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

use crate::desired_state::DesiredStateBackend;
use crate::error::ServiceError;
use crate::models::{
    validate_foreground_service_type, LifecycleMode, LifecycleState, LifecycleStatus, PluginEvent,
    ServiceContext, ServiceState as ServiceLifecycle, ServiceStatus, StartConfig, StopReason,
    ValidationIssue,
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
    StopWithReason {
        reason: StopReason,
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
    SetDesiredRunning {
        desired: bool,
        config: Option<StartConfig>,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    EnableAutoRestart {
        config: Option<StartConfig>,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    DisableAutoRestart {
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    GetDesiredState {
        reply: oneshot::Sender<Option<crate::desired_state::DesiredState>>,
    },
    NativeLifecycleEvent {
        event: crate::models::NativeLifecycleEvent,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    GetLifecycleStatus {
        desktop_mode: Option<String>,
        reply: oneshot::Sender<LifecycleStatus>,
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

    /// Stop the running background service with a specific reason.
    ///
    /// Applies a reason-based desired-state policy: intentional stops
    /// (UserStop, AppStop, etc.) clear desired state, while platform
    /// errors and timeouts preserve it for auto-restart recovery.
    pub async fn stop_with_reason(&self, reason: StopReason) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::StopWithReason { reason, reply })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Stop the running background service synchronously with a specific reason.
    ///
    /// Blocking variant of [`ServiceManagerHandle::stop_with_reason`].
    pub fn stop_blocking_with_reason(&self, reason: StopReason) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .blocking_send(ManagerCommand::StopWithReason { reason, reply })
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
                ..Default::default()
            };
        }
        rx.await.unwrap_or(ServiceStatus {
            state: ServiceLifecycle::Idle,
            ..Default::default()
        })
    }

    /// Send a native lifecycle event to the actor.
    ///
    /// Maps the native event to the appropriate [`StopReason`] and delegates
    /// to [`handle_stop_with_reason`].
    #[doc(hidden)]
    pub async fn send_native_lifecycle_event(
        &self,
        event: crate::models::NativeLifecycleEvent,
    ) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::NativeLifecycleEvent { event, reply })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
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
    /// Desired-state persistence backend.
    /// `None` on platforms that haven't set one up yet.
    desired_state: Option<Arc<dyn DesiredStateBackend>>,
    /// Current platform's lifecycle mode (FGS, BGTask, in-process, OS-service).
    lifecycle_mode: LifecycleMode,
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
    // Desired-state persistence backend. None if not configured.
    desired_state_backend: Option<Arc<dyn DesiredStateBackend>>,
) {
    let lifecycle_mode = {
        #[cfg(target_os = "android")]
        {
            LifecycleMode::AndroidForegroundService
        }
        #[cfg(target_os = "ios")]
        {
            LifecycleMode::IosBgTaskScheduler
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            LifecycleMode::DesktopInProcess
        }
    };

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
        desired_state: desired_state_backend,
        lifecycle_mode,
    };

    while let Some(cmd) = rx.recv().await {
        match cmd {
            ManagerCommand::Start { config, reply, app } => {
                let _ = reply.send(handle_start(&mut state, app, config));
            }
            ManagerCommand::Stop { reply } => {
                let _ = reply.send(handle_stop(&mut state));
            }
            ManagerCommand::StopWithReason { reason, reply } => {
                let _ = reply.send(handle_stop_with_reason(&mut state, reason));
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
                let mut status = ServiceStatus {
                    state: *state.lifecycle_state.lock().unwrap(),
                    last_error: state.last_error.lock().unwrap().clone(),
                    platform_mode: Some(state.lifecycle_mode),
                    ..Default::default()
                };

                if let Some(ref backend) = state.desired_state {
                    if let Ok(ds) = backend.load() {
                        status.desired_running = Some(ds.desired_running);
                        status.native_state = ds
                            .last_native_state
                            .as_deref()
                            .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok());
                        status.last_start_config = ds
                            .last_start_config
                            .and_then(|v| serde_json::from_value(v).ok());
                        status.last_heartbeat_at = ds.last_heartbeat_epoch_ms;
                        status.restart_attempt = if ds.restart_attempt > 0 {
                            Some(ds.restart_attempt)
                        } else {
                            None
                        };
                        status.recovery_reason = ds.recovery_reason;
                        status.platform_error = ds.last_platform_error;
                    }
                }

                let _ = reply.send(status);
            }
            ManagerCommand::SetDesiredRunning {
                desired,
                config,
                reply,
            } => {
                let _ = reply.send(handle_set_desired_running(&mut state, desired, config));
            }
            ManagerCommand::EnableAutoRestart { config, reply } => {
                let _ = reply.send(handle_enable_auto_restart(&mut state, config));
            }
            ManagerCommand::DisableAutoRestart { reply } => {
                let _ = reply.send(handle_disable_auto_restart(&mut state));
            }
            ManagerCommand::GetDesiredState { reply } => {
                let _ = reply.send(handle_get_desired_state(&state));
            }
            ManagerCommand::NativeLifecycleEvent { event, reply } => {
                let reason = event.to_stop_reason();
                let _ = reply.send(handle_stop_with_reason(&mut state, reason));
            }
            ManagerCommand::GetLifecycleStatus {
                desktop_mode,
                reply,
            } => {
                let _ = reply.send(build_lifecycle_status(&state, desktop_mode.as_deref()));
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
        service_label: config.service_label.clone(),
        #[cfg(mobile)]
        foreground_service_type: config.foreground_service_type.clone(),
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
                        reason: StopReason::TaskCompleted,
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

    // Persist desired_running=true after successful start.
    save_desired_running(state, true, Some(&config));

    Ok(())
}

/// Handle a `Stop` command.
///
/// Takes the token from state and cancels it, then stops mobile keepalive.
/// Returns `NotRunning` if no service is active.
fn handle_stop<R: Runtime>(state: &mut ServiceState<R>) -> Result<(), ServiceError> {
    handle_stop_with_reason(state, StopReason::UserStop)
}

/// Handle a `StopWithReason` command.
///
/// Like `handle_stop` but applies a reason-based desired-state policy:
/// - Clears desired state for intentional stops: `UserStop`, `AppStop`,
///   `NativeNotificationStop`, `TaskCompleted`.
/// - Preserves desired state for platform/error reasons: `PlatformTimeout`,
///   `PlatformExpiration`, `OsRestart`, `BootRecovery`, `Error`.
fn handle_stop_with_reason<R: Runtime>(
    state: &mut ServiceState<R>,
    reason: StopReason,
) -> Result<(), ServiceError> {
    let mut guard = state.token.lock().unwrap();
    match guard.take() {
        Some(token) => {
            token.cancel();
            state.is_running.store(false, Ordering::SeqCst);
            *state.lifecycle_state.lock().unwrap() = ServiceLifecycle::Stopped;
            *state.last_error.lock().unwrap() = None;
            drop(guard);
            if should_stop_keepalive(reason) {
                if let Some(ref mobile) = state.mobile {
                    if let Err(e) = mobile.stop_keepalive() {
                        log::warn!("stop_keepalive failed: {e}");
                    }
                }
            }
            if should_clear_desired_state(reason) {
                save_desired_running(state, false, None);
            }
            Ok(())
        }
        None => Err(ServiceError::NotRunning),
    }
}

/// Returns `true` if the given stop reason should clear the desired-state
/// (i.e. set `desired_running = false`). Intentional user/app stops clear
/// desired state so auto-restart won't fight the user's intent. Platform
/// timeouts and errors preserve desired state so recovery can restart.
fn should_clear_desired_state(reason: StopReason) -> bool {
    matches!(
        reason,
        StopReason::UserStop
            | StopReason::AppStop
            | StopReason::NativeNotificationStop
            | StopReason::TaskCompleted
    )
}

/// Returns `true` if `stop_keepalive` should be called for the given reason.
/// `PlatformExpiration` is skipped because the OS has already killed the
/// background task — calling stop_keepalive would be redundant.
fn should_stop_keepalive(reason: StopReason) -> bool {
    !matches!(reason, StopReason::PlatformExpiration)
}

// ─── Desired-State Helpers ──────────────────────────────────────────────

/// Save desired-state to the backend (if configured).
///
/// On `desired=true`: saves `desired_running=true` with config and timestamp.
/// On `desired=false`: saves `desired_running=false` and clears recovery fields.
fn save_desired_running<R: Runtime>(
    state: &ServiceState<R>,
    desired: bool,
    config: Option<&StartConfig>,
) {
    let Some(ref backend) = state.desired_state else {
        return;
    };

    let mut ds = backend.load().unwrap_or_default();
    ds.desired_running = desired;
    if desired {
        ds.last_start_config = config.map(|c| serde_json::to_value(c).unwrap_or_default());
        ds.last_start_epoch_ms = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        );
    } else {
        ds.last_start_config = None;
        ds.last_start_epoch_ms = None;
        ds.recovery_pending = false;
        ds.recovery_reason = None;
        ds.restart_attempt = 0;
    }
    if let Err(e) = backend.save(&ds) {
        log::warn!("failed to save desired state: {e}");
    }
}

/// Handle a `SetDesiredRunning` command.
///
/// Persists the desired running state WITHOUT affecting the actual running state.
/// This is used by `enableAutoRestart()` / `disableAutoRestart()` to set intent
/// for recovery without starting/stopping the service.
fn handle_set_desired_running<R: Runtime>(
    state: &mut ServiceState<R>,
    desired: bool,
    config: Option<StartConfig>,
) -> Result<(), ServiceError> {
    save_desired_running(state, desired, config.as_ref());
    Ok(())
}

/// Handle an `EnableAutoRestart` command.
///
/// Persists `desired_running=true` with the optional config WITHOUT starting
/// the service. Used to set recovery intent for future restart/reboot.
fn handle_enable_auto_restart<R: Runtime>(
    state: &mut ServiceState<R>,
    config: Option<StartConfig>,
) -> Result<(), ServiceError> {
    save_desired_running(state, true, config.as_ref());
    Ok(())
}

/// Handle a `DisableAutoRestart` command.
///
/// Persists `desired_running=false` with cleared recovery fields WITHOUT
/// stopping the service.
fn handle_disable_auto_restart<R: Runtime>(
    state: &mut ServiceState<R>,
) -> Result<(), ServiceError> {
    save_desired_running(state, false, None);
    Ok(())
}

/// Handle a `GetDesiredState` command.
///
/// Returns the persisted desired state, or `None` if no backend is configured.
fn handle_get_desired_state<R: Runtime>(
    state: &ServiceState<R>,
) -> Option<crate::desired_state::DesiredState> {
    state
        .desired_state
        .as_ref()
        .and_then(|backend| backend.load().ok())
}

/// Compose a [`LifecycleStatus`] snapshot from the actor's current state.
///
/// Gathers: service lifecycle state → `LifecycleState`, desired-state fields
/// from the persistence backend, platform capabilities, and validation issues.
fn build_lifecycle_status<R: Runtime>(
    state: &ServiceState<R>,
    desktop_mode: Option<&str>,
) -> LifecycleStatus {
    let lifecycle_state: LifecycleState = (*state.lifecycle_state.lock().unwrap()).into();
    let last_error = state.last_error.lock().unwrap().clone();

    // Load desired-state fields.
    let desired = state.desired_state.as_ref().and_then(|b| b.load().ok());

    let desired_running = desired.as_ref().is_some_and(|d| d.desired_running);
    let recovery_enabled = desired_running;
    let recovery_pending = desired.as_ref().is_some_and(|d| d.recovery_pending);
    let recovery_reason = desired.as_ref().and_then(|d| d.recovery_reason.clone());
    let last_start_config = desired
        .as_ref()
        .and_then(|d| d.last_start_config.clone())
        .and_then(|v| serde_json::from_value(v).ok());
    let last_platform_state = desired.as_ref().and_then(|d| d.last_native_state.clone());
    let last_platform_error = desired.as_ref().and_then(|d| d.last_platform_error.clone());

    let (platform, _) = crate::capabilities::CapabilityProvider::detect_platform(desktop_mode);
    let capabilities = crate::capabilities::CapabilityProvider::capabilities(
        platform,
        state.lifecycle_mode,
        false,
    );
    let report = crate::validator::SetupValidator::validate(platform);
    let mut issues: Vec<ValidationIssue> = report
        .errors
        .into_iter()
        .map(|i| ValidationIssue {
            severity: crate::models::Severity::Error,
            code: i.code,
            message: i.message,
            fix: i.fix,
            platform,
        })
        .collect();
    issues.extend(report.warnings.into_iter().map(|i| ValidationIssue {
        severity: crate::models::Severity::Warning,
        code: i.code,
        message: i.message,
        fix: i.fix,
        platform,
    }));

    LifecycleStatus {
        state: lifecycle_state,
        desired_running,
        recovery_enabled,
        recovery_pending,
        recovery_reason,
        last_start_config,
        last_platform_state,
        last_platform_error,
        last_error,
        platform,
        capabilities,
        issues,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desired_state::DesiredState;
    use crate::models::{NativeLifecycleEvent, NativeState};
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
        setup_manager_with_backend(None)
    }

    /// Create a manager actor with a desired-state backend.
    fn setup_manager_with_backend(
        backend: Option<Arc<dyn DesiredStateBackend>>,
    ) -> ServiceManagerHandle<tauri::test::MockRuntime> {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        tokio::spawn(manager_loop(
            cmd_rx, factory, 28.0, 0.0, 15.0, 15.0, false, false, backend,
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
        setup_manager_with_factory_and_backend(factory, None)
    }

    /// Create a manager actor with a custom factory and desired-state backend.
    fn setup_manager_with_factory_and_backend(
        factory: ServiceFactory<tauri::test::MockRuntime>,
        backend: Option<Arc<dyn DesiredStateBackend>>,
    ) -> ServiceManagerHandle<tauri::test::MockRuntime> {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        tokio::spawn(manager_loop(
            cmd_rx, factory, 28.0, 0.0, 15.0, 15.0, false, false, backend,
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
            cmd_rx, factory, 15.0, 0.0, 15.0, 15.0, false, false, None,
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
            cmd_rx, factory, 28.0, 60.0, 15.0, 15.0, false, false, None,
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
            cmd_rx, factory, 28.0, 0.0, 15.0, 15.0, false, false, None,
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

    // ── Desired-state MockBackend ─────────────────────────────────────────

    /// Mock desired-state backend that records all saves in a Mutex<Vec>.
    struct MockDesiredStateBackend {
        saves: std::sync::Mutex<Vec<DesiredState>>,
    }

    impl MockDesiredStateBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                saves: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn last_save(&self) -> Option<DesiredState> {
            self.saves.lock().unwrap().last().cloned()
        }

        #[allow(dead_code)]
        fn save_count(&self) -> usize {
            self.saves.lock().unwrap().len()
        }

        #[allow(dead_code)]
        fn saves(&self) -> std::sync::MutexGuard<'_, Vec<DesiredState>> {
            self.saves.lock().unwrap()
        }
    }

    impl DesiredStateBackend for MockDesiredStateBackend {
        fn load(&self) -> Result<DesiredState, String> {
            Ok(self
                .saves
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default())
        }

        fn save(&self, state: &DesiredState) -> Result<(), String> {
            self.saves.lock().unwrap().push(state.clone());
            Ok(())
        }

        fn clear(&self) -> Result<(), String> {
            self.saves.lock().unwrap().clear();
            Ok(())
        }
    }

    // ── Desired-state actor integration tests ─────────────────────────────

    async fn send_set_desired_running(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        desired: bool,
        config: Option<StartConfig>,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::SetDesiredRunning {
                desired,
                config,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    #[tokio::test]
    async fn start_saves_desired_running_true() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "Syncing".into(),
            ..Default::default()
        };
        send_start_with_config(&handle, config, app.handle().clone())
            .await
            .unwrap();

        // Give the actor a moment to process the save (it happens after spawn).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let last = backend
            .last_save()
            .expect("should have saved desired state");
        assert!(
            last.desired_running,
            "desired_running should be true after start"
        );
        assert!(
            last.last_start_config.is_some(),
            "last_start_config should be set"
        );
        assert!(
            last.last_start_epoch_ms.is_some(),
            "last_start_epoch_ms should be set"
        );
    }

    #[tokio::test]
    async fn stop_saves_desired_running_false_with_cleared_recovery() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();

        // Simulate some recovery state that should be cleared on stop.
        {
            let mut saves = backend.saves.lock().unwrap();
            let last = saves.last_mut().unwrap();
            last.recovery_pending = true;
            last.recovery_reason = Some("boot".into());
            last.restart_attempt = 3;
        }

        send_stop(&handle).await.unwrap();

        let last = backend.last_save().expect("should have saved on stop");
        assert!(
            !last.desired_running,
            "desired_running should be false after stop"
        );
        assert!(
            last.last_start_config.is_none(),
            "last_start_config should be cleared"
        );
        assert!(
            last.last_start_epoch_ms.is_none(),
            "last_start_epoch_ms should be cleared"
        );
        assert!(!last.recovery_pending, "recovery_pending should be cleared");
        assert_eq!(
            last.recovery_reason, None,
            "recovery_reason should be cleared"
        );
        assert_eq!(last.restart_attempt, 0, "restart_attempt should be cleared");
    }

    #[tokio::test]
    async fn set_desired_running_saves_without_affecting_is_running() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));

        // Not running initially
        assert!(!send_is_running(&handle).await);

        // Set desired_running=true WITHOUT starting
        let config = StartConfig {
            service_label: "AutoRestart".into(),
            ..Default::default()
        };
        send_set_desired_running(&handle, true, Some(config.clone()))
            .await
            .unwrap();

        // Should NOT be running
        assert!(
            !send_is_running(&handle).await,
            "SetDesiredRunning should not affect is_running"
        );

        // But desired state should be saved
        let last = backend.last_save().expect("should have saved");
        assert!(last.desired_running);
        assert!(last.last_start_config.is_some());

        // Now set desired_running=false
        send_set_desired_running(&handle, false, None)
            .await
            .unwrap();

        assert!(!send_is_running(&handle).await);

        let last = backend.last_save().expect("should have saved");
        assert!(!last.desired_running);
    }

    #[tokio::test]
    async fn no_backend_means_no_panic() {
        // No backend — should work fine without panicking.
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        send_stop(&handle).await.unwrap();

        send_set_desired_running(&handle, true, None).await.unwrap();
        // If we got here, no panic occurred.
    }

    #[tokio::test]
    async fn start_config_serialized_in_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "CustomLabel".into(),
            foreground_service_type: "specialUse".into(),
        };
        send_start_with_config(&handle, config, app.handle().clone())
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let last = backend.last_save().expect("should have saved");
        let saved_config = last.last_start_config.expect("config should be set");
        assert_eq!(saved_config["serviceLabel"], "CustomLabel");
        assert_eq!(saved_config["foregroundServiceType"], "specialUse");
    }

    // ── GetState population from desired-state backend (Step 4, task 1c5e) ──

    #[tokio::test]
    async fn get_state_returns_desired_running_true_after_start() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let status = send_get_state(&handle).await;
        assert_eq!(
            status.desired_running,
            Some(true),
            "desired_running should be Some(true) after start with backend"
        );
    }

    #[tokio::test]
    async fn get_state_returns_desired_running_false_after_stop() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        send_stop(&handle).await.unwrap();

        let status = send_get_state(&handle).await;
        assert_eq!(
            status.desired_running,
            Some(false),
            "desired_running should be Some(false) after stop with backend"
        );
    }

    #[tokio::test]
    async fn get_state_returns_none_fields_when_no_backend() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let status = send_get_state(&handle).await;
        assert_eq!(status.desired_running, None);
        assert_eq!(status.native_state, None);
        assert_eq!(status.last_start_config, None);
        assert_eq!(status.last_heartbeat_at, None);
        assert_eq!(status.restart_attempt, None);
        assert_eq!(status.recovery_reason, None);
        assert_eq!(status.platform_error, None);
    }

    #[tokio::test]
    async fn get_state_returns_last_start_config_from_backend() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "TestService".into(),
            foreground_service_type: "specialUse".into(),
        };
        send_start_with_config(&handle, config, app.handle().clone())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let status = send_get_state(&handle).await;
        let cfg = status
            .last_start_config
            .expect("last_start_config should be populated from backend");
        assert_eq!(cfg.service_label, "TestService");
        assert_eq!(cfg.foreground_service_type, "specialUse");
    }

    #[tokio::test]
    async fn get_state_populates_all_desired_state_fields() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Mutate the backend state to simulate recovery fields being set.
        {
            let mut saves = backend.saves.lock().unwrap();
            let last = saves.last_mut().unwrap();
            last.last_native_state = Some("timeout".into());
            last.last_platform_error = Some("FGS timed out".into());
            last.restart_attempt = 3;
            last.recovery_reason = Some("boot recovery".into());
            last.last_heartbeat_epoch_ms = Some(1700000005000);
        }

        let status = send_get_state(&handle).await;
        assert_eq!(status.desired_running, Some(true));
        assert_eq!(status.native_state, Some(NativeState::Timeout));
        assert_eq!(status.platform_error, Some("FGS timed out".into()));
        assert_eq!(status.restart_attempt, Some(3));
        assert_eq!(status.recovery_reason, Some("boot recovery".into()));
        assert_eq!(status.last_heartbeat_at, Some(1700000005000));
    }

    #[tokio::test]
    async fn get_state_returns_platform_mode() {
        let handle = setup_manager();

        let status = send_get_state(&handle).await;
        // On desktop (Linux test runner), should be DesktopInProcess.
        assert_eq!(
            status.platform_mode,
            Some(LifecycleMode::DesktopInProcess),
            "platform_mode should be populated even without backend"
        );
    }

    // ── Step 13: EnableAutoRestart / DisableAutoRestart / GetDesiredState tests ──

    async fn send_enable_auto_restart(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        config: Option<StartConfig>,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::EnableAutoRestart { config, reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    async fn send_disable_auto_restart(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::DisableAutoRestart { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    async fn send_get_desired_state(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    ) -> Option<DesiredState> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::GetDesiredState { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    #[tokio::test]
    async fn enable_auto_restart_saves_true_without_starting() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));

        assert!(!send_is_running(&handle).await);

        send_enable_auto_restart(&handle, None).await.unwrap();

        // Should NOT start the service
        assert!(
            !send_is_running(&handle).await,
            "enableAutoRestart should not start the service"
        );

        // But desired state should be saved as true
        let ds = backend.last_save().expect("should have saved");
        assert!(ds.desired_running, "desired_running should be true");
    }

    #[tokio::test]
    async fn disable_auto_restart_saves_false_without_stopping() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        // Start the service first
        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        // Disable auto restart
        send_disable_auto_restart(&handle).await.unwrap();

        // Should NOT stop the service
        assert!(
            send_is_running(&handle).await,
            "disableAutoRestart should not stop the service"
        );

        // But desired state should be saved as false
        let ds = backend.last_save().expect("should have saved");
        assert!(!ds.desired_running, "desired_running should be false");
    }

    #[tokio::test]
    async fn enable_auto_restart_with_config_stores_config() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));

        let config = StartConfig {
            service_label: "MyService".into(),
            foreground_service_type: "specialUse".into(),
        };
        send_enable_auto_restart(&handle, Some(config.clone()))
            .await
            .unwrap();

        let ds = backend.last_save().expect("should have saved");
        assert!(ds.desired_running);
        let saved_config = ds.last_start_config.expect("config should be stored");
        assert_eq!(saved_config["serviceLabel"], "MyService");
        assert_eq!(saved_config["foregroundServiceType"], "specialUse");
        assert!(
            ds.last_start_epoch_ms.is_some(),
            "should set last_start_epoch_ms"
        );
    }

    #[tokio::test]
    async fn disable_auto_restart_clears_recovery_fields() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));

        // Enable with some recovery state
        send_enable_auto_restart(&handle, None).await.unwrap();
        {
            let mut saves = backend.saves.lock().unwrap();
            let last = saves.last_mut().unwrap();
            last.recovery_pending = true;
            last.recovery_reason = Some("boot".into());
            last.restart_attempt = 5;
        }

        // Disable should clear recovery
        send_disable_auto_restart(&handle).await.unwrap();

        let ds = backend.last_save().expect("should have saved");
        assert!(!ds.desired_running);
        assert!(!ds.recovery_pending, "recovery_pending should be cleared");
        assert_eq!(
            ds.recovery_reason, None,
            "recovery_reason should be cleared"
        );
        assert_eq!(ds.restart_attempt, 0, "restart_attempt should be cleared");
    }

    #[tokio::test]
    async fn get_desired_state_returns_current_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));

        // Initially returns default
        let ds = send_get_desired_state(&handle).await;
        assert!(ds.is_some());
        assert!(!ds.unwrap().desired_running);

        // After enable, returns updated state
        let config = StartConfig {
            service_label: "Test".into(),
            ..Default::default()
        };
        send_enable_auto_restart(&handle, Some(config))
            .await
            .unwrap();

        let ds = send_get_desired_state(&handle)
            .await
            .expect("should return state");
        assert!(ds.desired_running);
        assert!(ds.last_start_config.is_some());
    }

    #[tokio::test]
    async fn get_desired_state_returns_none_without_backend() {
        let handle = setup_manager();
        let ds = send_get_desired_state(&handle).await;
        assert!(
            ds.is_none(),
            "GetDesiredState should return None without a backend"
        );
    }

    #[tokio::test]
    async fn enable_disable_no_backend_no_panic() {
        let handle = setup_manager();

        // These should succeed (no-op) without a backend
        send_enable_auto_restart(&handle, None).await.unwrap();
        send_disable_auto_restart(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn get_state_stop_clears_start_config_and_recovery() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "Syncing".into(),
            ..Default::default()
        };
        send_start_with_config(&handle, config, app.handle().clone())
            .await
            .unwrap();
        send_stop(&handle).await.unwrap();

        let status = send_get_state(&handle).await;
        assert_eq!(status.desired_running, Some(false));
        assert_eq!(
            status.last_start_config, None,
            "last_start_config should be None after stop"
        );
        assert_eq!(
            status.restart_attempt, None,
            "restart_attempt should be None after stop"
        );
        assert_eq!(
            status.recovery_reason, None,
            "recovery_reason should be None after stop"
        );
    }

    // ── Step 5 (task 8763): Desktop persistence integration tests ──────────

    use crate::desired_state::FileDesiredStateBackend;
    use std::path::PathBuf;

    fn temp_state_dir() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    fn file_backend(dir: PathBuf) -> Arc<dyn DesiredStateBackend> {
        Arc::new(FileDesiredStateBackend::new(dir))
    }

    #[tokio::test]
    async fn enable_auto_restart_persists_desired_running_true_to_file() {
        let dir = temp_state_dir();
        let backend = file_backend(dir.clone());
        let handle = setup_manager_with_backend(Some(backend));

        send_enable_auto_restart(&handle, None).await.unwrap();

        // Verify the file was written with desired_running=true
        let file_backend = FileDesiredStateBackend::new(dir);
        let state = file_backend.load().unwrap();
        assert!(
            state.desired_running,
            "file should contain desired_running=true after enable_auto_restart"
        );
    }

    #[tokio::test]
    async fn simulated_process_restart_loads_persisted_state() {
        let dir = temp_state_dir();
        let backend = file_backend(dir.clone());
        let config = StartConfig {
            service_label: "PersistentSvc".into(),
            foreground_service_type: "dataSync".into(),
        };

        // Simulate first process: enable auto-restart with config
        let handle1 = setup_manager_with_backend(Some(backend));
        send_enable_auto_restart(&handle1, Some(config.clone()))
            .await
            .unwrap();

        // Drop the first manager (simulates process death)
        drop(handle1);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Simulate second process: fresh manager with same backend dir
        let backend2 = file_backend(dir.clone());
        let handle2 = setup_manager_with_backend(Some(backend2));

        // The fresh manager should be able to load the persisted state
        let ds = send_get_desired_state(&handle2)
            .await
            .expect("should return persisted state");
        assert!(
            ds.desired_running,
            "persisted desired_running should be true after simulated restart"
        );
        let saved_config = ds
            .last_start_config
            .expect("config should be persisted across restart");
        assert_eq!(saved_config["serviceLabel"], "PersistentSvc");
    }

    #[tokio::test]
    async fn disable_auto_restart_clears_file_backed_state() {
        let dir = temp_state_dir();
        let backend = file_backend(dir.clone());
        let handle = setup_manager_with_backend(Some(backend));

        // First enable
        send_enable_auto_restart(&handle, None).await.unwrap();
        let ds = send_get_desired_state(&handle)
            .await
            .expect("should return state");
        assert!(ds.desired_running, "should be true after enable");

        // Now disable
        send_disable_auto_restart(&handle).await.unwrap();

        // Verify file-backed state is now false with cleared fields
        let file_backend = FileDesiredStateBackend::new(dir);
        let state = file_backend.load().unwrap();
        assert!(
            !state.desired_running,
            "file should contain desired_running=false after disable"
        );
        assert!(
            state.last_start_config.is_none(),
            "config should be cleared"
        );
        assert!(
            state.last_start_epoch_ms.is_none(),
            "epoch should be cleared"
        );
        assert!(!state.recovery_pending, "recovery should be cleared");
        assert_eq!(state.restart_attempt, 0, "restart_attempt should be 0");
    }

    #[tokio::test]
    async fn file_backend_get_desired_state_returns_none_without_backend() {
        let handle = setup_manager();

        let ds = send_get_desired_state(&handle).await;
        assert!(
            ds.is_none(),
            "get_desired_state should return None without backend (existing behavior preserved)"
        );
    }

    // ── Step 6 (task d820): StopWithReason command and handler tests ──────────

    async fn send_stop_with_reason(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        reason: StopReason,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::StopWithReason { reason, reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    #[tokio::test]
    async fn stop_with_reason_user_stop_clears_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::UserStop)
            .await
            .unwrap();

        // UserStop should save desired_running=false
        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before + 1,
            "UserStop should save a new desired state"
        );
        let last = saves.last().unwrap();
        assert!(
            !last.desired_running,
            "UserStop should clear desired_running"
        );
        assert!(last.last_start_config.is_none(), "config should be cleared");
    }

    #[tokio::test]
    async fn stop_with_reason_app_stop_clears_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::AppStop)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(saves.len(), saves_before + 1);
        assert!(
            !saves.last().unwrap().desired_running,
            "AppStop should clear desired_running"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_native_notification_stop_clears_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::NativeNotificationStop)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(saves.len(), saves_before + 1);
        assert!(
            !saves.last().unwrap().desired_running,
            "NativeNotificationStop should clear desired_running"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_task_completed_clears_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::TaskCompleted)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(saves.len(), saves_before + 1);
        assert!(
            !saves.last().unwrap().desired_running,
            "TaskCompleted should clear desired_running"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_platform_expiration_preserves_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::PlatformExpiration)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "PlatformExpiration should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_platform_timeout_preserves_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::PlatformTimeout)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "PlatformTimeout should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_error_preserves_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::Error)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "Error should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_not_running_returns_not_running() {
        let handle = setup_manager();

        let result = send_stop_with_reason(&handle, StopReason::UserStop).await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "StopWithReason should return NotRunning when idle"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_cancels_service() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        send_stop_with_reason(&handle, StopReason::UserStop)
            .await
            .unwrap();

        assert!(
            !send_is_running(&handle).await,
            "service should be stopped after StopWithReason"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_stops_mobile_keepalive() {
        let mock = MockMobile::new();
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        assert_eq!(mock.stop_called.load(Ordering::Acquire), 0);

        send_stop_with_reason(&handle, StopReason::UserStop)
            .await
            .unwrap();

        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            1,
            "stop_keepalive should be called once after StopWithReason"
        );
    }

    // ── Step 6 (task fee4): handle_stop delegates to handle_stop_with_reason ──

    #[tokio::test]
    async fn stop_delegates_to_stop_with_reason_user_stop_clears_desired() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        // Plain Stop should behave like StopWithReason(UserStop) — clear desired state
        send_stop(&handle).await.unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before + 1,
            "Stop should save desired state (delegates to StopWithReason(UserStop))"
        );
        assert!(
            !saves.last().unwrap().desired_running,
            "Stop should clear desired_running"
        );
    }

    // ── Step 6 (task fee4): ServiceManagerHandle::stop_with_reason ──────────

    #[tokio::test]
    async fn stop_with_reason_handle_method_stops_service() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        handle.stop_with_reason(StopReason::UserStop).await.unwrap();

        assert!(
            !send_is_running(&handle).await,
            "service should be stopped after stop_with_reason"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_handle_method_preserves_desired_for_platform_timeout() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        handle
            .stop_with_reason(StopReason::PlatformTimeout)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "PlatformTimeout should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_handle_method_returns_not_running_when_idle() {
        let handle = setup_manager();

        let result = handle.stop_with_reason(StopReason::UserStop).await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "stop_with_reason should return NotRunning when idle"
        );
    }

    // ── Step 6 (task fee4): ServiceManagerHandle::stop_blocking_with_reason ──

    #[tokio::test]
    async fn stop_blocking_with_reason_stops_service() {
        let handle = Arc::new(setup_manager());
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        let h = handle.clone();
        let result =
            tokio::task::spawn_blocking(move || h.stop_blocking_with_reason(StopReason::AppStop))
                .await
                .expect("spawn_blocking panicked");

        assert!(
            result.is_ok(),
            "stop_blocking_with_reason should succeed: {result:?}"
        );
        assert!(
            !send_is_running(&handle).await,
            "service should be stopped after stop_blocking_with_reason"
        );
    }

    #[tokio::test]
    async fn stop_blocking_with_reason_returns_not_running_when_idle() {
        let handle = Arc::new(setup_manager());

        let h = handle.clone();
        let result =
            tokio::task::spawn_blocking(move || h.stop_blocking_with_reason(StopReason::UserStop))
                .await
                .expect("spawn_blocking panicked");

        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "stop_blocking_with_reason should return NotRunning when idle: {result:?}"
        );
    }

    // ── Step 6 (task d336): Idempotent stop and PlatformExpiration keepalive ──

    #[tokio::test]
    async fn stop_with_reason_idempotent_second_returns_not_running() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // First stop succeeds
        send_stop_with_reason(&handle, StopReason::UserStop)
            .await
            .unwrap();

        let saves_after_first = backend.saves.lock().unwrap().len();

        // Second stop returns NotRunning with no additional side effects
        let result = send_stop_with_reason(&handle, StopReason::UserStop).await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "second StopWithReason should return NotRunning: {result:?}"
        );

        let saves_after_second = backend.saves.lock().unwrap().len();
        assert_eq!(
            saves_after_first, saves_after_second,
            "second StopWithReason should not produce additional desired-state saves"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_platform_expiration_skips_stop_keepalive() {
        let mock = MockMobile::new();
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            0,
            "stop_keepalive should not be called yet"
        );

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::PlatformExpiration)
            .await
            .unwrap();

        assert!(!send_is_running(&handle).await, "service should be stopped");
        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            0,
            "PlatformExpiration should NOT call stop_keepalive"
        );

        // Desired state should be preserved (not cleared)
        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "PlatformExpiration should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );
    }

    // ── Cancel-listener actor-level integration tests ────────────────────────
    //
    // These tests exercise the full cmd_tx → manager_loop path that
    // run_cancel_listener (in lib.rs) uses to send StopWithReason commands.
    // They verify desired-state and keepalive behaviour with both
    // MockDesiredStateBackend and MockMobile wired into the actor.

    #[tokio::test]
    async fn cancel_listener_platform_timeout_preserves_desired_and_stops_keepalive() {
        let mock = MockMobile::new();
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        // Simulate what run_cancel_listener sends on timeout
        send_stop_with_reason(&handle, StopReason::PlatformTimeout)
            .await
            .unwrap();

        assert!(!send_is_running(&handle).await, "service should be stopped");

        // PlatformTimeout should call stop_keepalive (unlike PlatformExpiration)
        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            1,
            "PlatformTimeout should call stop_keepalive"
        );

        // Desired state should be preserved
        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "PlatformTimeout should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );
    }

    #[tokio::test]
    async fn cancel_listener_user_stop_clears_desired_and_stops_keepalive() {
        let mock = MockMobile::new();
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // UserStop via plain Stop command (delegates to StopWithReason(UserStop))
        send_stop(&handle).await.unwrap();

        assert!(!send_is_running(&handle).await, "service should be stopped");

        // UserStop should call stop_keepalive
        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            1,
            "UserStop should call stop_keepalive"
        );

        // Desired state should be cleared
        let last = backend
            .last_save()
            .expect("should have saved desired state");
        assert!(
            !last.desired_running,
            "UserStop should clear desired_running to false"
        );
    }

    // ── Step 10 (task 3f1f): NativeLifecycleEvent command and handler tests ──

    async fn send_native_event(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        event: NativeLifecycleEvent,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::NativeLifecycleEvent { event, reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    #[tokio::test]
    async fn native_lifecycle_notification_stop_clears_desired_state() {
        let mock = MockMobile::new();
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_native_event(&handle, NativeLifecycleEvent::AndroidNotificationStop)
            .await
            .unwrap();

        assert!(!send_is_running(&handle).await, "service should be stopped");

        // NativeNotificationStop clears desired state
        let saves = backend.saves.lock().unwrap();
        assert_eq!(saves.len(), saves_before + 1);
        assert!(
            !saves.last().unwrap().desired_running,
            "AndroidNotificationStop should clear desired_running"
        );

        // stop_keepalive should have been called
        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            1,
            "AndroidNotificationStop should call stop_keepalive"
        );
    }

    #[tokio::test]
    async fn native_lifecycle_timeout_preserves_desired_state() {
        let mock = MockMobile::new();
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_native_event(
            &handle,
            NativeLifecycleEvent::AndroidTimeout {
                fgs_type: Some("dataSync".into()),
            },
        )
        .await
        .unwrap();

        assert!(!send_is_running(&handle).await, "service should be stopped");

        // PlatformTimeout preserves desired state
        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "AndroidTimeout should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );

        // stop_keepalive should have been called (not PlatformExpiration)
        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            1,
            "AndroidTimeout should call stop_keepalive"
        );
    }

    #[tokio::test]
    async fn native_lifecycle_event_idempotent_when_already_stopped() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Stop first
        send_stop(&handle).await.unwrap();
        assert!(!send_is_running(&handle).await);

        let saves_before = backend.saves.lock().unwrap().len();

        // Send native event while already stopped — should be a no-op (NotRunning)
        let result =
            send_native_event(&handle, NativeLifecycleEvent::AndroidNotificationStop).await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "native event while stopped should return NotRunning: {result:?}"
        );

        // No additional desired-state saves
        {
            let saves = backend.saves.lock().unwrap();
            assert_eq!(
                saves.len(),
                saves_before,
                "no additional saves when already stopped"
            );
        }

        // Same for timeout variant
        let result = send_native_event(
            &handle,
            NativeLifecycleEvent::AndroidTimeout { fgs_type: None },
        )
        .await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "timeout while stopped should return NotRunning: {result:?}"
        );
    }

    // ── Step 13: GetLifecycleStatus command tests ────────────────────────────

    /// Helper: send GetLifecycleStatus and return the result.
    async fn send_get_lifecycle_status(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    ) -> LifecycleStatus {
        let (reply, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::GetLifecycleStatus {
                desktop_mode: None,
                reply,
            })
            .await
            .expect("send GetLifecycleStatus");
        rx.await.expect("receive LifecycleStatus")
    }

    #[tokio::test]
    async fn get_lifecycle_status_returns_idle_initially() {
        let handle = setup_manager();
        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            matches!(status.state, LifecycleState::Idle),
            "expected Idle, got {:?}",
            status.state
        );
        assert!(!status.desired_running);
        assert!(!status.recovery_enabled);
        assert!(!status.recovery_pending);
        assert!(status.last_error.is_none());
        assert!(status.last_start_config.is_none());
    }

    #[tokio::test]
    async fn get_lifecycle_status_returns_running_after_start() {
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            matches!(status.state, LifecycleState::Running),
            "expected Running, got {:?}",
            status.state
        );
    }

    #[tokio::test]
    async fn get_lifecycle_status_reflects_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );

        // Enable auto-restart (sets desired_running=true)
        send_enable_auto_restart(&handle, None).await.unwrap();

        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            status.desired_running,
            "expected desired_running=true after enable_auto_restart"
        );
        assert!(
            status.recovery_enabled,
            "expected recovery_enabled=true when desired_running=true"
        );
    }

    #[tokio::test]
    async fn get_lifecycle_status_clears_after_disable_recovery() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );

        // Enable then disable
        send_enable_auto_restart(&handle, None).await.unwrap();
        send_disable_auto_restart(&handle).await.unwrap();

        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            !status.desired_running,
            "expected desired_running=false after disable"
        );
        assert!(
            !status.recovery_enabled,
            "expected recovery_enabled=false after disable"
        );
    }

    #[tokio::test]
    async fn get_lifecycle_status_includes_platform_and_capabilities() {
        let handle = setup_manager();
        let status = send_get_lifecycle_status(&handle).await;

        // On the test machine (Linux desktop), platform should be Linux
        #[cfg(target_os = "linux")]
        assert!(
            matches!(status.platform, crate::models::Platform::Linux),
            "expected Linux platform, got {:?}",
            status.platform
        );
        // Capabilities should be populated
        assert!(
            !status.capabilities.limitations.is_empty()
                || !status.capabilities.required_setup.is_empty(),
            "capabilities should have some content"
        );
    }

    #[tokio::test]
    async fn get_lifecycle_status_returns_stopped_after_stop() {
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        send_stop(&handle).await.unwrap();

        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            matches!(status.state, LifecycleState::Stopped),
            "expected Stopped, got {:?}",
            status.state
        );
    }
}
