//! Actor-based service manager.
//!
//! The [`manager_loop`] function runs as a single-owner Tokio task that receives
//! [`ManagerCommand`] messages through an `mpsc` channel. This serialises all
//! state mutations (start, stop, is_running) and prevents concurrent interleaving.
//!
//! Most of this module is `pub(crate)` — the public API surface is re-exported
//! from the crate root. Items that are `pub` only for the iOS lifecycle bridge
//! are marked `#[doc(hidden)]`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::error::ServiceError;
use crate::models::{PluginEvent, ServiceContext, StartConfig};
use crate::notifier::Notifier;
use crate::service_trait::BackgroundService;

/// Callback fired when the service task completes. Receives `true` on success.
#[doc(hidden)]
pub type OnCompleteCallback = Box<dyn Fn(bool) + Send + Sync>;

/// Abstraction over mobile keepalive operations.
///
/// Defined here (not behind `#[cfg(mobile)]`) so the actor can reference it
/// on all platforms. On desktop, `ServiceState.mobile` is `None` and these
/// methods are never called. On mobile, `MobileLifecycle` implements this trait.
pub(crate) trait MobileKeepalive: Send + Sync {
    /// Start the OS-specific keepalive (Android foreground service / iOS BGTask).
    fn start_keepalive(&self, label: &str, foreground_service_type: &str, ios_safety_timeout_secs: Option<f64>, ios_processing_safety_timeout_secs: Option<f64>) -> Result<(), ServiceError>;
    /// Stop the OS-specific keepalive.
    fn stop_keepalive(&self) -> Result<(), ServiceError>;
}

/// Type-erased factory: produces a fresh `Box<dyn BackgroundService<R>>` on demand.
#[doc(hidden)]
pub type ServiceFactory<R> =
    Box<dyn Fn() -> Box<dyn BackgroundService<R>> + Send + Sync>;

// ─── Commands ───────────────────────────────────────────────────────────

/// Commands sent to the service manager actor.
///
/// Internal implementation detail — not part of the public API.
///
/// This enum is `#[non_exhaustive]` to prevent external construction.
/// Use [`ServiceManagerHandle`] methods instead.
#[non_exhaustive]
pub enum ManagerCommand<R: Runtime> {
    Start {
        config: StartConfig,
        reply: oneshot::Sender<Result<(), ServiceError>>,
        app: AppHandle<R>,
    },
    Stop {
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    IsRunning {
        reply: oneshot::Sender<bool>,
    },
    SetOnComplete {
        callback: OnCompleteCallback,
    },
    #[allow(dead_code, private_interfaces)]
    SetMobile {
        mobile: Arc<dyn MobileKeepalive>,
    },
}

// ─── Handle ────────────────────────────────────────────────────────────

/// Handle to the service manager actor. Stored as Tauri managed state.
///
/// Tauri commands send messages through the internal channel; the actor
/// task processes them sequentially, preventing concurrent start/stop
/// interleaving.
pub struct ServiceManagerHandle<R: Runtime> {
    pub(crate) cmd_tx: mpsc::Sender<ManagerCommand<R>>,
}

impl<R: Runtime> ServiceManagerHandle<R> {
    /// Create a new handle backed by the given channel sender.
    pub fn new(cmd_tx: mpsc::Sender<ManagerCommand<R>>) -> Self {
        Self { cmd_tx }
    }

    /// Start a background service.
    ///
    /// Sends a `Start` command to the actor. Returns `AlreadyRunning` if a
    /// service is already active.
    pub async fn start(
        &self,
        app: AppHandle<R>,
        config: StartConfig,
    ) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::Start {
                config,
                reply,
                app,
            })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Stop the running background service.
    ///
    /// Sends a `Stop` command to the actor. Returns `NotRunning` if no
    /// service is active.
    pub async fn stop(&self) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::Stop { reply })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Check whether a background service is currently running.
    pub async fn is_running(&self) -> bool {
        let (reply, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(ManagerCommand::IsRunning { reply })
            .await
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Set the callback fired when the service task completes.
    ///
    /// The callback is captured at spawn time (generation-guarded), so calling
    /// this while a service is running will only affect the *next* start.
    #[doc(hidden)]
    pub async fn set_on_complete(&self, callback: OnCompleteCallback) {
        let _ = self
            .cmd_tx
            .send(ManagerCommand::SetOnComplete { callback })
            .await;
    }
}

// ─── Actor State ───────────────────────────────────────────────────────

/// Internal state owned exclusively by the actor task.
struct ServiceState<R: Runtime> {
    /// Cancellation token: `Some` means a service is running.
    /// Shared with the spawned service task via `Arc<Mutex<>>` so it can
    /// clear the slot when the task finishes.
    token: Arc<Mutex<Option<CancellationToken>>>,
    /// Generation counter for the race-condition guard.
    /// Incremented on each start; shared via `Arc<AtomicU64>`.
    generation: Arc<AtomicU64>,
    /// Callback fired once when the service task completes.
    /// Captured via `take()` at spawn time so a new callback can be set
    /// for the next start.
    on_complete: Option<OnCompleteCallback>,
    /// Factory that creates fresh service instances.
    factory: ServiceFactory<R>,
    /// Mobile keepalive handle. Set via `SetMobile` command on mobile platforms.
    mobile: Option<Arc<dyn MobileKeepalive>>,
    /// iOS safety timeout in seconds (from PluginConfig, default 28.0).
    /// Passed to mobile via `start_keepalive`. Android ignores this field.
    ios_safety_timeout_secs: f64,
    /// iOS BGProcessingTask safety timeout in seconds (from PluginConfig, default 0.0).
    /// When > 0.0, caps processing task duration. Passed as `Some(value)` to mobile.
    /// When 0.0, passed as `None` (no cap).
    ios_processing_safety_timeout_secs: f64,
}

// ─── Actor Loop ────────────────────────────────────────────────────────

/// Main actor loop: receives commands and dispatches to handlers.
///
/// Runs as a spawned Tokio task. The loop exits when all `Sender` halves
/// are dropped (i.e., the handle is dropped).
#[doc(hidden)]
pub async fn manager_loop<R: Runtime>(
    mut rx: mpsc::Receiver<ManagerCommand<R>>,
    factory: ServiceFactory<R>,
    // iOS safety timeout in seconds. From PluginConfig.
    // Default: 28.0 (Apple recommends keeping BG tasks under ~30s).
    // Passed to mobile via actor's `start_keepalive` call.
    ios_safety_timeout_secs: f64,
    // iOS BGProcessingTask safety timeout in seconds. From PluginConfig.
    // Default: 0.0 (no cap). When > 0.0, passed as Some(value) to mobile.
    ios_processing_safety_timeout_secs: f64,
) {
    let mut state = ServiceState {
        token: Arc::new(Mutex::new(None)),
        generation: Arc::new(AtomicU64::new(0)),
        on_complete: None,
        factory,
        mobile: None,
        ios_safety_timeout_secs,
        ios_processing_safety_timeout_secs,
    };

    while let Some(cmd) = rx.recv().await {
        match cmd {
            ManagerCommand::Start { config, reply, app } => {
                let _ = reply.send(handle_start(&mut state, app, config));
            }
            ManagerCommand::Stop { reply } => {
                let _ = reply.send(handle_stop(&mut state));
            }
            ManagerCommand::IsRunning { reply } => {
                let _ = reply.send(state.token.lock().unwrap().is_some());
            }
            ManagerCommand::SetOnComplete { callback } => {
                state.on_complete = Some(callback);
            }
            ManagerCommand::SetMobile { mobile } => {
                state.mobile = Some(mobile);
            }
        }
    }
}

// ─── Command Handlers ──────────────────────────────────────────────────

/// Handle a `Start` command.
///
/// Order of operations (critical for the race-condition fix):
/// 1. Check `AlreadyRunning` — reject early, no side-effects.
/// 2. Create token, increment generation.
/// 3. Start mobile keepalive (AFTER AlreadyRunning check).
///    On failure: rollback token and callback, return error.
/// 4. Spawn service task (init -> run -> cleanup).
fn handle_start<R: Runtime>(
    state: &mut ServiceState<R>,
    app: AppHandle<R>,
    config: StartConfig,
) -> Result<(), ServiceError> {
    let mut guard = state.token.lock().unwrap();

    if guard.is_some() {
        return Err(ServiceError::AlreadyRunning);
    }

    let token = CancellationToken::new();
    let shutdown = token.clone();
    *guard = Some(token);
    let my_gen = state.generation.fetch_add(1, Ordering::Release) + 1;

    drop(guard);

    // Capture on_complete at spawn time (generation-guarded).
    // Takes the callback out of the slot so a new start can set a fresh one.
    let captured_callback = state.on_complete.take();

    // Start mobile keepalive AFTER AlreadyRunning check.
    // On failure: rollback (clear token, restore callback).
    if let Some(ref mobile) = state.mobile {
        let processing_timeout = if state.ios_processing_safety_timeout_secs > 0.0 {
            Some(state.ios_processing_safety_timeout_secs)
        } else {
            None
        };
        if let Err(e) = mobile.start_keepalive(&config.service_label, &config.foreground_service_type, Some(state.ios_safety_timeout_secs), processing_timeout) {
            // Rollback: clear the token we just set.
            state.token.lock().unwrap().take();
            // Rollback: restore the callback we took.
            state.on_complete = captured_callback;
            return Err(e);
        }
    }

    // Shared refs for the spawned task's cleanup logic.
    let token_ref = state.token.clone();
    let gen_ref = state.generation.clone();

    let mut service = (state.factory)();

    let ctx = ServiceContext {
        notifier: Notifier { app: app.clone() },
        app: app.clone(),
        shutdown,
        service_label: Some(config.service_label),
        foreground_service_type: Some(config.foreground_service_type),
    };

    // Use tauri::async_runtime::spawn() instead of tokio::spawn() because
    // the plugin setup closure may run before a Tokio runtime context is
    // entered on the current thread (e.g. Android auto-start in setup).
    tauri::async_runtime::spawn(async move {
        // Phase 1: init
        if let Err(e) = service.init(&ctx).await {
            let _ = app.emit(
                "background-service://event",
                PluginEvent::Error {
                    message: e.to_string(),
                },
            );
            // Clear token only if generation hasn't advanced.
            if gen_ref.load(Ordering::Acquire) == my_gen {
                token_ref.lock().unwrap().take();
            }
            // Fire callback with false on init failure.
            if let Some(cb) = captured_callback {
                cb(false);
            }
            return;
        }

        // Emit Started
        let _ = app.emit("background-service://event", PluginEvent::Started);

        // Phase 2: run
        let result = service.run(&ctx).await;

        // Emit terminal event.
        match result {
            Ok(()) => {
                let _ = app.emit(
                    "background-service://event",
                    PluginEvent::Stopped {
                        reason: "completed".into(),
                    },
                );
            }
            Err(ref e) => {
                let _ = app.emit(
                    "background-service://event",
                    PluginEvent::Error {
                        message: e.to_string(),
                    },
                );
            }
        }

        // Fire on_complete callback (captured at spawn time).
        // MUST fire before clearing the token so that
        // `wait_until_stopped` only returns after the callback ran.
        if let Some(cb) = captured_callback {
            cb(result.is_ok());
        }

        // Clear token only if generation hasn't advanced.
        if gen_ref.load(Ordering::Acquire) == my_gen {
            token_ref.lock().unwrap().take();
        }
    });

    Ok(())
}

/// Handle a `Stop` command.
///
/// Takes the token from state and cancels it, then stops mobile keepalive.
/// Returns `NotRunning` if no service is active.
fn handle_stop<R: Runtime>(state: &mut ServiceState<R>) -> Result<(), ServiceError> {
    let mut guard = state.token.lock().unwrap();
    match guard.take() {
        Some(token) => {
            token.cancel();
            drop(guard);
            // Stop mobile keepalive after token cancellation.
            if let Some(ref mobile) = state.mobile {
                if let Err(e) = mobile.stop_keepalive() {
                    log::warn!("stop_keepalive failed (service already cancelled): {e}");
                }
            }
            Ok(())
        }
        None => Err(ServiceError::NotRunning),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicI8, AtomicU8, AtomicUsize};

    // ── Mock mobile for keepalive testing ─────────────────────────────

    /// Mock mobile that records start/stop_keepalive calls.
    struct MockMobile {
        start_called: AtomicUsize,
        stop_called: AtomicUsize,
        start_fail: bool,
        last_label: std::sync::Mutex<Option<String>>,
        last_fst: std::sync::Mutex<Option<String>>,
        last_timeout_secs: std::sync::Mutex<Option<f64>>,
        last_processing_timeout_secs: std::sync::Mutex<Option<f64>>,
    }

    impl MockMobile {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                start_called: AtomicUsize::new(0),
                stop_called: AtomicUsize::new(0),
                start_fail: false,
                last_label: std::sync::Mutex::new(None),
                last_fst: std::sync::Mutex::new(None),
                last_timeout_secs: std::sync::Mutex::new(None),
                last_processing_timeout_secs: std::sync::Mutex::new(None),
            })
        }

        fn new_failing() -> Arc<Self> {
            Arc::new(Self {
                start_called: AtomicUsize::new(0),
                stop_called: AtomicUsize::new(0),
                start_fail: true,
                last_label: std::sync::Mutex::new(None),
                last_fst: std::sync::Mutex::new(None),
                last_timeout_secs: std::sync::Mutex::new(None),
                last_processing_timeout_secs: std::sync::Mutex::new(None),
            })
        }
    }

    impl MobileKeepalive for MockMobile {
        fn start_keepalive(&self, label: &str, foreground_service_type: &str, ios_safety_timeout_secs: Option<f64>, ios_processing_safety_timeout_secs: Option<f64>) -> Result<(), ServiceError> {
            self.start_called.fetch_add(1, Ordering::Release);
            *self.last_label.lock().unwrap() = Some(label.to_string());
            *self.last_fst.lock().unwrap() = Some(foreground_service_type.to_string());
            *self.last_timeout_secs.lock().unwrap() = ios_safety_timeout_secs;
            *self.last_processing_timeout_secs.lock().unwrap() = ios_processing_safety_timeout_secs;
            if self.start_fail {
                return Err(ServiceError::Platform("mock keepalive failure".into()));
            }
            Ok(())
        }

        fn stop_keepalive(&self) -> Result<(), ServiceError> {
            self.stop_called.fetch_add(1, Ordering::Release);
            Ok(())
        }
    }

    /// Service that blocks in run() until cancelled.
    /// Used for lifecycle tests where is_running must remain true.
    struct BlockingService;

    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for BlockingService {
        async fn init(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        async fn run(
            &mut self,
            ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            ctx.shutdown.cancelled().await;
            Ok(())
        }
    }

    /// Create a manager actor with a BlockingService factory.
    fn setup_manager() -> ServiceManagerHandle<tauri::test::MockRuntime> {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        tokio::spawn(manager_loop(cmd_rx, factory, 28.0, 0.0));
        handle
    }

    async fn send_start(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        app: AppHandle<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::Start {
                config: StartConfig::default(),
                reply: tx,
                app,
            })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    async fn send_stop(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::Stop { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    async fn send_is_running(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    ) -> bool {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::IsRunning { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    // ── AC1: Start from idle succeeds ────────────────────────────────

    #[tokio::test]
    async fn start_from_idle() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        let result = send_start(&handle, app.handle().clone()).await;
        assert!(result.is_ok(), "start should succeed from idle");
        assert!(
            send_is_running(&handle).await,
            "should be running after start"
        );
    }

    // ── AC2: Stop from running succeeds ──────────────────────────────

    #[tokio::test]
    async fn stop_from_running() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();

        let result = send_stop(&handle).await;
        assert!(result.is_ok(), "stop should succeed from running");
        assert!(
            !send_is_running(&handle).await,
            "should not be running after stop"
        );
    }

    // ── AC3: Double start returns AlreadyRunning ────────────────────

    #[tokio::test]
    async fn double_start_returns_already_running() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();

        let result = send_start(&handle, app.handle().clone()).await;
        assert!(
            matches!(result, Err(ServiceError::AlreadyRunning)),
            "second start should return AlreadyRunning"
        );
    }

    // ── AC4: Stop when not running returns NotRunning ────────────────

    #[tokio::test]
    async fn stop_when_not_running_returns_not_running() {
        let handle = setup_manager();

        let result = send_stop(&handle).await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "stop should return NotRunning when idle"
        );
    }

    // ── AC5: Start-stop-restart cycle works ──────────────────────────

    #[tokio::test]
    async fn start_stop_restart_cycle() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        // Start
        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        // Stop
        send_stop(&handle).await.unwrap();
        assert!(!send_is_running(&handle).await);

        // Restart
        let result = send_start(&handle, app.handle().clone()).await;
        assert!(result.is_ok(), "restart should succeed after stop");
        assert!(
            send_is_running(&handle).await,
            "should be running after restart"
        );
    }

    // ── Test services for callback testing ────────────────────────────

    /// Service that completes run() immediately with success.
    struct ImmediateSuccessService;

    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for ImmediateSuccessService {
        async fn init(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        async fn run(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// Service whose run() returns an error immediately.
    struct ImmediateErrorService;

    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for ImmediateErrorService {
        async fn init(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        async fn run(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Err(ServiceError::Runtime("run error".into()))
        }
    }

    /// Service whose init() fails.
    struct FailingInitService;

    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for FailingInitService {
        async fn init(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Err(ServiceError::Init("init error".into()))
        }

        async fn run(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// Create a manager actor with a custom factory.
    fn setup_manager_with_factory(
        factory: ServiceFactory<tauri::test::MockRuntime>,
    ) -> ServiceManagerHandle<tauri::test::MockRuntime> {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        tokio::spawn(manager_loop(cmd_rx, factory, 28.0, 0.0));
        handle
    }

    async fn send_set_on_complete(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        callback: OnCompleteCallback,
    ) {
        handle
            .cmd_tx
            .send(ManagerCommand::SetOnComplete { callback })
            .await
            .unwrap();
    }

    /// Wait for the service to finish (is_running becomes false).
    /// Polls with a short sleep between attempts.
    async fn wait_until_stopped(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        timeout_ms: u64,
    ) {
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < timeout_ms as u128 {
            if !send_is_running(handle).await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("Service did not stop within {timeout_ms}ms");
    }

    // ── AC6 (Step 3): Callback fires on success ──────────────────────

    #[tokio::test]
    async fn callback_fires_on_success() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
        let app = tauri::test::mock_app();

        let called = Arc::new(AtomicI8::new(-1));
        let called_clone = called.clone();
        send_set_on_complete(
            &handle,
            Box::new(move |success| {
                called_clone.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;

        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_stopped(&handle, 1000).await;

        assert_eq!(
            called.load(Ordering::Acquire),
            1,
            "callback should be called with true"
        );
    }

    // ── AC7 (Step 3): Callback fires on error ────────────────────────

    #[tokio::test]
    async fn callback_fires_on_error() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(ImmediateErrorService)));
        let app = tauri::test::mock_app();

        let called = Arc::new(AtomicI8::new(-1));
        let called_clone = called.clone();
        send_set_on_complete(
            &handle,
            Box::new(move |success| {
                called_clone.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;

        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_stopped(&handle, 1000).await;

        assert_eq!(
            called.load(Ordering::Acquire),
            0,
            "callback should be called with false on error"
        );
    }

    // ── AC8 (Step 3): Callback fires on init failure ─────────────────

    #[tokio::test]
    async fn callback_fires_on_init_failure() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(FailingInitService)));
        let app = tauri::test::mock_app();

        let called = Arc::new(AtomicI8::new(-1));
        let called_clone = called.clone();
        send_set_on_complete(
            &handle,
            Box::new(move |success| {
                called_clone.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;

        send_start(&handle, app.handle().clone()).await.unwrap();

        // Init failure: service was never truly running, so token gets cleared quickly.
        // Wait a short time for the spawned task to complete.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            called.load(Ordering::Acquire),
            0,
            "callback should be called with false on init failure"
        );
        assert!(
            !send_is_running(&handle).await,
            "should not be running after init failure"
        );
    }

    // ── AC9 (Step 3): No callback no panic ───────────────────────────

    #[tokio::test]
    async fn no_callback_no_panic() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
        let app = tauri::test::mock_app();

        // Deliberately do NOT call SetOnComplete.
        let result = send_start(&handle, app.handle().clone()).await;
        assert!(result.is_ok(), "start without callback should succeed");

        wait_until_stopped(&handle, 1000).await;
        // If we get here without panicking, the test passes.
    }

    // ── AC10 (Step 3): Generation guard prevents stale cleanup ───────

    #[tokio::test]
    async fn generation_guard_prevents_stale_cleanup() {
        // First start with FailingInit (generation 1) — clears its own token.
        // Second start with ImmediateSuccess (generation 2) — should succeed
        // because the old task's cleanup shouldn't corrupt the new state.
        let call_count = Arc::new(AtomicU8::new(0));
        let call_count_clone = call_count.clone();

        let handle = setup_manager_with_factory(Box::new(move || {
            let cc = call_count_clone.clone();
            // First call: FailingInit. Second call: ImmediateSuccess.
            // Use AtomicU8 to track which invocation this is.
            if cc.fetch_add(1, Ordering::AcqRel) == 0 {
                Box::new(FailingInitService) as Box<dyn BackgroundService<tauri::test::MockRuntime>>
            } else {
                Box::new(ImmediateSuccessService)
            }
        }));
        let app = tauri::test::mock_app();

        // First start: init fails, token cleared by spawned task.
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Second start: should succeed — generation guard prevented stale cleanup.
        let result = send_start(&handle, app.handle().clone()).await;
        assert!(
            result.is_ok(),
            "second start should succeed after init failure: {result:?}"
        );
        assert!(
            send_is_running(&handle).await,
            "should be running after second start"
        );
    }

    // ── AC11 (Step 3): Callback captured at spawn time ───────────────

    #[tokio::test]
    async fn callback_captured_at_spawn_time() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(BlockingService)));
        let app = tauri::test::mock_app();

        // Set callback A, start, then set callback B.
        // When the service completes, A should fire (not B).
        let which = Arc::new(AtomicU8::new(0)); // 0=none, 1=A, 2=B
        let which_clone_a = which.clone();
        let which_clone_b = which.clone();

        send_set_on_complete(
            &handle,
            Box::new(move |_| {
                which_clone_a.store(1, Ordering::Release);
            }),
        )
        .await;

        send_start(&handle, app.handle().clone()).await.unwrap();

        // Service is blocking — set a NEW callback while it runs.
        send_set_on_complete(
            &handle,
            Box::new(move |_| {
                which_clone_b.store(2, Ordering::Release);
            }),
        )
        .await;

        // Stop the service — this triggers cleanup and callback.
        send_stop(&handle).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            which.load(Ordering::Acquire),
            1,
            "callback A should fire, not B"
        );
    }

    // ── Mobile keepalive helpers ──────────────────────────────────────

    async fn send_set_mobile(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        mobile: Arc<dyn MobileKeepalive>,
    ) {
        handle
            .cmd_tx
            .send(ManagerCommand::SetMobile { mobile })
            .await
            .unwrap();
    }

    // ── AC1 (Step 5): start_keepalive called on start ────────────────

    #[tokio::test]
    async fn start_keepalive_called_on_start() {
        let mock = MockMobile::new();
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        assert_eq!(
            mock.start_called.load(Ordering::Acquire),
            1,
            "start_keepalive should be called once"
        );
        assert_eq!(
            mock.last_label.lock().unwrap().as_deref(),
            Some("Service running"),
            "label should be forwarded"
        );
    }

    // ── AC2 (Step 5): start_keepalive failure rollback ───────────────

    #[tokio::test]
    async fn start_keepalive_failure_rollback() {
        let mock = MockMobile::new_failing();
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        let callback_called = Arc::new(AtomicI8::new(-1));
        let cb_clone = callback_called.clone();
        send_set_on_complete(
            &handle,
            Box::new(move |success| {
                cb_clone.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;

        send_set_mobile(&handle, mock.clone()).await;

        let result = send_start(&handle, app.handle().clone()).await;
        assert!(
            matches!(result, Err(ServiceError::Platform(_))),
            "start should return Platform error on keepalive failure: {result:?}"
        );

        // Token should be cleared (not running).
        assert!(
            !send_is_running(&handle).await,
            "token should be rolled back after keepalive failure"
        );

        // Callback should be restored — can be set again.
        let callback_called2 = Arc::new(AtomicI8::new(-1));
        let cb_clone2 = callback_called2.clone();
        send_set_on_complete(
            &handle,
            Box::new(move |success| {
                cb_clone2.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;

        // Without the failing mobile, a start should succeed and callback should work.
        // Use a fresh manager without mobile to test callback restoration.
        let handle2 = setup_manager_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
        let callback_restored = Arc::new(AtomicI8::new(-1));
        let cb_r = callback_restored.clone();
        send_set_on_complete(
            &handle2,
            Box::new(move |success| {
                cb_r.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;
        send_start(&handle2, app.handle().clone()).await.unwrap();
        wait_until_stopped(&handle2, 1000).await;
        assert_eq!(
            callback_restored.load(Ordering::Acquire),
            1,
            "callback should fire after successful start (proves rollback restored it)"
        );
    }

    // ── AC3 (Step 5): stop_keepalive called on stop ──────────────────

    #[tokio::test]
    async fn stop_keepalive_called_on_stop() {
        let mock = MockMobile::new();
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            0,
            "stop_keepalive should not be called yet"
        );

        send_stop(&handle).await.unwrap();

        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            1,
            "stop_keepalive should be called once after stop"
        );
    }

    // ── stop_keepalive failure does not propagate ──────────────────────────

    /// Mock mobile where `stop_keepalive` always fails.
    struct MockMobileFailingStop;

    impl MobileKeepalive for MockMobileFailingStop {
        fn start_keepalive(&self, _label: &str, _foreground_service_type: &str, _ios_safety_timeout_secs: Option<f64>, _ios_processing_safety_timeout_secs: Option<f64>) -> Result<(), ServiceError> {
            Ok(())
        }

        fn stop_keepalive(&self) -> Result<(), ServiceError> {
            Err(ServiceError::Platform("mock stop failure".into()))
        }
    }

    #[tokio::test]
    async fn stop_keepalive_failure_does_not_propagate() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, Arc::new(MockMobileFailingStop)).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        let result = send_stop(&handle).await;
        assert!(result.is_ok(), "stop should succeed even when stop_keepalive fails");

        assert!(
            !send_is_running(&handle).await,
            "service should not be running after stop"
        );
    }

    // ── iOS safety timeout passed to mobile ──────────────────────────────

    #[tokio::test]
    async fn ios_safety_timeout_passed_to_mobile() {
        let mock = MockMobile::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        // Use a custom timeout value (not default 28.0)
        tokio::spawn(manager_loop(cmd_rx, factory, 15.0, 0.0));

        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        // Verify the timeout was passed through to the mock
        let timeout = *mock.last_timeout_secs.lock().unwrap();
        assert_eq!(
            timeout,
            Some(15.0),
            "ios_safety_timeout_secs should be passed to mobile"
        );
    }

    // ── iOS processing timeout passed to mobile ──────────────────────────────

    #[tokio::test]
    async fn ios_processing_timeout_passed_to_mobile() {
        let mock = MockMobile::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        // Use a custom processing timeout value
        tokio::spawn(manager_loop(cmd_rx, factory, 28.0, 60.0));

        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        // Verify the processing timeout was passed through to the mock
        let timeout = *mock.last_processing_timeout_secs.lock().unwrap();
        assert_eq!(
            timeout,
            Some(60.0),
            "ios_processing_safety_timeout_secs should be passed to mobile"
        );
    }

    #[tokio::test]
    async fn ios_processing_timeout_zero_passes_as_none() {
        let mock = MockMobile::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        // Processing timeout = 0.0 (default, no cap)
        tokio::spawn(manager_loop(cmd_rx, factory, 28.0, 0.0));

        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        // Zero timeout should be passed as None
        let timeout = *mock.last_processing_timeout_secs.lock().unwrap();
        assert_eq!(
            timeout,
            None,
            "ios_processing_safety_timeout_secs of 0.0 should pass None to mobile"
        );
    }
}
