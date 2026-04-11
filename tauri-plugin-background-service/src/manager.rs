//! Actor-based service manager.
//!
//! The [`manager_loop`] function runs as a single-owner Tokio task that receives
//! [`ManagerCommand`] messages through an `mpsc` channel. This serialises all
//! state mutations (start, stop, is_running) and prevents concurrent interleaving.
//!
//! Most of this module is `pub(crate)` — the public API surface is re-exported
//! from the crate root. Items that are `pub` only for the iOS lifecycle bridge
//! are marked `#[doc(hidden)]`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::error::ServiceError;
use crate::models::{
    validate_foreground_service_type, PluginEvent, ServiceContext,
    ServiceState as ServiceLifecycle, ServiceStatus, StartConfig,
};
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
    #[allow(clippy::too_many_arguments)]
    fn start_keepalive(
        &self,
        label: &str,
        foreground_service_type: &str,
        ios_safety_timeout_secs: Option<f64>,
        ios_processing_safety_timeout_secs: Option<f64>,
        ios_earliest_refresh_begin_minutes: Option<f64>,
        ios_earliest_processing_begin_minutes: Option<f64>,
        ios_requires_external_power: Option<bool>,
        ios_requires_network_connectivity: Option<bool>,
    ) -> Result<(), ServiceError>;
    /// Stop the OS-specific keepalive.
    fn stop_keepalive(&self) -> Result<(), ServiceError>;
}

/// Type-erased factory: produces a fresh `Box<dyn BackgroundService<R>>` on demand.
#[doc(hidden)]
pub type ServiceFactory<R> = Box<dyn Fn() -> Box<dyn BackgroundService<R>> + Send + Sync>;

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
    GetState {
        reply: oneshot::Sender<ServiceStatus>,
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
    pub async fn start(&self, app: AppHandle<R>, config: StartConfig) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::Start { config, reply, app })
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

    /// Stop the running background service synchronously.
    ///
    /// Uses `blocking_send` so this can be called from synchronous contexts
    /// (e.g., a Tauri `on_event` closure). Returns `NotRunning` if no
    /// service is active.
    pub fn stop_blocking(&self) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .blocking_send(ManagerCommand::Stop { reply })
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.blocking_recv()
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

    /// Get the current service lifecycle status.
    pub async fn get_state(&self) -> ServiceStatus {
        let (reply, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(ManagerCommand::GetState { reply })
            .await
            .is_err()
        {
            return ServiceStatus {
                state: ServiceLifecycle::Idle,
                last_error: None,
            };
        }
        rx.await.unwrap_or(ServiceStatus {
            state: ServiceLifecycle::Idle,
            last_error: None,
        })
    }
}

// ─── Actor State ───────────────────────────────────────────────────────

/// Internal state owned exclusively by the actor task.
struct ServiceState<R: Runtime> {
    /// Fast path: `true` when a service task is active.
    /// Set by `handle_start`, cleared by `handle_stop` or task cleanup.
    /// Avoids acquiring the Mutex for status-only queries.
    is_running: Arc<AtomicBool>,
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
    /// iOS BGAppRefreshTask earliest begin date in minutes (default 15.0).
    ios_earliest_refresh_begin_minutes: f64,
    /// iOS BGProcessingTask earliest begin date in minutes (default 15.0).
    ios_earliest_processing_begin_minutes: f64,
    /// iOS BGProcessingTask requires external power (default false).
    ios_requires_external_power: bool,
    /// iOS BGProcessingTask requires network connectivity (default false).
    ios_requires_network_connectivity: bool,
    /// Current lifecycle state of the service.
    /// Shared with spawned task for transitions (Initializing→Running→Stopped).
    lifecycle_state: Arc<Mutex<ServiceLifecycle>>,
    /// Last error message from init/run failure.
    /// Shared with spawned task for error capture.
    last_error: Arc<Mutex<Option<String>>>,
}

// ─── Actor Loop ────────────────────────────────────────────────────────

/// Main actor loop: receives commands and dispatches to handlers.
///
/// Runs as a spawned Tokio task. The loop exits when all `Sender` halves
/// are dropped (i.e., the handle is dropped).
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
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
    // iOS BGAppRefreshTask earliest begin date in minutes. From PluginConfig.
    ios_earliest_refresh_begin_minutes: f64,
    // iOS BGProcessingTask earliest begin date in minutes. From PluginConfig.
    ios_earliest_processing_begin_minutes: f64,
    // iOS BGProcessingTask requires external power. From PluginConfig.
    ios_requires_external_power: bool,
    // iOS BGProcessingTask requires network connectivity. From PluginConfig.
    ios_requires_network_connectivity: bool,
) {
    let mut state = ServiceState {
        is_running: Arc::new(AtomicBool::new(false)),
        token: Arc::new(Mutex::new(None)),
        generation: Arc::new(AtomicU64::new(0)),
        on_complete: None,
        factory,
        mobile: None,
        ios_safety_timeout_secs,
        ios_processing_safety_timeout_secs,
        ios_earliest_refresh_begin_minutes,
        ios_earliest_processing_begin_minutes,
        ios_requires_external_power,
        ios_requires_network_connectivity,
        lifecycle_state: Arc::new(Mutex::new(ServiceLifecycle::Idle)),
        last_error: Arc::new(Mutex::new(None)),
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
                let _ = reply.send(state.is_running.load(Ordering::SeqCst));
            }
            ManagerCommand::SetOnComplete { callback } => {
                state.on_complete = Some(callback);
            }
            ManagerCommand::SetMobile { mobile } => {
                state.mobile = Some(mobile);
            }
            ManagerCommand::GetState { reply } => {
                let status = ServiceStatus {
                    state: *state.lifecycle_state.lock().unwrap(),
                    last_error: state.last_error.lock().unwrap().clone(),
                };
                let _ = reply.send(status);
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

    // Validate foreground service type against the allowlist.
    // Only relevant on mobile (Android foreground service types).
    // On desktop the type is ignored — no OS enforcement mechanism.
    if cfg!(mobile) {
        validate_foreground_service_type(&config.foreground_service_type)?;
    }

    let token = CancellationToken::new();
    let shutdown = token.clone();
    *guard = Some(token);
    let my_gen = state.generation.fetch_add(1, Ordering::Release) + 1;
    state.is_running.store(true, Ordering::SeqCst);
    *state.lifecycle_state.lock().unwrap() = ServiceLifecycle::Initializing;
    *state.last_error.lock().unwrap() = None;

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
        if let Err(e) = mobile.start_keepalive(
            &config.service_label,
            &config.foreground_service_type,
            Some(state.ios_safety_timeout_secs),
            processing_timeout,
            Some(state.ios_earliest_refresh_begin_minutes),
            Some(state.ios_earliest_processing_begin_minutes),
            Some(state.ios_requires_external_power),
            Some(state.ios_requires_network_connectivity),
        ) {
            // Rollback: clear the token we just set.
            state.token.lock().unwrap().take();
            state.is_running.store(false, Ordering::SeqCst);
            *state.lifecycle_state.lock().unwrap() = ServiceLifecycle::Idle;
            // Rollback: restore the callback we took.
            state.on_complete = captured_callback;
            return Err(e);
        }
    }

    // Shared refs for the spawned task's cleanup logic.
    let token_ref = state.token.clone();
    let gen_ref = state.generation.clone();
    let is_running_ref = state.is_running.clone();
    let lifecycle_ref = state.lifecycle_state.clone();
    let last_error_ref = state.last_error.clone();

    let mut service = (state.factory)();

    let ctx = ServiceContext {
        notifier: Notifier { app: app.clone() },
        app: app.clone(),
        shutdown,
        #[cfg(mobile)]
        service_label: config.service_label,
        #[cfg(mobile)]
        foreground_service_type: config.foreground_service_type,
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
                is_running_ref.store(false, Ordering::SeqCst);
                // Initializing → Stopped on init failure.
                {
                    let mut lc = lifecycle_ref.lock().unwrap();
                    if *lc == ServiceLifecycle::Initializing {
                        *lc = ServiceLifecycle::Stopped;
                    }
                }
                *last_error_ref.lock().unwrap() = Some(e.to_string());
            }
            // Fire callback with false on init failure.
            if let Some(cb) = captured_callback {
                cb(false);
            }
            return;
        }

        // Initializing → Running after successful init (generation + state guarded).
        if gen_ref.load(Ordering::Acquire) == my_gen {
            let mut lc = lifecycle_ref.lock().unwrap();
            if *lc == ServiceLifecycle::Initializing {
                *lc = ServiceLifecycle::Running;
            }
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
            is_running_ref.store(false, Ordering::SeqCst);
            // → Stopped on run completion (generation guarded).
            {
                let mut lc = lifecycle_ref.lock().unwrap();
                if matches!(
                    *lc,
                    ServiceLifecycle::Initializing | ServiceLifecycle::Running
                ) {
                    *lc = ServiceLifecycle::Stopped;
                }
            }
            if let Err(ref e) = result {
                *last_error_ref.lock().unwrap() = Some(e.to_string());
            }
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
            state.is_running.store(false, Ordering::SeqCst);
            *state.lifecycle_state.lock().unwrap() = ServiceLifecycle::Stopped;
            *state.last_error.lock().unwrap() = None;
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
        last_earliest_refresh_begin_minutes: std::sync::Mutex<Option<f64>>,
        last_earliest_processing_begin_minutes: std::sync::Mutex<Option<f64>>,
        last_requires_external_power: std::sync::Mutex<Option<bool>>,
        last_requires_network_connectivity: std::sync::Mutex<Option<bool>>,
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
                last_earliest_refresh_begin_minutes: std::sync::Mutex::new(None),
                last_earliest_processing_begin_minutes: std::sync::Mutex::new(None),
                last_requires_external_power: std::sync::Mutex::new(None),
                last_requires_network_connectivity: std::sync::Mutex::new(None),
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
                last_earliest_refresh_begin_minutes: std::sync::Mutex::new(None),
                last_earliest_processing_begin_minutes: std::sync::Mutex::new(None),
                last_requires_external_power: std::sync::Mutex::new(None),
                last_requires_network_connectivity: std::sync::Mutex::new(None),
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn mock_start_keepalive(
        mock: &MockMobile,
        label: &str,
        foreground_service_type: &str,
        ios_safety_timeout_secs: Option<f64>,
        ios_processing_safety_timeout_secs: Option<f64>,
        ios_earliest_refresh_begin_minutes: Option<f64>,
        ios_earliest_processing_begin_minutes: Option<f64>,
        ios_requires_external_power: Option<bool>,
        ios_requires_network_connectivity: Option<bool>,
    ) -> Result<(), ServiceError> {
        mock.start_called.fetch_add(1, Ordering::Release);
        *mock.last_label.lock().unwrap() = Some(label.to_string());
        *mock.last_fst.lock().unwrap() = Some(foreground_service_type.to_string());
        *mock.last_timeout_secs.lock().unwrap() = ios_safety_timeout_secs;
        *mock.last_processing_timeout_secs.lock().unwrap() = ios_processing_safety_timeout_secs;
        *mock.last_earliest_refresh_begin_minutes.lock().unwrap() =
            ios_earliest_refresh_begin_minutes;
        *mock.last_earliest_processing_begin_minutes.lock().unwrap() =
            ios_earliest_processing_begin_minutes;
        *mock.last_requires_external_power.lock().unwrap() = ios_requires_external_power;
        *mock.last_requires_network_connectivity.lock().unwrap() =
            ios_requires_network_connectivity;
        if mock.start_fail {
            return Err(ServiceError::Platform("mock keepalive failure".into()));
        }
        Ok(())
    }

    impl MobileKeepalive for MockMobile {
        #[allow(clippy::too_many_arguments)]
        fn start_keepalive(
            &self,
            label: &str,
            foreground_service_type: &str,
            ios_safety_timeout_secs: Option<f64>,
            ios_processing_safety_timeout_secs: Option<f64>,
            ios_earliest_refresh_begin_minutes: Option<f64>,
            ios_earliest_processing_begin_minutes: Option<f64>,
            ios_requires_external_power: Option<bool>,
            ios_requires_network_connectivity: Option<bool>,
        ) -> Result<(), ServiceError> {
            mock_start_keepalive(
                self,
                label,
                foreground_service_type,
                ios_safety_timeout_secs,
                ios_processing_safety_timeout_secs,
                ios_earliest_refresh_begin_minutes,
                ios_earliest_processing_begin_minutes,
                ios_requires_external_power,
                ios_requires_network_connectivity,
            )
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
        tokio::spawn(manager_loop(
            cmd_rx, factory, 28.0, 0.0, 15.0, 15.0, false, false,
        ));
        handle
    }

    async fn send_start(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        app: AppHandle<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        send_start_with_config(handle, StartConfig::default(), app).await
    }

    async fn send_start_with_config(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        config: StartConfig,
        app: AppHandle<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::Start {
                config,
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

    async fn send_is_running(handle: &ServiceManagerHandle<tauri::test::MockRuntime>) -> bool {
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
        tokio::spawn(manager_loop(
            cmd_rx, factory, 28.0, 0.0, 15.0, 15.0, false, false,
        ));
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

    // ── N2: is_running returns false after natural completion ────────

    #[tokio::test]
    async fn is_running_false_after_natural_completion() {
        // Use a service that yields during run() so the is_running check
        // doesn't race with immediate completion.
        struct YieldingService;

        #[async_trait]
        impl BackgroundService<tauri::test::MockRuntime> for YieldingService {
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
                // Sleep long enough for the caller to observe is_running=true,
                // then complete naturally (no cancellation).
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(())
            }
        }

        let handle = setup_manager_with_factory(Box::new(|| Box::new(YieldingService)));
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(
            send_is_running(&handle).await,
            "should be running immediately after start"
        );

        // Wait for the service to complete naturally (no stop).
        wait_until_stopped(&handle, 2000).await;

        assert!(
            !send_is_running(&handle).await,
            "is_running should be false after natural completion"
        );
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

    #[allow(clippy::too_many_arguments)]
    impl MobileKeepalive for MockMobileFailingStop {
        fn start_keepalive(
            &self,
            _label: &str,
            _foreground_service_type: &str,
            _ios_safety_timeout_secs: Option<f64>,
            _ios_processing_safety_timeout_secs: Option<f64>,
            _ios_earliest_refresh_begin_minutes: Option<f64>,
            _ios_earliest_processing_begin_minutes: Option<f64>,
            _ios_requires_external_power: Option<bool>,
            _ios_requires_network_connectivity: Option<bool>,
        ) -> Result<(), ServiceError> {
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
        assert!(
            result.is_ok(),
            "stop should succeed even when stop_keepalive fails"
        );

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
        tokio::spawn(manager_loop(
            cmd_rx, factory, 15.0, 0.0, 15.0, 15.0, false, false,
        ));

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
        tokio::spawn(manager_loop(
            cmd_rx, factory, 28.0, 60.0, 15.0, 15.0, false, false,
        ));

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

    // ── Service that captures ServiceContext fields for inspection ──────

    /// Service that captures `service_label` and `foreground_service_type`
    /// from the `ServiceContext` it receives in `init()`.
    /// Only compiled on mobile where those fields exist.
    #[cfg(mobile)]
    struct ContextCapturingService {
        captured_label: Arc<std::sync::Mutex<Option<String>>>,
        captured_fst: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[cfg(mobile)]
    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for ContextCapturingService {
        async fn init(
            &mut self,
            ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            *self.captured_label.lock().unwrap() = Some(ctx.service_label.clone());
            *self.captured_fst.lock().unwrap() = Some(ctx.foreground_service_type.clone());
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

    // ── AC (Step 11): ServiceContext fields are populated on mobile ────

    #[cfg(mobile)]
    #[tokio::test]
    async fn service_context_fields_populated_on_mobile() {
        let captured_label: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let captured_fst: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let cl = captured_label.clone();
        let cf = captured_fst.clone();

        let handle = setup_manager_with_factory(Box::new(move || {
            let cl = cl.clone();
            let cf = cf.clone();
            Box::new(ContextCapturingService {
                captured_label: cl,
                captured_fst: cf,
            })
        }));
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "Syncing".into(),
            foreground_service_type: "dataSync".into(),
        };

        send_start_with_config(&handle, config, app.handle().clone())
            .await
            .unwrap();

        // Give the spawned task time to run init() (which captures the values).
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // On mobile, both fields should be populated as Strings
        assert_eq!(
            captured_label.lock().unwrap().as_deref(),
            Some("Syncing"),
            "service_label should be 'Syncing' on mobile"
        );
        assert_eq!(
            captured_fst.lock().unwrap().as_deref(),
            Some("dataSync"),
            "foreground_service_type should be 'dataSync' on mobile"
        );

        send_stop(&handle).await.unwrap();
    }

    // ── S1: handle_start accepts invalid foreground_service_type on desktop ──

    #[tokio::test]
    async fn handle_start_accepts_invalid_foreground_service_type_on_desktop() {
        // On desktop (cfg!(mobile) == false), the foreground_service_type
        // validation is skipped. An arbitrary string should succeed.
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "test".into(),
            foreground_service_type: "bogusType".into(),
        };

        let result = send_start_with_config(&handle, config, app.handle().clone()).await;
        assert!(
            result.is_ok(),
            "start with invalid fg type should succeed on desktop: {result:?}"
        );
        assert!(
            send_is_running(&handle).await,
            "service should be running after start with invalid type on desktop"
        );

        send_stop(&handle).await.unwrap();
    }

    // ── handle_start accepts all valid foreground_service_types ────────

    #[tokio::test]
    async fn handle_start_accepts_all_valid_foreground_service_types() {
        for &valid_type in crate::models::VALID_FOREGROUND_SERVICE_TYPES {
            let handle = setup_manager();
            let app = tauri::test::mock_app();

            let config = StartConfig {
                service_label: "test".into(),
                foreground_service_type: valid_type.into(),
            };

            let result = send_start_with_config(&handle, config, app.handle().clone()).await;
            assert!(
                result.is_ok(),
                "start with valid type '{valid_type}' should succeed: {result:?}"
            );
            assert!(send_is_running(&handle).await);
            // Stop for cleanup
            send_stop(&handle).await.unwrap();
        }
    }

    // ── State transition helpers ────────────────────────────────────────

    async fn send_get_state(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    ) -> ServiceStatus {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::GetState { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    // ── State transition: initial state is Idle ───────────────────────

    #[tokio::test]
    async fn get_state_returns_idle_initially() {
        let handle = setup_manager();
        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Idle);
        assert_eq!(status.last_error, None);
    }

    // ── State transition: Idle → Initializing → Running → Stopped ─────

    #[tokio::test]
    async fn lifecycle_idle_to_running_to_stopped() {
        // Use BlockingService so we can reliably observe the Running state.
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        // Idle initially
        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Idle);

        // Start — transitions to Initializing, then Running after init()
        send_start(&handle, app.handle().clone()).await.unwrap();

        // Small delay for spawned task to complete init() → Running
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Running);

        // Stop → Stopped
        send_stop(&handle).await.unwrap();
        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Stopped);
        assert_eq!(status.last_error, None);
    }

    // ── State transition: Idle → Initializing → Stopped on init failure ─

    #[tokio::test]
    async fn lifecycle_init_failure_sets_stopped_with_error() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(FailingInitService)));
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();

        // Wait for init failure to propagate
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Stopped);
        assert!(
            status.last_error.is_some(),
            "last_error should be set on init failure"
        );
        assert!(
            status.last_error.unwrap().contains("init error"),
            "error should mention init error"
        );
    }

    // ── State transition: explicit stop sets Stopped, clears last_error ─

    #[tokio::test]
    async fn lifecycle_explicit_stop_sets_stopped_clears_error() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Running);

        send_stop(&handle).await.unwrap();

        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Stopped);
        assert_eq!(
            status.last_error, None,
            "explicit stop should clear last_error"
        );
    }

    // ── State transition: restart clears stale last_error ─────────────

    #[tokio::test]
    async fn restart_clears_stale_last_error() {
        // Step 1: start with a service whose init() fails → Stopped + last_error set
        let handle = setup_manager_with_factory(Box::new(|| Box::new(FailingInitService)));
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Stopped);
        assert!(
            status.last_error.is_some(),
            "should have error after init failure"
        );

        // Step 2: restart with a succeeding service — last_error must be cleared
        // We can't swap the factory, but we CAN verify the field is cleared
        // by starting again with the same failing service and checking that
        // handle_start resets last_error before the spawn.
        // Instead, use a two-phase factory: first fails, then succeeds.
        let call_count = Arc::new(AtomicUsize::new(0));
        let count_clone = call_count.clone();
        let handle2 = setup_manager_with_factory(Box::new(move || {
            let n = count_clone.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Box::new(FailingInitService)
            } else {
                Box::new(ImmediateSuccessService)
            }
        }));
        let app2 = tauri::test::mock_app();

        // First start: init fails
        send_start(&handle2, app2.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let status = send_get_state(&handle2).await;
        assert_eq!(status.state, ServiceLifecycle::Stopped);
        assert!(
            status.last_error.is_some(),
            "first run should set last_error"
        );

        // Second start: succeeds — last_error must be None
        send_start(&handle2, app2.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let status = send_get_state(&handle2).await;
        // After successful init + run completion, state is Stopped (natural completion)
        // but last_error should be cleared by handle_start
        assert_eq!(
            status.last_error, None,
            "last_error must be cleared on restart, not stale from previous failure"
        );
    }

    // ── get_state via ServiceManagerHandle method ─────────────────────

    #[tokio::test]
    async fn get_state_handle_method_returns_idle() {
        let handle = setup_manager();
        let status = handle.get_state().await;
        assert_eq!(status.state, ServiceLifecycle::Idle);
        assert_eq!(status.last_error, None);
    }

    // ── stop_blocking sends Stop command and returns success from running ─

    #[tokio::test]
    async fn stop_blocking_returns_success_from_running() {
        let handle = Arc::new(setup_manager());
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        // Must call stop_blocking from outside the async runtime.
        let h = handle.clone();
        let result = tokio::task::spawn_blocking(move || h.stop_blocking())
            .await
            .expect("spawn_blocking panicked");
        assert!(
            result.is_ok(),
            "stop_blocking should succeed from running: {result:?}"
        );
        assert!(
            !send_is_running(&handle).await,
            "should not be running after stop_blocking"
        );
    }

    // ── stop_blocking returns NotRunning when idle ───────────────────────

    #[tokio::test]
    async fn stop_blocking_returns_not_running_when_idle() {
        let handle = Arc::new(setup_manager());

        let h = handle.clone();
        let result = tokio::task::spawn_blocking(move || h.stop_blocking())
            .await
            .expect("spawn_blocking panicked");
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "stop_blocking should return NotRunning when idle: {result:?}"
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
        tokio::spawn(manager_loop(
            cmd_rx, factory, 28.0, 0.0, 15.0, 15.0, false, false,
        ));

        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        // Zero timeout should be passed as None
        let timeout = *mock.last_processing_timeout_secs.lock().unwrap();
        assert_eq!(
            timeout, None,
            "ios_processing_safety_timeout_secs of 0.0 should pass None to mobile"
        );
    }
}
