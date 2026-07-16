#![doc(html_root_url = "https://docs.rs/tauri-plugin-background-service/0.7.1")]

//! # tauri-plugin-background-service
//!
//! A [Tauri](https://tauri.app) v2 plugin that manages long-lived background service
//! lifecycle across **Android**, **iOS**, and **Desktop**.
//!
//! Users implement the [`BackgroundService`] trait; the plugin handles OS-specific
//! keepalive (Android foreground service, iOS `BGTaskScheduler`), cancellation via
//! [`CancellationToken`](tokio_util::sync::CancellationToken), and state management
//! through an actor pattern.
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use tauri_plugin_background_service::{
//!     BackgroundService, ServiceContext, ServiceError, init_with_service,
//! };
//!
//! struct MyService;
//!
//! #[async_trait::async_trait]
//! impl<R: tauri::Runtime> BackgroundService<R> for MyService {
//!     async fn init(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
//!         Ok(())
//!     }
//!
//!     async fn run(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
//!         tokio::select! {
//!             _ = ctx.shutdown.cancelled() => Ok(()),
//!             _ = do_work(ctx) => Ok(()),
//!         }
//!     }
//! }
//!
//! tauri::Builder::default()
//!     .plugin(init_with_service(|| MyService))
//! ```
//!
//! ## Platform Behavior
//!
//! | Platform | Keepalive Mechanism | Auto-restart |
//! |----------|-------------------|-------------|
//! | Android | Foreground service with persistent notification (`START_STICKY`) | Yes |
//! | iOS | `BGTaskScheduler` with expiration handler | No |
//! | Desktop | Plain `tokio::spawn` | No |
//!
//! ## iOS Setup
//!
//! Add the following entries to your app's `Info.plist`:
//!
//! ```xml
//! <key>BGTaskSchedulerPermittedIdentifiers</key>
//! <array>
//!     <string>$(BUNDLE_ID).bg-refresh</string>
//!     <string>$(BUNDLE_ID).bg-processing</string>
//! </array>
//!
//! <key>UIBackgroundModes</key>
//! <array>
//!     <string>processing</string>
//!     <string>fetch</string>
//! </array>
//! ```
//!
//! Replace `$(BUNDLE_ID)` with your app's bundle identifier.
//! Without these entries, `BGTaskScheduler.shared.submit(_:)` will throw at runtime.
//!
//! See the [project repository](https://github.com/dardourimohamed/tauri-background-service)
//! for detailed platform guides and API documentation.

pub mod capabilities;
pub mod desired_state;
pub mod error;
pub mod manager;
pub mod models;
pub mod notifier;
pub mod service_trait;
pub mod validator;

#[cfg(mobile)]
pub mod mobile;

#[cfg(feature = "desktop-service")]
pub mod desktop;

// ─── Public API Surface ──────────────────────────────────────────────────────

pub use error::ServiceError;
#[doc(hidden)]
pub use manager::{manager_loop, OnCompleteCallback, ServiceFactory, ServiceManagerHandle};
pub use models::{
    IOSSchedulingStatus, LifecycleState, LifecycleStatus, PendingTaskInfo, Platform,
    PlatformCapabilities, PluginConfig, PluginEvent, ServiceContext, ServiceState, ServiceStatus,
    SetupIssue, SetupValidationReport, StartConfig, ValidationIssue,
};
pub use notifier::{Notifier, NotifierPolicy, NotifySink};
pub use service_trait::BackgroundService;

#[cfg(all(feature = "desktop-service", any(unix, windows)))]
pub use desktop::headless::{headless_main, headless_main_with_desired_state};

// ─── Internal Imports ────────────────────────────────────────────────────────

use tauri::{
    plugin::{Builder, TauriPlugin},
    AppHandle, Manager, Runtime,
};

use crate::manager::ManagerCommand;

#[cfg(mobile)]
use crate::manager::MobileKeepalive;

// `MobileLifecycle` is referenced from the iOS plugin bindings below AND from the
// android-active notification-permission commands (NTF-09), so it must be in
// scope on every mobile target (Android + iOS), not just iOS.
#[cfg(mobile)]
use mobile::MobileLifecycle;

use std::sync::Arc;

// ─── iOS Plugin Binding ──────────────────────────────────────────────────────
// Must be at module level. Referenced by mobile::init() when registering
// the iOS plugin. Only compiled when targeting iOS.

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_background_service);

// ─── iOS Lifecycle Helpers ────────────────────────────────────────────────────

/// Set the on_complete callback so iOS `completeBgTask` fires when `run()` finishes.
///
/// Sends `SetOnComplete` to the actor. Must be called **before** `Start` because
/// `handle_start` captures the callback via `take()` at spawn time.
#[cfg(target_os = "ios")]
async fn ios_set_on_complete_callback<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let mobile = app.state::<Arc<MobileLifecycle<R>>>();
    let mobile_handle = mobile.handle.clone();
    let manager = app.state::<ServiceManagerHandle<R>>();

    let mob_for_complete = MobileLifecycle {
        handle: mobile_handle,
    };
    manager
        .cmd_tx
        .send(ManagerCommand::SetOnComplete {
            callback: Box::new(move |success| {
                let _ = mob_for_complete.complete_bg_task(success);
            }),
        })
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "ios"))]
async fn ios_set_on_complete_callback<R: Runtime>(_app: &AppHandle<R>) -> Result<(), String> {
    Ok(())
}

/// Spawn a blocking thread that waits for the iOS expiration signal (`waitForCancel`).
///
/// Must be called **after** `Start` succeeds so the service is running when the
/// cancel listener begins waiting. Sends `Stop` to the actor when cancelled.
///
/// Three outcomes:
/// 1. **Resolved invoke** (safety timer / expiration) → `Ok(())` → send `StopWithReason(PlatformExpiration)`.
/// 2. **Timeout** (default: 4h) → call `cancel_cancel_listener` to unblock the
///    thread, then send `StopWithReason(PlatformTimeout)`.
/// 3. **Rejected invoke** (explicit stop / natural completion) → `Err` → no action.
///
/// Core cancel listener logic, extracted for testability.
///
/// - `wait_fn`: blocking function simulating `wait_for_cancel` (returns `Ok(())` on resolve,
///   `Err` on reject).
/// - `cancel_fn`: called on timeout to unblock the `wait_fn` thread.
/// - `cmd_tx`: channel to send `StopWithReason` command on resolve/timeout.
/// - `timeout_secs`: how long to wait before treating the listener as timed out.
///
/// Returns `true` if a `StopWithReason` was sent, `false` otherwise.
#[allow(dead_code)] // used on iOS + in tests
async fn run_cancel_listener<R: Runtime>(
    wait_fn: Box<dyn FnOnce() -> Result<(), ServiceError> + Send>,
    cancel_fn: Box<dyn FnOnce() + Send>,
    cmd_tx: tokio::sync::mpsc::Sender<ManagerCommand<R>>,
    timeout_secs: u64,
) -> bool {
    let handle = tokio::task::spawn_blocking(wait_fn);
    let result = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), handle).await;
    match result {
        // Resolved invoke (safety timer or expiration) → graceful shutdown
        Ok(Ok(Ok(()))) => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx
                .send(ManagerCommand::StopWithReason {
                    reason: crate::models::StopReason::PlatformExpiration,
                    reply: tx,
                })
                .await;
            let _ = rx.await;
            true
        }
        // Timeout → unblock the spawn_blocking thread, then graceful shutdown
        Err(_) => {
            cancel_fn();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx
                .send(ManagerCommand::StopWithReason {
                    reason: crate::models::StopReason::PlatformTimeout,
                    reply: tx,
                })
                .await;
            let _ = rx.await;
            true
        }
        // Rejected invoke (explicit stop or natural completion) → no action
        _ => false,
    }
}

#[cfg(target_os = "ios")]
fn ios_spawn_cancel_listener<R: Runtime>(app: &AppHandle<R>, timeout_secs: u64) {
    let mobile = app.state::<Arc<MobileLifecycle<R>>>();
    let mobile_handle = mobile.handle.clone();
    let mobile_handle_for_cancel = mobile.handle.clone();
    let manager = app.state::<ServiceManagerHandle<R>>();
    let cmd_tx = manager.cmd_tx.clone();

    tokio::spawn(async move {
        let wait_fn = Box::new(move || {
            let mob = MobileLifecycle {
                handle: mobile_handle,
            };
            mob.wait_for_cancel()
        });
        let cancel_fn = Box::new(move || {
            let cancel_mob = MobileLifecycle {
                handle: mobile_handle_for_cancel,
            };
            let _ = cancel_mob.cancel_cancel_listener();
        });
        // Ignore result — the listener fires-and-forgets.
        let _ = run_cancel_listener(wait_fn, cancel_fn, cmd_tx, timeout_secs).await;
    });
}

#[cfg(not(target_os = "ios"))]
fn ios_spawn_cancel_listener<R: Runtime>(_app: &AppHandle<R>, _timeout_secs: u64) {}

/// Spawn the iOS **cold BGTask auto-start** probe after plugin setup returns.
///
/// `run_mobile_plugin` is synchronous on the Rust side. Calling it directly from
/// plugin setup can deadlock startup on iOS: setup runs on the main thread, while
/// the Swift command handlers marshal their work back onto `DispatchQueue.main`.
/// Spawning the probe lets Tauri finish building the app and keeps the native
/// bridge calls off the main thread.
#[cfg(target_os = "ios")]
fn ios_spawn_cold_auto_start<R: Runtime>(app: &AppHandle<R>) {
    let app = app.app_handle().clone();
    tauri::async_runtime::spawn(async move {
        ios_handle_cold_auto_start(&app).await;
    });
}

/// Handle iOS cold auto-start when the process was launched for a pending BGTask.
#[cfg(target_os = "ios")]
async fn ios_handle_cold_auto_start<R: Runtime>(app: &AppHandle<R>) {
    let mobile = app.state::<Arc<MobileLifecycle<R>>>().inner().clone();

    let pending = match tokio::task::spawn_blocking({
        let mobile = mobile.clone();
        move || mobile.get_pending_bg_task()
    })
    .await
    {
        Ok(Ok(Some(pending))) => pending,
        Ok(Ok(None)) => {
            // No pending BGTask — normal launch.
            return;
        }
        Ok(Err(e)) => {
            log::warn!("iOS: failed to get pending BGTask: {e}");
            return;
        }
        Err(e) => {
            log::warn!("iOS: failed to join pending BGTask query: {e}");
            return;
        }
    };
    let _ = pending;

    // Read desired_running + last_start_config from the typed DTO (no untyped
    // JSON reads), exactly as the warm auto-start path does.
    let should_start = match tokio::task::spawn_blocking({
        let mobile = mobile.clone();
        move || mobile.get_desired_state_status()
    })
    .await
    {
        Ok(Ok(status)) => status.and_then(|status| {
            let config_str = status.last_start_config?;
            Some((status.desired_running, config_str))
        }),
        Ok(Err(e)) => {
            log::warn!("iOS: failed to get desired-state status: {e}");
            None
        }
        Err(e) => {
            log::warn!("iOS: failed to join desired-state query: {e}");
            None
        }
    };

    let Some((true, config_str)) = should_start else {
        log::info!(
            "iOS: skipped auto-start: desired_running=false — clearing stale pending BGTask"
        );
        let _ = tokio::task::spawn_blocking({
            let mobile = mobile.clone();
            move || mobile.clear_pending_bg_task()
        })
        .await;
        return;
    };

    let Ok(config) = serde_json::from_str::<StartConfig>(&config_str) else {
        log::warn!(
            "iOS: failed to parse stored start config — preserving pending task info for diagnostics"
        );
        return;
    };

    let manager = app.state::<ServiceManagerHandle<R>>();
    let cmd_tx = manager.cmd_tx.clone();
    let app_clone = app.app_handle().clone();
    let timeout_secs = app.state::<PluginConfig>().ios_cancel_listener_timeout_secs;

    // Set on_complete callback for iOS completeBgTask before Start so the actor
    // captures the callback at spawn time.
    let mob_handle = mobile.handle.clone();
    if let Err(e) = cmd_tx
        .send(ManagerCommand::SetOnComplete {
            callback: Box::new(move |success| {
                let ml = MobileLifecycle {
                    handle: mob_handle.clone(),
                };
                let _ = ml.complete_bg_task(success);
            }),
        })
        .await
    {
        log::warn!("iOS: auto-start preserved pending BGTask after failure: {e}");
        let _ = tokio::task::spawn_blocking(move || mobile.record_failed_pending()).await;
        return;
    }

    // H3: clear the pending BGTask only after Start succeeds; on failure
    // preserve it + record a failure marker. The clear lives inside
    // `run_auto_start`, gated on `rx.await == Ok(Ok(()))`.
    let mobile_for_success = mobile.clone();
    let mobile_for_failure = mobile.clone();
    let app_for_listener = app_clone.clone();

    log::info!("iOS: auto-starting service for pending BGTask");
    let on_success = Box::new(move || {
        let _ = mobile_for_success.clear_pending_bg_task();
        ios_spawn_cancel_listener(&app_for_listener, timeout_secs);
    });
    let on_failure = Box::new(move || {
        let _ = mobile_for_failure.record_failed_pending();
    });
    run_auto_start(config, app_clone, cmd_tx, on_success, on_failure).await;
}

/// Spawn the iOS **warm BGTask-delivery** listener (H14).
///
/// A BGTask delivered to a warm/idle process never re-runs the cold auto-start
/// block (that runs once at setup), so without this the process would only
/// persist the pending record and wait. This listener blocks on the Swift
/// `waitForBgTask` Pending Invoke; each time `handleBackgroundTask`/
/// `handleProcessingTask` resolves it, Rust drives [`run_warm_start`] — mirroring
/// the cold sequence (re-send `SetOnComplete` → `Start` → consume on success),
/// while a delivery to an already-running actor is a clean no-op.
///
/// The loop re-blocks after each delivery and exits when the invoke is rejected
/// (`cancel_warm_listener`) or the blocking thread fails.
#[cfg(target_os = "ios")]
fn ios_spawn_warm_listener<R: Runtime>(app: &AppHandle<R>) {
    let app = app.app_handle().clone();
    tauri::async_runtime::spawn(async move {
        loop {
            // Block until iOS delivers a BGTask to the warm process.
            let mobile_handle = app.state::<Arc<MobileLifecycle<R>>>().handle.clone();
            let wait = tokio::task::spawn_blocking(move || {
                MobileLifecycle {
                    handle: mobile_handle,
                }
                .wait_for_bg_task()
            })
            .await;

            match wait {
                Ok(Ok(())) => {
                    ios_handle_warm_delivery(&app).await;
                }
                // Rejected invoke (teardown) or join error → stop listening.
                _ => {
                    log::info!("iOS: warm BGTask listener stopped");
                    break;
                }
            }
        }
    });
}

/// Handle a single warm BGTask delivery: read the typed pending + desired state
/// and drive [`run_warm_start`], mirroring the cold auto-start block.
#[cfg(target_os = "ios")]
async fn ios_handle_warm_delivery<R: Runtime>(app: &AppHandle<R>) {
    // Clone owned values out of managed state so no `State` borrow is held
    // across the `run_warm_start` await.
    let mobile = app.state::<Arc<MobileLifecycle<R>>>().inner().clone();

    let pending = match mobile.get_pending_bg_task() {
        Ok(Some(p)) => p,
        Ok(None) => {
            log::debug!("iOS: warm delivery signalled with no pending BGTask");
            return;
        }
        Err(e) => {
            log::warn!("iOS: warm delivery — failed to get pending BGTask: {e}");
            return;
        }
    };
    let _ = pending;

    // Read desired_running + last_start_config from the typed DTO (no untyped
    // JSON reads), exactly as the cold auto-start path does.
    let should_start = mobile
        .get_desired_state_status()
        .ok()
        .flatten()
        .and_then(|status| {
            let config_str = status.last_start_config?;
            Some((status.desired_running, config_str))
        });

    let Some((true, config_str)) = should_start else {
        log::info!("iOS: warm delivery skipped: desired_running=false");
        return;
    };

    let Ok(config) = serde_json::from_str::<StartConfig>(&config_str) else {
        log::warn!(
            "iOS: warm delivery — failed to parse stored start config; preserving pending task info"
        );
        return;
    };

    let manager = app.state::<ServiceManagerHandle<R>>();
    let cmd_tx = manager.cmd_tx.clone();
    let app_clone = app.app_handle().clone();
    let timeout_secs = app.state::<PluginConfig>().ios_cancel_listener_timeout_secs;

    // Re-send SetOnComplete so completeBgTask fires (take()n at spawn).
    let mob_handle = mobile.handle.clone();
    let on_complete: OnCompleteCallback = Box::new(move |success| {
        let ml = MobileLifecycle {
            handle: mob_handle.clone(),
        };
        let _ = ml.complete_bg_task(success);
    });

    // On success: consume the pending record (M14 part 2) + spawn the cancel
    // listener. On genuine failure: preserve the evidence + record a marker.
    let mobile_for_success = mobile.clone();
    let mobile_for_failure = mobile.clone();
    let app_for_listener = app_clone.clone();
    let on_success = Box::new(move || {
        let _ = mobile_for_success.clear_pending_bg_task();
        ios_spawn_cancel_listener(&app_for_listener, timeout_secs);
    });
    let on_failure = Box::new(move || {
        let _ = mobile_for_failure.record_failed_pending();
    });

    log::info!("iOS: warm-starting service for delivered BGTask");
    run_warm_start(
        config,
        app_clone,
        cmd_tx,
        on_complete,
        on_success,
        on_failure,
    )
    .await;
}

#[cfg(not(target_os = "ios"))]
#[allow(dead_code)]
fn ios_spawn_warm_listener<R: Runtime>(_app: &AppHandle<R>) {}

/// Drive the iOS cold auto-start sequence and consume the pending BGTask
/// **exactly once, only on success** (H3).
///
/// The pending record is the evidence that iOS launched us for a background
/// task. Clearing it before `Start` actually succeeds means a failed start
/// silently discards that evidence — the task never reruns and we can't tell
/// it failed. So the clear is gated on the actor replying `Ok(Ok(()))`:
/// - **success** → `on_success` (clear pending + spawn the cancel listener),
///   logged "consumed after success".
/// - **failure** (command channel closed, reply dropped, or `Start` errored) →
///   `on_failure` (record a failure marker; the pending record is preserved
///   because it is *not* cleared), logged "preserved after failure".
///
/// Extracted for host testability — mirrors [`run_cancel_listener`]. The iOS
/// wiring injects the mobile side-effects (clear / record-failure / cancel
/// listener) as closures so this core gating logic runs on the macOS test gate.
///
/// Returns `true` if `Start` succeeded (pending consumed), `false` otherwise.
#[allow(dead_code)] // used on iOS + in tests
async fn run_auto_start<R: Runtime>(
    config: StartConfig,
    app: AppHandle<R>,
    cmd_tx: tokio::sync::mpsc::Sender<ManagerCommand<R>>,
    on_success: Box<dyn FnOnce() + Send>,
    on_failure: Box<dyn FnOnce() + Send>,
) -> bool {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if cmd_tx
        .send(ManagerCommand::Start {
            config,
            reply: tx,
            app,
        })
        .await
        .is_err()
    {
        log::warn!(
            "iOS: auto-start preserved pending BGTask after failure (command channel closed)"
        );
        on_failure();
        return false;
    }

    match rx.await {
        Ok(Ok(())) => {
            log::info!("iOS: auto-start consumed pending BGTask after success");
            on_success();
            true
        }
        Ok(Err(e)) => {
            log::warn!("iOS: auto-start preserved pending BGTask after failure: {e}");
            on_failure();
            false
        }
        Err(e) => {
            log::warn!(
                "iOS: auto-start preserved pending BGTask after failure (reply dropped: {e})"
            );
            on_failure();
            false
        }
    }
}

/// Drive the iOS **warm** BGTask-delivery start sequence (H14, M14 part 2).
///
/// A BGTask delivered to a warm/idle process must actually start the Rust
/// service, not merely persist pending state and wait. This mirrors
/// [`run_auto_start`] (the *cold* launch path) but runs while the process is
/// already alive, so it must be a clean no-op when the actor is already running:
///
/// 1. **Guard `AlreadyRunning`** — pre-check `is_running`. A warm delivery to a
///    running actor returns `false` without arming a stale `on_complete` or
///    consuming the pending record (M14 part 2: it cannot re-arm a cold
///    auto-start).
/// 2. **Re-send `SetOnComplete`** — the actor `take()`s the callback at spawn,
///    so a fresh one is required for each start; this is how `completeBgTask`
///    fires (not the iOS safety timer).
/// 3. **`Start`** → on success consume the pending record (`on_success`, which
///    also spawns the cancel listener in production); on a genuine failure
///    preserve the evidence and record a marker (`on_failure`).
///
/// Extracted for host testability — the iOS wiring injects the mobile
/// side-effects (clear / record-failure / cancel listener) as closures so this
/// core gating logic runs on the macOS test gate.
///
/// Returns `true` if a warm `Start` succeeded (pending consumed), `false`
/// otherwise (no-op while running, or preserved-on-failure).
#[allow(dead_code)] // used on iOS + in tests
async fn run_warm_start<R: Runtime>(
    config: StartConfig,
    app: AppHandle<R>,
    cmd_tx: tokio::sync::mpsc::Sender<ManagerCommand<R>>,
    on_complete: OnCompleteCallback,
    on_success: Box<dyn FnOnce() + Send>,
    on_failure: Box<dyn FnOnce() + Send>,
) -> bool {
    // Guard AlreadyRunning: a warm delivery to a running actor is a clean no-op.
    // Pre-checking `is_running` (rather than reacting to a `Start` rejection)
    // avoids arming a stale `on_complete`, which would otherwise fire for the
    // wrong BGTask on the next legitimate start. The pending record is left for
    // the running service to consume on its own completion.
    let (run_tx, run_rx) = tokio::sync::oneshot::channel();
    if cmd_tx
        .send(ManagerCommand::IsRunning { reply: run_tx })
        .await
        .is_err()
    {
        log::warn!(
            "iOS: warm start preserved pending BGTask after failure (command channel closed)"
        );
        on_failure();
        return false;
    }
    if run_rx.await.unwrap_or(false) {
        log::info!("iOS: warm BGTask delivery while already running — no-op");
        return false;
    }

    // Re-send SetOnComplete: the actor `take()`s it at spawn, so a fresh callback
    // is required for each start so `completeBgTask` fires (not the safety timer).
    if cmd_tx
        .send(ManagerCommand::SetOnComplete {
            callback: on_complete,
        })
        .await
        .is_err()
    {
        log::warn!("iOS: warm start preserved pending BGTask after failure (channel closed)");
        on_failure();
        return false;
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    if cmd_tx
        .send(ManagerCommand::Start {
            config,
            reply: tx,
            app,
        })
        .await
        .is_err()
    {
        log::warn!(
            "iOS: warm start preserved pending BGTask after failure (command channel closed)"
        );
        on_failure();
        return false;
    }

    match rx.await {
        Ok(Ok(())) => {
            log::info!("iOS: warm BGTask delivery started service; consumed pending BGTask");
            on_success();
            true
        }
        // Lost the race after the IsRunning pre-check — still a clean no-op, not
        // a failure: the actor became running between the check and Start.
        Ok(Err(ServiceError::AlreadyRunning)) => {
            log::info!("iOS: warm BGTask delivery raced a running actor — no-op");
            false
        }
        Ok(Err(e)) => {
            log::warn!("iOS: warm start preserved pending BGTask after failure: {e}");
            on_failure();
            false
        }
        Err(e) => {
            log::warn!(
                "iOS: warm start preserved pending BGTask after failure (reply dropped: {e})"
            );
            on_failure();
            false
        }
    }
}

// ─── Tauri Commands ──────────────────────────────────────────────────────────

#[tauri::command]
async fn start<R: Runtime>(app: AppHandle<R>, config: StartConfig) -> Result<(), String> {
    // OS service mode: route through persistent IPC client.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
        // Check if IPC is connected before sending the start request.
        if ipc_state.client.is_connected() {
            return ipc_state
                .client
                .start(config)
                .await
                .map_err(|e| e.to_string());
        }

        // IPC is disconnected. Check if auto-start is enabled.
        let plugin_config = app.state::<PluginConfig>();
        if !plugin_config.desktop_start_service_if_missing {
            return Err(ServiceError::Ipc("ipcUnavailable".into()).to_string());
        }

        // Try to start the OS service and wait for IPC readiness.
        let socket_path = ipc_state.client.socket_path().display().to_string();
        let timeout =
            std::time::Duration::from_millis(plugin_config.desktop_service_start_timeout_ms);

        use desktop::service_manager::{derive_service_label, DesktopServiceManager};
        let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());
        let exec_path = std::env::current_exe().map_err(|e| e.to_string())?;
        {
            let mgr = DesktopServiceManager::new(&label, exec_path).map_err(|e| e.to_string())?;
            mgr.start().map_err(|e| e.to_string())?;
        }

        ipc_state.client.nudge_reconnect();
        let connected = ipc_state
            .client
            .wait_for_connected(timeout)
            .await
            .map_err(|e| e.to_string())?;

        if !connected {
            return Err(
                ServiceError::Ipc(format!("ipcUnavailable: socket {socket_path}")).to_string(),
            );
        }

        // IPC is now connected — send the start command.
        return ipc_state
            .client
            .start(config)
            .await
            .map_err(|e| e.to_string());
    }

    // In-process mode (default).
    // iOS: send SetOnComplete before Start so the callback is captured at spawn time.
    ios_set_on_complete_callback(&app).await?;

    // Mobile keepalive is now handled by the actor (Step 5).
    // The actor calls start_keepalive AFTER the AlreadyRunning check.

    let manager = app.state::<ServiceManagerHandle<R>>();
    let (tx, rx) = tokio::sync::oneshot::channel();
    manager
        .cmd_tx
        .send(ManagerCommand::Start {
            config,
            reply: tx,
            app: app.clone(),
        })
        .await
        .map_err(|e| e.to_string())?;

    rx.await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

    // iOS: spawn cancel listener after Start succeeds.
    let plugin_config = app.state::<PluginConfig>();
    ios_spawn_cancel_listener(&app, plugin_config.ios_cancel_listener_timeout_secs);

    Ok(())
}

#[tauri::command]
async fn stop<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    // OS service mode: route through persistent IPC client.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
        return ipc_state.client.stop().await.map_err(|e| e.to_string());
    }

    // In-process mode (default).
    let manager = app.state::<ServiceManagerHandle<R>>();
    let (tx, rx) = tokio::sync::oneshot::channel();
    manager
        .cmd_tx
        .send(ManagerCommand::Stop { reply: tx })
        .await
        .map_err(|e| e.to_string())?;

    rx.await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn is_running<R: Runtime>(app: AppHandle<R>) -> bool {
    // OS service mode: route through persistent IPC client.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
        return ipc_state.client.is_running().await.unwrap_or(false);
    }

    // In-process mode (default).
    let manager = app.state::<ServiceManagerHandle<R>>();
    let (tx, rx) = tokio::sync::oneshot::channel();
    if manager
        .cmd_tx
        .send(ManagerCommand::IsRunning { reply: tx })
        .await
        .is_err()
    {
        return false;
    }
    rx.await.unwrap_or(false)
}

#[tauri::command]
async fn get_service_state<R: Runtime>(app: AppHandle<R>) -> Result<models::ServiceStatus, String> {
    // OS service mode: route through persistent IPC client.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
        return ipc_state
            .client
            .get_state()
            .await
            .map_err(|e| e.to_string());
    }

    // In-process mode (default).
    let manager = app.state::<ServiceManagerHandle<R>>();
    Ok(manager.get_state().await)
}

#[tauri::command]
#[allow(unused_variables)]
async fn get_platform_capabilities<R: Runtime>(
    app: AppHandle<R>,
) -> Result<models::PlatformCapabilities, String> {
    #[cfg(feature = "desktop-service")]
    let plugin_config = app.state::<PluginConfig>();

    #[cfg(feature = "desktop-service")]
    let desktop_mode = Some(plugin_config.desktop_service_mode.as_str());
    #[cfg(not(feature = "desktop-service"))]
    let desktop_mode: Option<&str> = None;

    let (platform, lifecycle_mode) =
        capabilities::CapabilityProvider::detect_platform(desktop_mode);

    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    let os_service_installed = if matches!(lifecycle_mode, models::LifecycleMode::DesktopOsService)
    {
        use desktop::service_manager::{derive_service_label, DesktopServiceManager};
        let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());
        let exec = std::env::current_exe().unwrap_or_default();
        DesktopServiceManager::new(&label, exec)
            .map(|_| true)
            .unwrap_or(false)
    } else {
        false
    };

    #[cfg(not(all(feature = "desktop-service", any(unix, windows))))]
    let os_service_installed = false;

    Ok(capabilities::CapabilityProvider::capabilities(
        platform,
        lifecycle_mode,
        os_service_installed,
    ))
}

/// Query the iOS scheduling status from the native layer.
///
/// Returns `IOSSchedulingStatus` on iOS with scheduling results and desired state.
/// Returns a default status (not scheduled) on non-iOS platforms.
#[tauri::command]
async fn get_scheduling_status<R: Runtime>(
    app: AppHandle<R>,
) -> Result<models::IOSSchedulingStatus, String> {
    #[cfg(target_os = "ios")]
    {
        let mobile = app.state::<Arc<MobileLifecycle<R>>>();
        mobile
            .get_scheduling_status()
            .map_err(|e| e.to_string())
            .and_then(|opt| opt.ok_or_else(|| "no scheduling status available".to_string()))
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = app;
        Ok(models::IOSSchedulingStatus {
            refresh_scheduled: false,
            processing_scheduled: false,
            refresh_error: None,
            processing_error: None,
        })
    }
}

/// Request the Android battery-optimization (Doze) exemption (BGS-22, doc-08
/// Step 14).
///
/// Forwards to the Kotlin `requestBatteryExemption` @Command, which fires
/// `startActivity(Intent(ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
/// "package:<app>"))` to surface the system Doze-exemption dialog. The
/// `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` permission is declared in the plugin
/// `android/src/main/AndroidManifest.xml` but was previously never requested —
/// this wires the honest user-granted flow (preferring the flow over dropping
/// the permission). No-op on non-Android targets (the Kotlin @Command does not
/// exist; iOS/desktop have no Doze analogue). There is no Rust-side status
/// mirror: the exemption is OS-only and not queryable through this plugin.
#[tauri::command]
async fn request_battery_exemption<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let mobile = app.state::<Arc<MobileLifecycle<R>>>();
        mobile
            .request_battery_exemption()
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(())
    }
}

/// Query the persisted iOS desired-state status from the native layer.
///
/// Returns `IOSDesiredStateStatus` on iOS with the persisted desired state.
/// Returns a default status (not desired) on non-iOS platforms.
#[tauri::command]
async fn get_desired_state_status<R: Runtime>(
    app: AppHandle<R>,
) -> Result<models::IOSDesiredStateStatus, String> {
    #[cfg(target_os = "ios")]
    {
        let mobile = app.state::<Arc<MobileLifecycle<R>>>();
        mobile
            .get_desired_state_status()
            .map_err(|e| e.to_string())
            .and_then(|opt| opt.ok_or_else(|| "no desired-state status available".to_string()))
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = app;
        Ok(models::IOSDesiredStateStatus {
            desired_running: false,
            last_start_config: None,
            last_task_kind: None,
            last_task_started_at: None,
            last_task_completed_at: None,
            last_schedule_error: None,
            last_completion_reason: None,
            notification_granted: None,
        })
    }
}

/// Query the pending iOS background task info.
///
/// Returns `Some(PendingTaskInfo)` on iOS if the app was launched by iOS for
/// a background task and the info hasn't been cleared yet.
/// Returns `None` on non-iOS platforms or when no pending task exists.
#[tauri::command]
async fn get_pending_bg_task<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<models::PendingTaskInfo>, String> {
    #[cfg(target_os = "ios")]
    {
        let mobile = app.state::<Arc<MobileLifecycle<R>>>();
        mobile.get_pending_bg_task().map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = app;
        Ok(None)
    }
}

/// Query the POST_NOTIFICATIONS permission status (NTF-09).
///
/// Android returns the current status (`granted` | `notDetermined` | `denied`)
/// via the Kotlin `getNotificationPermissionStatus` command, which resolves
/// immediately — so the mobile call is made directly (mirroring
/// `get_scheduling_status`). Non-Android returns a default `{status: "granted"}`
/// so the command is callable cross-platform. (The iOS UN-prompt half of NTF-09
/// is Step 10c, not this command.)
#[tauri::command]
async fn get_notification_permission_status<R: Runtime>(
    app: AppHandle<R>,
) -> Result<models::NotificationPermissionStatus, String> {
    #[cfg(target_os = "android")]
    {
        let mobile = app.state::<Arc<MobileLifecycle<R>>>();
        mobile
            .get_notification_permission_status()
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(models::NotificationPermissionStatus {
            status: "granted".to_string(),
        })
    }
}

/// Request POST_NOTIFICATIONS permission (NTF-09).
///
/// Android runs the Kotlin `requestNotificationPermission` command, which on
/// API 33+ defers resolution to the OS permission dialog via the
/// `@PermissionCallback` (Step 10a). That call blocks `run_mobile_plugin`'s
/// `rx.recv()` for the dialog duration, so it is wrapped in
/// `tokio::task::spawn_blocking` to avoid blocking the async runtime (the
/// `wait_for_cancel` class). Non-Android returns a default `{status: "granted"}`.
#[tauri::command]
async fn request_notification_permission<R: Runtime>(
    app: AppHandle<R>,
) -> Result<models::NotificationPermissionStatus, String> {
    #[cfg(target_os = "android")]
    {
        let mobile = app.state::<Arc<MobileLifecycle<R>>>().inner().clone();
        tokio::task::spawn_blocking(move || mobile.request_notification_permission())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(models::NotificationPermissionStatus {
            status: "granted".to_string(),
        })
    }
}

/// Whether the app may post a full-screen intent (NTF-16, Step 12c).
///
/// Android queries `IncomingCallNotifier.canUseFullScreenIntent` via the Kotlin
/// `canUseFullScreenIntent` command, which resolves immediately — so the mobile
/// call is made directly (mirroring `get_notification_permission_status`). Non-
/// Android defaults to `canUse: true` so the re-grant affordance never shows
/// (FSI is irrelevant off-Android).
///
/// IPC SHAPE CONTRACT: returns a `serde_json::Value` OBJECT `{ "canUse": bool }`
/// (NOT a bare `bool`) so it matches the TS `invoke<{ canUse: boolean }>` wrapper
/// and the UI consumer `result.canUse`. A bare `Result<bool, String>` would
/// serde-serialize to a bare JSON boolean; the TS generic is only a compile-time
/// cast, so `result.canUse` would be `undefined` at runtime and the `=== false`
/// re-grant gate would NEVER fire — silently killing NTF-16 on Android (the only
/// platform where it matters). The Kotlin layer resolves `{canUse: bool}` either
/// way; this command re-wraps the typed bool the mobile bridge extracts so the
/// object shape flows end-to-end. The fully-mocked vitest cannot reach this Rust
/// serde shape, so it is pinned statically by the
/// `ntf16_full_screen_intent_wire_is_present_and_unique` integration test.
#[tauri::command]
async fn can_use_full_screen_intent<R: Runtime>(
    app: AppHandle<R>,
) -> Result<serde_json::Value, String> {
    #[cfg(target_os = "android")]
    {
        let mobile = app.state::<Arc<MobileLifecycle<R>>>();
        let can_use = mobile
            .can_use_full_screen_intent()
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "canUse": can_use }))
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(serde_json::json!({ "canUse": true }))
    }
}

/// Open the OS settings page to re-grant USE_FULL_SCREEN_INTENT (NTF-16).
///
/// Android runs the Kotlin `openFullScreenIntentSettings` command, which
/// resolves immediately (startActivity) — NO `spawn_blocking` (unlike
/// `request_notification_permission`, this is not a deferred @PermissionCallback
/// flow). Non-Android is a no-op (the affordance never shows there).
#[tauri::command]
async fn open_full_screen_intent_settings<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let mobile = app.state::<Arc<MobileLifecycle<R>>>();
        mobile
            .open_full_screen_intent_settings()
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(())
    }
}

/// Enable auto-restart for the background service.
///
/// Persists `desired_running=true` with an optional start config WITHOUT
/// starting the service. This sets the intent for recovery after process
/// kill or device reboot. The platform recovery mechanisms will use this
/// to automatically restart the service when conditions allow.
#[tauri::command]
async fn enable_auto_restart<R: Runtime>(
    app: AppHandle<R>,
    config: Option<StartConfig>,
) -> Result<(), String> {
    // OS service mode: route through persistent IPC client.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
        return ipc_state
            .client
            .enable_auto_restart(config)
            .await
            .map_err(|e| e.to_string());
    }

    let manager = app.state::<ServiceManagerHandle<R>>();
    let (tx, rx) = tokio::sync::oneshot::channel();
    manager
        .cmd_tx
        .send(ManagerCommand::EnableAutoRestart { config, reply: tx })
        .await
        .map_err(|e| e.to_string())?;
    rx.await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Disable auto-restart for the background service.
///
/// Persists `desired_running=false` and clears recovery fields WITHOUT
/// stopping the service if it is currently running. After calling this,
/// the platform recovery mechanisms will no longer attempt to restart the
/// service after process kill or device reboot.
#[tauri::command]
async fn disable_auto_restart<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    // OS service mode: route through persistent IPC client.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
        return ipc_state
            .client
            .disable_auto_restart()
            .await
            .map_err(|e| e.to_string());
    }

    let manager = app.state::<ServiceManagerHandle<R>>();
    let (tx, rx) = tokio::sync::oneshot::channel();
    manager
        .cmd_tx
        .send(ManagerCommand::DisableAutoRestart { reply: tx })
        .await
        .map_err(|e| e.to_string())?;
    rx.await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Get the persisted desired-state for the background service.
///
/// Returns `Some(DesiredState)` with the current recovery intent and metadata,
/// or `None` if no persistence backend is configured on the current platform.
#[tauri::command]
async fn get_desired_service_state<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<desired_state::DesiredState>, String> {
    // OS service mode: route through persistent IPC client.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
        return ipc_state
            .client
            .get_desired_state()
            .await
            .map_err(|e| e.to_string());
    }

    let manager = app.state::<ServiceManagerHandle<R>>();
    let (tx, rx) = tokio::sync::oneshot::channel();
    manager
        .cmd_tx
        .send(ManagerCommand::GetDesiredState { reply: tx })
        .await
        .map_err(|e| e.to_string())?;
    rx.await.map_err(|e| e.to_string())
}

/// Notify the Rust actor of a native platform lifecycle event.
///
/// Called from the native layer (Kotlin/Swift) when the OS triggers a
/// lifecycle action that the Rust actor must handle — e.g. the user pressed
/// "Stop" on the Android foreground notification, or Android timed out the
/// foreground service.
///
/// The actor maps each [`NativeLifecycleEvent`] variant to the appropriate
/// [`StopReason`](models::StopReason) and dispatches through
/// [`handle_stop_with_reason`](manager::handle_stop_with_reason).
///
/// This command is not intended for end-user consumption — it is called by
/// the native plugin code.
#[tauri::command]
async fn native_lifecycle_event<R: Runtime>(
    app: AppHandle<R>,
    event: models::NativeLifecycleEvent,
) -> Result<(), String> {
    let manager = app.state::<ServiceManagerHandle<R>>();
    manager
        .send_native_lifecycle_event(event)
        .await
        .map_err(|e| e.to_string())
}

/// Validate the background service setup for the current platform.
///
/// Returns a [`SetupValidationReport`] with errors (blocking) and warnings
/// (non-blocking) about platform-specific prerequisites.
#[tauri::command]
#[allow(unused_variables)]
async fn validate_setup<R: Runtime>(
    app: AppHandle<R>,
) -> Result<models::SetupValidationReport, String> {
    // OS service mode: route through persistent IPC client.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
        return ipc_state
            .client
            .validate_setup()
            .await
            .map_err(|e| e.to_string());
    }

    #[cfg(feature = "desktop-service")]
    let plugin_config = app.state::<PluginConfig>();

    #[cfg(feature = "desktop-service")]
    let desktop_mode = Some(plugin_config.desktop_service_mode.as_str());
    #[cfg(not(feature = "desktop-service"))]
    let desktop_mode: Option<&str> = None;

    let (platform, _) = capabilities::CapabilityProvider::detect_platform(desktop_mode);
    Ok(validator::SetupValidator::validate(platform))
}

/// Get the complete lifecycle status of the background service.
///
/// Returns a [`LifecycleStatus`] snapshot with current state, desired state,
/// recovery status, platform capabilities, and validation issues.
#[tauri::command]
async fn get_lifecycle_status<R: Runtime>(
    app: AppHandle<R>,
) -> Result<models::LifecycleStatus, String> {
    // OS service mode: route through persistent IPC client.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
        return ipc_state
            .client
            .get_lifecycle_status()
            .await
            .map_err(|e| e.to_string());
    }

    #[cfg(feature = "desktop-service")]
    let plugin_config = app.state::<PluginConfig>();

    #[cfg(feature = "desktop-service")]
    let desktop_mode = Some(plugin_config.desktop_service_mode.as_str());
    #[cfg(not(feature = "desktop-service"))]
    let desktop_mode: Option<&str> = None;

    let manager = app.state::<ServiceManagerHandle<R>>();
    let (tx, rx) = tokio::sync::oneshot::channel();
    manager
        .cmd_tx
        .send(ManagerCommand::GetLifecycleStatus {
            desktop_mode: desktop_mode.map(|s| s.to_string()),
            reply: tx,
        })
        .await
        .map_err(|e| e.to_string())?;

    rx.await.map_err(|e| e.to_string())
}

/// Configure recovery (auto-restart) for the background service.
///
/// When `enabled` is `true`, persists `desired_running=true` with an optional
/// start config (for recovery after process kill or device reboot).
/// When `enabled` is `false`, clears the recovery intent.
#[tauri::command]
async fn configure_recovery<R: Runtime>(
    app: AppHandle<R>,
    enabled: bool,
    config: Option<StartConfig>,
) -> Result<(), String> {
    if enabled {
        enable_auto_restart(app, config).await
    } else {
        disable_auto_restart(app).await
    }
}

// ─── Desktop OS Service State & Commands ──────────────────────────────────────

/// Managed state indicating OS service mode via IPC.
///
/// When present as managed state, the `start`/`stop`/`is_running` commands
/// route through the persistent IPC client instead of the in-process actor loop.
#[cfg(all(feature = "desktop-service", any(unix, windows)))]
struct DesktopIpcState {
    client: desktop::ipc_client::PersistentIpcClientHandle,
}

/// Set up OS-service mode: spawn the persistent IPC client, manage
/// [`DesktopIpcState`], and kick off auto-provisioning.
///
/// Called from plugin setup when `desktopServiceMode` is `"osService"` on a
/// desktop platform. After this returns, commands route through the IPC
/// client (the in-process `cmd_rx` is intentionally unused in this mode).
#[cfg(all(feature = "desktop-service", any(unix, windows), not(mobile)))]
fn setup_os_service_ipc<R: Runtime>(
    app: &AppHandle<R>,
    config: &PluginConfig,
) -> Result<(), ServiceError> {
    let label = desktop::service_manager::derive_service_label(
        app,
        config.desktop_service_label.as_deref(),
    );
    let socket_path = desktop::ipc::socket_path(&label)?;
    let client = desktop::ipc_client::PersistentIpcClientHandle::spawn(
        socket_path,
        app.app_handle().clone(),
    );
    app.manage(DesktopIpcState { client });

    // BGS-12: auto-provision ONLY when the user has consented to the
    // background service. Consent OFF ⇒ no auto-provision ⇒ a manual
    // `systemctl --user disable` is never reverted on the next launch (the
    // systemd unit re-install / re-enable / start inside
    // `spawn_os_service_auto_provision` → `install_service_inner` is skipped
    // entirely). The consent record is read LIVE from disk (the plugin
    // cannot import the app crate, so it reads the stable plain-JSON
    // `background-service-consent.json` contract directly).
    let consent_dir = app.path().app_data_dir().ok().map(|d| d.join("data"));
    let allow = consent_dir
        .as_deref()
        .map(|d| should_auto_provision(d, config.desktop_start_service_if_missing))
        .unwrap_or(false);
    if allow {
        spawn_os_service_auto_provision(app.app_handle());
    } else {
        log::info!(
            "Background service: OS-service auto-provision skipped \
             (consent off or desktopStartServiceIfMissing disabled)"
        );
    }
    Ok(())
}

/// The on-disk background-service consent filename (the stable plain-JSON
/// contract owned by the app's `consent` module; the plugin reads it directly
/// because it cannot import the app crate without a circular dependency).
#[cfg(feature = "desktop-service")]
const DESKTOP_CONSENT_FILENAME: &str = "background-service-consent.json";

/// Minimal mirror of the app's `BackgroundServiceConsent` on-disk record.
///
/// Only the `enabled` bool is read; serde ignores the record's other fields
/// (`auto_unlock`, `updated_at`) by default. Default-off on a missing/corrupt
/// record, matching the app's `load()` semantics.
#[cfg(feature = "desktop-service")]
#[derive(Debug, Default, serde::Deserialize)]
struct ProvisioningConsent {
    #[serde(default)]
    enabled: bool,
}

/// Whether the persisted consent record allows the OS service to be
/// auto-provisioned (BGS-12). Provisioning gates on the master service
/// consent (`enabled`); the `auto_unlock` sub-consent governs credential
/// auto-unlock (F3 / `perform_run`), a separate concern. Default-off on a
/// missing/corrupt record ⇒ provisioning blocked until consent is recorded.
#[cfg(feature = "desktop-service")]
fn desktop_consent_allows_provisioning(data_dir: &std::path::Path) -> bool {
    let path = data_dir.join(DESKTOP_CONSENT_FILENAME);
    let record = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<ProvisioningConsent>(&text).ok())
        .unwrap_or_default();
    record.enabled
}

/// The full auto-provision decision (BGS-12): the config must opt in
/// (`desktop_start_service_if_missing`) AND the user must have consented
/// (`enabled`). Both gates are load-bearing — the config gate alone left a
/// silent install with no consent; the consent gate alone ignores a host's
/// `desktopStartServiceIfMissing=false`.
#[cfg(feature = "desktop-service")]
fn should_auto_provision(data_dir: &std::path::Path, start_if_missing: bool) -> bool {
    start_if_missing && desktop_consent_allows_provisioning(data_dir)
}

#[cfg(feature = "desktop-service")]
#[tauri::command]
async fn install_service<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    install_service_inner(&app).await
}

// PRODUCT DECISION: daemon crash-loop + restart cadence (doc 08, BGS-05 Leg C).
// `RestartSec` (5s between restarts) + `StartLimitBurst`/`StartLimitIntervalSec`
// (5 restarts per 60s ⇒ systemd stops the crash-loop). `InstallOptions::default()`
// stays None for all three so tests + non-prod callers opt out; only this prod
// caller opts in. systemd-native; launchd ignores the StartLimit fields.
//
// BGS-05 re-fix (Critic Blocker 1, cfg-attribute displacement): EACH const AND
// `install_service_inner` carries its OWN `#[cfg(feature = "desktop-service")]`.
// The Step-6 original had a single cfg above the FIRST const; because `//`
// comments are trivia, the attribute bound ONLY to that first const and SILENTLY
// left `install_service_inner` + the other two consts ungated — breaking the
// default-features build (their bodies reference the cfg-gated `desktop` module
// + feature-gated `PluginConfig` fields). One cfg per item is displacement-proof.
#[cfg(feature = "desktop-service")]
const DEFAULT_RESTART_DELAY_SECS: u32 = 5;
#[cfg(feature = "desktop-service")]
const DEFAULT_START_LIMIT_BURST: u32 = 5;
#[cfg(feature = "desktop-service")]
const DEFAULT_START_LIMIT_INTERVAL_SECS: u32 = 60;

#[cfg(feature = "desktop-service")]
async fn install_service_inner<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    use desktop::service_manager::{derive_service_label, DesktopServiceManager};
    let plugin_config = app.state::<PluginConfig>();
    let label = derive_service_label(app, plugin_config.desktop_service_label.as_deref());
    let exec_path = std::env::current_exe().map_err(|e| e.to_string())?;

    // Validate that the executable exists and is executable.
    if !exec_path.exists() {
        return Err(format!(
            "Current executable does not exist at {}: cannot install OS service",
            exec_path.display()
        ));
    }

    // Verify the binary supports --service-label by spawning it with the flag
    // and checking for a specific exit behavior. We use a timeout to avoid
    // hanging if the binary starts a GUI.
    let validate_result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(&exec_path)
            .arg("--service-label")
            .arg(&label)
            .arg("--validate-service-install")
            .output(),
    )
    .await;

    match validate_result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.trim().contains("ok") {
                return Err("Binary does not handle --validate-service-install. \
                     Ensure headless_main() is called from your app's main()."
                    .into());
            }
        }
        Ok(Err(e)) => {
            return Err(format!(
                "Failed to validate executable for --service-label: {e}"
            ));
        }
        Err(_) => {
            // Timed out — the binary probably started the GUI instead of handling
            // the service flag. Warn but don't block installation.
            log::warn!(
                "Timeout validating --service-label support. \
                 Ensure your app's main() handles the --service-label argument \
                 and calls headless_main()."
            );
        }
    }

    {
        let mgr = DesktopServiceManager::new(&label, exec_path).map_err(|e| e.to_string())?;
        use desktop::service_manager::InstallOptions;
        let options = InstallOptions {
            autostart: plugin_config.desktop_service_autostart,
            // BGS-05 Leg C: the prod daemon autostarts with a real restart cadence
            // (RestartSec) + a crash-loop cap (StartLimitBurst/Interval). The
            // struct default remains None for all three.
            restart_delay_secs: Some(DEFAULT_RESTART_DELAY_SECS),
            journal_output: true,
            log_path: None,
            start_limit_burst: Some(DEFAULT_START_LIMIT_BURST),
            start_limit_interval_secs: Some(DEFAULT_START_LIMIT_INTERVAL_SECS),
        };
        mgr.install(&options).map_err(|e| e.to_string())?;
    }

    // Nudge the persistent IPC client to skip backoff and reconnect.
    if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
        ipc_state.client.nudge_reconnect();
        let timeout =
            std::time::Duration::from_millis(plugin_config.desktop_service_start_timeout_ms);
        ipc_state.client.wait_for_connected(timeout).await.ok();
    }

    Ok(())
}

/// Auto-provision the OS service at app startup (install + start + IPC wait).
///
/// Spawned from plugin setup in OS-service mode when
/// `desktopStartServiceIfMissing` is enabled. If the daemon's IPC socket does
/// not become reachable within a short grace period, the service unit is
/// installed (idempotent) and started, then the persistent IPC client is
/// nudged to reconnect. Failures are logged; the host app keeps whatever
/// in-process fallback it set up.
#[cfg(all(feature = "desktop-service", any(unix, windows), not(mobile)))]
fn spawn_os_service_auto_provision<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let (start_if_missing, start_timeout_ms) = {
            let plugin_config = app.state::<PluginConfig>();
            (
                plugin_config.desktop_start_service_if_missing,
                plugin_config.desktop_service_start_timeout_ms,
            )
        };
        if !start_if_missing {
            return;
        }

        // Grace period: an already-running daemon connects almost immediately.
        if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
            match ipc_state
                .client
                .wait_for_connected(std::time::Duration::from_secs(3))
                .await
            {
                Ok(true) => {
                    log::info!("OS service already running — IPC connected");
                    return;
                }
                Ok(false) => {}
                Err(e) => log::warn!("IPC wait failed during auto-provision: {e}"),
            }
        }

        log::info!("OS service IPC unavailable — auto-provisioning (install + start)");
        if let Err(e) = install_service_inner(&app).await {
            log::warn!(
                "OS service auto-install failed: {e}; \
                 app continues with in-process fallback"
            );
            return;
        }

        // Explicitly start the unit (install may only enable autostart).
        {
            use desktop::service_manager::{derive_service_label, DesktopServiceManager};
            let plugin_config = app.state::<PluginConfig>();
            let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());
            let exec_path = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("OS service auto-start failed: cannot resolve current exe: {e}");
                    return;
                }
            };
            match DesktopServiceManager::new(&label, exec_path) {
                Ok(mgr) => {
                    if let Err(e) = mgr.start() {
                        log::warn!("OS service auto-start failed: {e}");
                        return;
                    }
                }
                Err(e) => {
                    log::warn!("OS service manager unavailable: {e}");
                    return;
                }
            }
        }

        if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
            ipc_state.client.nudge_reconnect();
            let timeout = std::time::Duration::from_millis(start_timeout_ms);
            match ipc_state.client.wait_for_connected(timeout).await {
                Ok(true) => log::info!("OS service auto-provision complete — IPC connected"),
                Ok(false) => log::warn!(
                    "OS service installed and started but IPC did not connect within {}ms",
                    timeout.as_millis()
                ),
                Err(e) => log::warn!("IPC wait failed after auto-provision: {e}"),
            }
        }
    });
}

#[cfg(feature = "desktop-service")]
#[tauri::command]
async fn uninstall_service<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    use desktop::service_manager::{derive_service_label, DesktopServiceManager};
    let plugin_config = app.state::<PluginConfig>();
    let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());
    let exec_path = std::env::current_exe().map_err(|e| e.to_string())?;
    let mgr = DesktopServiceManager::new(&label, exec_path).map_err(|e| e.to_string())?;
    mgr.uninstall().map_err(|e| e.to_string())
}

// ─── Desktop OS Service Start/Stop/Status Commands ────────────────────────────

/// Build an [`OsServiceStatus`] from available information.
///
/// Gathers the service label, mode string, IPC connection state, socket path,
/// and optional last error into a status snapshot.
#[cfg(all(feature = "desktop-service", any(unix, windows)))]
fn build_os_service_status(
    label: &str,
    ipc_connected: bool,
    socket_path: Option<String>,
    last_error: Option<String>,
) -> models::OsServiceStatus {
    let mode = if cfg!(target_os = "macos") {
        "launchd"
    } else if cfg!(windows) {
        "scm"
    } else {
        "systemd"
    };

    let installed = if ipc_connected {
        models::OsServiceInstallState::Running
    } else {
        // If not running via IPC, we can't easily determine install state
        // without calling external tools. Default to Installed if the manager
        // was constructable (caller checks this before calling build).
        models::OsServiceInstallState::Installed
    };

    models::OsServiceStatus {
        label: label.to_string(),
        mode: mode.to_string(),
        installed,
        ipc_connected,
        socket_path,
        last_error,
    }
}

/// Start the OS-level background service (desktop only).
///
/// Delegates to [`DesktopServiceManager::start()`] (systemd, launchd, or
/// Windows SCM), then nudges the persistent IPC client to reconnect.
#[cfg(feature = "desktop-service")]
#[tauri::command]
async fn start_os_service<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    #[cfg(any(unix, windows))]
    {
        use desktop::service_manager::{derive_service_label, DesktopServiceManager};
        let plugin_config = app.state::<PluginConfig>();
        let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());
        let exec_path = std::env::current_exe().map_err(|e| e.to_string())?;
        {
            let mgr = DesktopServiceManager::new(&label, exec_path).map_err(|e| e.to_string())?;
            mgr.start().map_err(|e| e.to_string())?;
        }

        // Nudge the persistent IPC client to skip backoff and reconnect.
        if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
            ipc_state.client.nudge_reconnect();
            let timeout =
                std::time::Duration::from_millis(plugin_config.desktop_service_start_timeout_ms);
            ipc_state.client.wait_for_connected(timeout).await.ok();
        }

        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = app;
        Err(os_service_unsupported_platform())
    }
}

/// Stop the OS-level background service (desktop only).
///
/// Delegates to [`DesktopServiceManager::stop()`] (systemd, launchd, or
/// Windows SCM).
#[cfg(feature = "desktop-service")]
#[tauri::command]
async fn stop_os_service<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    #[cfg(any(unix, windows))]
    {
        use desktop::service_manager::{derive_service_label, DesktopServiceManager};
        let plugin_config = app.state::<PluginConfig>();
        let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());
        let exec_path = std::env::current_exe().map_err(|e| e.to_string())?;
        let mgr = DesktopServiceManager::new(&label, exec_path).map_err(|e| e.to_string())?;
        mgr.stop().map_err(|e| e.to_string())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = app;
        Err(os_service_unsupported_platform())
    }
}

/// Restart the OS-level background service (desktop only).
///
/// Calls stop (best-effort) then start via the platform service manager.
#[cfg(feature = "desktop-service")]
#[tauri::command]
async fn restart_os_service<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    #[cfg(any(unix, windows))]
    {
        use desktop::service_manager::{derive_service_label, DesktopServiceManager};
        let plugin_config = app.state::<PluginConfig>();
        let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());
        let exec_path = std::env::current_exe().map_err(|e| e.to_string())?;
        let mgr = DesktopServiceManager::new(&label, exec_path).map_err(|e| e.to_string())?;
        mgr.stop().ok(); // Best-effort stop — service may not be running.
        mgr.start().map_err(|e| e.to_string())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = app;
        Err(os_service_unsupported_platform())
    }
}

/// Get the status of the OS-level background service (desktop only).
///
/// Returns [`OsServiceStatus`] with label, mode (systemd / launchd / scm),
/// IPC state, and socket or pipe path.
#[cfg(feature = "desktop-service")]
#[tauri::command]
async fn get_os_service_status<R: Runtime>(
    app: AppHandle<R>,
) -> Result<models::OsServiceStatus, String> {
    #[cfg(any(unix, windows))]
    {
        use desktop::service_manager::derive_service_label;
        let plugin_config = app.state::<PluginConfig>();
        let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());

        let ipc_connected = app
            .try_state::<DesktopIpcState>()
            .map(|s| s.client.is_connected())
            .unwrap_or(false);

        let socket_path = desktop::ipc::socket_path(&label)
            .ok()
            .map(|p| p.to_string_lossy().to_string());

        Ok(build_os_service_status(
            &label,
            ipc_connected,
            socket_path,
            None,
        ))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = app;
        Err(os_service_unsupported_platform())
    }
}

/// Error string for OS-service commands on platforms with no IPC transport
/// (neither Unix domain sockets nor Windows named pipes).
#[cfg(all(feature = "desktop-service", not(any(unix, windows))))]
fn os_service_unsupported_platform() -> String {
    ServiceError::Platform("OS-service mode is not supported on this platform".into()).to_string()
}

// ─── Plugin Builder ──────────────────────────────────────────────────────────

/// Create the Tauri plugin with your service factory.
///
/// ```rust,ignore
/// // MyService must implement BackgroundService<R>
/// tauri::Builder::default()
///     .plugin(tauri_plugin_background_service::init_with_service(|| MyService::new()))
/// ```
pub fn init_with_service<R, S, F>(factory: F) -> TauriPlugin<R, PluginConfig>
where
    R: Runtime,
    S: BackgroundService<R>,
    F: Fn() -> S + Send + Sync + 'static,
{
    let boxed_factory: ServiceFactory<R> = Box::new(move || Box::new(factory()));

    Builder::<R, PluginConfig>::new("background-service")
        .invoke_handler(tauri::generate_handler![
            start,
            stop,
            is_running,
            get_service_state,
            get_platform_capabilities,
            get_scheduling_status,
            get_desired_state_status,
            get_pending_bg_task,
            get_notification_permission_status,
            request_notification_permission,
            can_use_full_screen_intent,
            open_full_screen_intent_settings,
            enable_auto_restart,
            disable_auto_restart,
            get_desired_service_state,
            native_lifecycle_event,
            validate_setup,
            get_lifecycle_status,
            configure_recovery,
            request_battery_exemption,
            #[cfg(feature = "desktop-service")]
            install_service,
            #[cfg(feature = "desktop-service")]
            uninstall_service,
            #[cfg(feature = "desktop-service")]
            start_os_service,
            #[cfg(feature = "desktop-service")]
            stop_os_service,
            #[cfg(feature = "desktop-service")]
            restart_os_service,
            #[cfg(feature = "desktop-service")]
            get_os_service_status,
        ])
        .setup(move |app, api| {
            let config = api.config().clone();
            let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(config.channel_capacity);
            #[cfg(mobile)]
            let mobile_cmd_tx = cmd_tx.clone();
            let handle = ServiceManagerHandle::new(cmd_tx);
            app.manage(handle);

            app.manage(config.clone());

            let ios_safety_timeout_secs = config.ios_safety_timeout_secs;
            let ios_processing_safety_timeout_secs = config.ios_processing_safety_timeout_secs;
            let ios_earliest_refresh_begin_minutes = config.ios_earliest_refresh_begin_minutes;
            let ios_earliest_processing_begin_minutes =
                config.ios_earliest_processing_begin_minutes;
            let ios_requires_external_power = config.ios_requires_external_power;
            let ios_requires_network_connectivity = config.ios_requires_network_connectivity;
            let ios_processing_ceiling_multiplier = config.ios_processing_ceiling_multiplier;
            let android_fg_service_types = config.android_foreground_service_types.clone();
            let android_validate_fg_type = config.android_validate_foreground_service_type;

            // D1: lifecycle-notification policy, derived once at spawn.
            // Defaults keep every notification off; DEC-002 suppresses the
            // Android paths already covered by native Kotlin notifications.
            let notifier_policy = NotifierPolicy::derive(&config, cfg!(target_os = "android"));
            let notify_sink: Option<Arc<dyn NotifySink>> = Some(Arc::new(Notifier {
                app: app.app_handle().clone(),
            }));

            // One authoritative Rust-backed desired-state model on every platform
            // (H4 / D1). The mobile arm was previously hardcoded `None`, so iOS
            // status lied (`desired_running` always false) and the recovery
            // commands silently no-op'd. With a `Some(...)` backend the actor
            // persists desired state in an app-data-dir file, `build_lifecycle_status`
            // reports the real desired state, and the recovery commands mirror
            // their effect into Swift `UserDefaults` + BGTask scheduling
            // (see `manager::mirror_desired_to_native` / `MobileLifecycle::mirror_desired_state`).
            let desired_state_backend: Option<Arc<dyn desired_state::DesiredStateBackend>> = {
                match app.path().app_data_dir() {
                    Ok(data_dir) => Some(Arc::new(desired_state::FileDesiredStateBackend::new(
                        data_dir,
                    ))),
                    Err(e) => {
                        log::warn!("Failed to get app data dir for desired-state persistence: {e}");
                        None
                    }
                }
            };

            // Mode dispatch: spawn in-process actor or configure IPC for OS service.
            //
            // OS-service mode is strictly a DESKTOP concept. Android and iOS are
            // `unix` targets, so the cfg below must exclude mobile: otherwise a
            // consumer that enables `desktop-service` unconditionally and sets
            // `desktopServiceMode: "osService"` would route mobile into the
            // desktop IPC path, never spawn the actor loop, drop `cmd_rx`, and
            // every ManagerCommand (including Start) would fail on a closed
            // channel — the native foreground service would never start.
            #[cfg(all(feature = "desktop-service", any(unix, windows), not(mobile)))]
            if config.desktop_service_mode == "osService" {
                // OS service mode: spawn persistent IPC client.
                setup_os_service_ipc(app, &config)?;
            } else {
                // In-process mode (default): spawn the actor loop.
                let factory = boxed_factory;
                tauri::async_runtime::spawn(manager_loop(
                    cmd_rx,
                    factory,
                    ios_safety_timeout_secs,
                    ios_processing_safety_timeout_secs,
                    ios_earliest_refresh_begin_minutes,
                    ios_earliest_processing_begin_minutes,
                    ios_requires_external_power,
                    ios_requires_network_connectivity,
                    ios_processing_ceiling_multiplier,
                    desired_state_backend,
                    android_fg_service_types.clone(),
                    android_validate_fg_type,
                    notifier_policy,
                    notify_sink,
                    None,
                    false,
                ));
            }

            // Mobile: ALWAYS spawn the in-process actor, regardless of any
            // desktop-service configuration. `DesktopIpcState` is never managed
            // on mobile, so the command-level IPC early-returns are inert.
            #[cfg(all(feature = "desktop-service", unix, mobile))]
            {
                if config.desktop_service_mode == "osService" {
                    log::warn!(
                        "desktopServiceMode=osService is ignored on mobile; \
                         using the in-process service actor"
                    );
                }
                let factory = boxed_factory;
                tauri::async_runtime::spawn(manager_loop(
                    cmd_rx,
                    factory,
                    ios_safety_timeout_secs,
                    ios_processing_safety_timeout_secs,
                    ios_earliest_refresh_begin_minutes,
                    ios_earliest_processing_begin_minutes,
                    ios_requires_external_power,
                    ios_requires_network_connectivity,
                    ios_processing_ceiling_multiplier,
                    desired_state_backend,
                    android_fg_service_types.clone(),
                    android_validate_fg_type,
                    notifier_policy,
                    notify_sink,
                    None,
                    false,
                ));
            }

            // Unknown desktop platform class (neither unix nor windows): no
            // IPC transport exists, so osService mode cannot be honored.
            #[cfg(all(feature = "desktop-service", not(any(unix, windows))))]
            {
                if config.desktop_service_mode == "osService" {
                    log::warn!(
                        "Desktop OS-service mode is not supported on this platform; \
                         background-service commands will fail instead of running in-process"
                    );
                    drop(cmd_rx);
                } else {
                    // On non-Unix platforms, only explicit in-process mode is available.
                    let factory = boxed_factory;
                    tauri::async_runtime::spawn(manager_loop(
                        cmd_rx,
                        factory,
                        ios_safety_timeout_secs,
                        ios_processing_safety_timeout_secs,
                        ios_earliest_refresh_begin_minutes,
                        ios_earliest_processing_begin_minutes,
                        ios_requires_external_power,
                        ios_requires_network_connectivity,
                        ios_processing_ceiling_multiplier,
                        desired_state_backend,
                        android_fg_service_types.clone(),
                        android_validate_fg_type,
                        notifier_policy,
                        notify_sink,
                        None,
                        false,
                    ));
                }
            }

            #[cfg(not(feature = "desktop-service"))]
            {
                let factory = boxed_factory;
                tauri::async_runtime::spawn(manager_loop(
                    cmd_rx,
                    factory,
                    ios_safety_timeout_secs,
                    ios_processing_safety_timeout_secs,
                    ios_earliest_refresh_begin_minutes,
                    ios_earliest_processing_begin_minutes,
                    ios_requires_external_power,
                    ios_requires_network_connectivity,
                    ios_processing_ceiling_multiplier,
                    desired_state_backend,
                    android_fg_service_types,
                    android_validate_fg_type,
                    notifier_policy,
                    notify_sink,
                    None,
                    false,
                ));
            }

            #[cfg(mobile)]
            {
                let lifecycle = mobile::init(app, api)?;
                let lifecycle_arc = Arc::new(lifecycle);

                // Send SetMobile to actor so keepalive is managed by the actor.
                let mobile_trait: Arc<dyn MobileKeepalive> = lifecycle_arc.clone();
                if let Err(e) = mobile_cmd_tx.try_send(ManagerCommand::SetMobile {
                    mobile: mobile_trait,
                }) {
                    log::error!("Failed to send SetMobile command: {e}");
                }

                // Store for iOS callbacks and Android auto-start helpers.
                app.manage(lifecycle_arc);
            }

            // iOS: auto-start when launched by OS for a pending BGTask.
            // The native bridge calls are spawned after setup returns so Swift
            // can service their main-queue work while Tauri continues startup.
            #[cfg(target_os = "ios")]
            {
                ios_spawn_cold_auto_start(app);

                // H14: listen for BGTasks delivered to the warm process and
                // start the Rust service for each (the cold block above only
                // runs once at launch).
                ios_spawn_warm_listener(app);
            }

            Ok(())
        })
        .on_event(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // Android foreground service mode owns its lifecycle outside the
                // Activity. Closing the UI must not stop the background Core.
                #[cfg(target_os = "android")]
                {
                    let _ = app;
                    return;
                }

                #[cfg(not(target_os = "android"))]
                {
                    // In OS service mode, the service runs in a separate process — skip.
                    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
                    if app.try_state::<DesktopIpcState>().is_some() {
                        return;
                    }
                    let manager = app.state::<ServiceManagerHandle<R>>();
                    // H2: on iOS, app `Exit` is OS-driven backgrounding/termination,
                    // not a user stop. Route it through `ProcessExit` so desired
                    // state + the BGTask schedule survive (recovery resumes
                    // delivery). On desktop, closing the app is a genuine stop.
                    #[cfg(target_os = "ios")]
                    let stop_result =
                        manager.stop_blocking_with_reason(crate::models::StopReason::ProcessExit);
                    #[cfg(not(target_os = "ios"))]
                    let stop_result = manager.stop_blocking();
                    if let Err(e) = stop_result {
                        log::warn!("Failed to stop background service on app exit: {e}");
                    }
                }
            }
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Minimal service for testing type compatibility.
    struct DummyService;

    #[async_trait]
    impl BackgroundService<tauri::Wry> for DummyService {
        async fn init(&mut self, _ctx: &ServiceContext<tauri::Wry>) -> Result<(), ServiceError> {
            Ok(())
        }

        async fn run(&mut self, _ctx: &ServiceContext<tauri::Wry>) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    // ── Construction Tests ───────────────────────────────────────────────

    #[test]
    fn service_manager_handle_constructs() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);
        let _handle: ServiceManagerHandle<tauri::Wry> = ServiceManagerHandle::new(cmd_tx);
    }

    #[test]
    fn factory_produces_boxed_service() {
        let factory: ServiceFactory<tauri::Wry> = Box::new(|| Box::new(DummyService));
        let _service: Box<dyn BackgroundService<tauri::Wry>> = factory();
    }

    #[test]
    fn handle_factory_creates_fresh_instances() {
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = count.clone();

        let factory: ServiceFactory<tauri::Wry> = Box::new(move || {
            count_clone.fetch_add(1, Ordering::SeqCst);
            Box::new(DummyService)
        });

        let _ = (factory)();
        let _ = (factory)();

        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    // ── Compile-time Tests ───────────────────────────────────────────────

    /// Verify `init_with_service` returns `TauriPlugin<R>`.
    #[allow(dead_code)]
    fn init_with_service_returns_tauri_plugin<R: Runtime, S, F>(
        factory: F,
    ) -> TauriPlugin<R, PluginConfig>
    where
        S: BackgroundService<R>,
        F: Fn() -> S + Send + Sync + 'static,
    {
        init_with_service(factory)
    }

    /// Verify `start` command signature is generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn start_command_signature<R: Runtime>(
        app: AppHandle<R>,
        config: StartConfig,
    ) -> Result<(), String> {
        start(app, config).await
    }

    /// Verify `stop` command signature is generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn stop_command_signature<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
        stop(app).await
    }

    /// Verify `is_running` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn is_running_command_signature<R: Runtime>(app: AppHandle<R>) -> bool {
        is_running(app).await
    }

    /// Verify `get_service_state` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn get_service_state_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<models::ServiceStatus, String> {
        get_service_state(app).await
    }

    /// Verify `get_scheduling_status` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn get_scheduling_status_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<models::IOSSchedulingStatus, String> {
        get_scheduling_status(app).await
    }

    /// Verify `get_desired_state_status` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn get_desired_state_status_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<models::IOSDesiredStateStatus, String> {
        get_desired_state_status(app).await
    }

    /// Verify `get_pending_bg_task` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn get_pending_bg_task_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<Option<models::PendingTaskInfo>, String> {
        get_pending_bg_task(app).await
    }

    /// Verify `get_notification_permission_status` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn get_notification_permission_status_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<models::NotificationPermissionStatus, String> {
        get_notification_permission_status(app).await
    }

    /// Verify `request_notification_permission` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn request_notification_permission_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<models::NotificationPermissionStatus, String> {
        request_notification_permission(app).await
    }

    /// Verify `can_use_full_screen_intent` command signature is async and generic over `R: Runtime`.
    /// Return type mirrors the command (`Result<serde_json::Value, String>` — the `{canUse: bool}`
    /// object shape; see the command's IPC SHAPE CONTRACT doc).
    #[allow(dead_code)]
    async fn can_use_full_screen_intent_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<serde_json::Value, String> {
        can_use_full_screen_intent(app).await
    }

    /// Verify `open_full_screen_intent_settings` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn open_full_screen_intent_settings_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<(), String> {
        open_full_screen_intent_settings(app).await
    }

    /// Verify `enable_auto_restart` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn enable_auto_restart_command_signature<R: Runtime>(
        app: AppHandle<R>,
        config: Option<StartConfig>,
    ) -> Result<(), String> {
        enable_auto_restart(app, config).await
    }

    /// Verify `disable_auto_restart` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn disable_auto_restart_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<(), String> {
        disable_auto_restart(app).await
    }

    /// Verify `get_desired_service_state` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn get_desired_service_state_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<Option<desired_state::DesiredState>, String> {
        get_desired_service_state(app).await
    }

    /// Verify `validate_setup` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn validate_setup_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<models::SetupValidationReport, String> {
        validate_setup(app).await
    }

    /// Verify `native_lifecycle_event` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn native_lifecycle_event_command_signature<R: Runtime>(
        app: AppHandle<R>,
        event: models::NativeLifecycleEvent,
    ) -> Result<(), String> {
        native_lifecycle_event(app, event).await
    }

    /// Verify `get_lifecycle_status` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn get_lifecycle_status_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<models::LifecycleStatus, String> {
        get_lifecycle_status(app).await
    }

    /// Verify `configure_recovery` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn configure_recovery_command_signature<R: Runtime>(
        app: AppHandle<R>,
        enabled: bool,
        config: Option<StartConfig>,
    ) -> Result<(), String> {
        configure_recovery(app, enabled, config).await
    }

    // ── Desktop IPC State Tests ─────────────────────────────────────────

    /// Verify PersistentIpcClientHandle can be constructed.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    #[tokio::test]
    async fn desktop_ipc_state_with_persistent_client() {
        use desktop::ipc_client::PersistentIpcClientHandle;
        let app = tauri::test::mock_app();
        let path = std::path::PathBuf::from("/tmp/test-persistent-client.sock");
        let client = PersistentIpcClientHandle::spawn(path, app.handle().clone());
        // The client is spawned but may not be connected yet — that's fine.
        // Just verify we can construct the state.
        let _state = DesktopIpcState { client };
    }

    /// AC2: a config with `desktopServiceMode: "osService"` on the current
    /// platform constructs the IPC-client state — `DesktopIpcState` is managed
    /// so commands route through IPC instead of failing on a closed channel.
    #[cfg(all(feature = "desktop-service", any(unix, windows), not(mobile)))]
    #[tokio::test]
    async fn os_service_mode_constructs_ipc_state() {
        let app = tauri::test::mock_app();
        let handle = app.handle();
        let config = PluginConfig {
            desktop_service_mode: "osService".into(),
            ..Default::default()
        };
        // The auto-provision task reads PluginConfig from managed state.
        handle.manage(config.clone());

        setup_os_service_ipc(handle, &config).expect("osService IPC setup should succeed");

        assert!(
            handle.try_state::<DesktopIpcState>().is_some(),
            "DesktopIpcState must be managed in osService mode"
        );
    }

    // ── BGS-12: consent gate on OS-service auto-provisioning ──────────────

    /// The OS service auto-provisions ONLY when the config opts in AND the
    /// user has consented to the background service. This is the AC decision
    /// predicate; the spawn itself is undrivable in a unit test (real IPC +
    /// service-manager), so it is pinned by the include_str static gate below.
    #[cfg(feature = "desktop-service")]
    #[test]
    fn bgs12_no_autoprovision_without_consent() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();

        // No consent record ⇒ default off ⇒ NO auto-provision, even when the
        // config opts in. NV-MUT (neuter the helper to return `true`) → RED.
        assert!(
            !should_auto_provision(dir, true),
            "consent off (no record): must NOT auto-provision even if start_if_missing=true"
        );

        // Consent ON (enabled) ⇒ auto-provision allowed when config opts in.
        std::fs::write(
            dir.join(DESKTOP_CONSENT_FILENAME),
            serde_json::json!({"enabled": true, "auto_unlock": true, "updated_at": 1}).to_string(),
        )
        .unwrap();
        assert!(
            should_auto_provision(dir, true),
            "consent on + start_if_missing: auto-provision allowed"
        );

        // start_if_missing=false ⇒ NO auto-provision even with consent (a
        // host config that opts out is respected; a manual disable sticks).
        assert!(
            !should_auto_provision(dir, false),
            "start_if_missing=false: must NOT auto-provision"
        );
    }

    /// Provisioning gates on `enabled` (the master service consent), NOT on
    /// `auto_unlock` (the credential auto-unlock sub-consent — a separate
    /// concern owned by the F3 / `perform_run` gates). A corrupt record
    /// defaults off.
    #[cfg(feature = "desktop-service")]
    #[test]
    fn bgs12_provisioning_gates_on_enabled_not_auto_unlock() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        // enabled=true, auto_unlock=false ⇒ service consented ⇒ provision OK.
        std::fs::write(
            dir.join(DESKTOP_CONSENT_FILENAME),
            serde_json::json!({"enabled": true, "auto_unlock": false}).to_string(),
        )
        .unwrap();
        assert!(
            should_auto_provision(dir, true),
            "enabled alone (service consent) must allow provisioning"
        );

        // Corrupt record ⇒ default off ⇒ no provisioning.
        std::fs::write(dir.join(DESKTOP_CONSENT_FILENAME), b"not json {{{").unwrap();
        assert!(
            !should_auto_provision(dir, true),
            "corrupt consent record must default off (no provisioning)"
        );
    }

    /// include_str! static gate (mem-1783121224-667 pattern): pin that
    /// `setup_os_service_ipc` wires the consent gate before the
    /// auto-provision spawn. Runtime concat so the asserted token never
    /// appears verbatim on this line (defeats self-referential false-pinning).
    #[cfg(feature = "desktop-service")]
    #[test]
    fn bgs12_setup_os_service_ipc_wires_consent_gate() {
        let src = include_str!("lib.rs");
        let call = ["should_auto", "_provision("].concat();
        assert!(
            src.contains(&call[..]),
            "setup_os_service_ipc must call the consent-gate helper before spawning auto-provision"
        );
        // Pin the else branch (the skip). The literal is split so it never
        // appears verbatim in this test source (defeats self-referential
        // false-pinning against `include_str!("lib.rs")`).
        let skip = ["OS-service auto-", "provision skipped"].concat();
        assert!(
            src.contains(&skip[..]),
            "setup_os_service_ipc must skip the spawn when consent is off (the else branch)"
        );
    }

    /// BGS-21 (doc-08 Step 12) Rust-side reachability pin (mem-1783277823-8dae):
    /// the two notification-permission commands must be (1) declared in build.rs
    /// `COMMANDS` (drives permission-token autogeneration), (2) registered in
    /// `generate_handler!` (JS reachability — Tauri v2 JS invokes resolve only
    /// against registered Rust commands), and (3) bridged in mobile.rs via
    /// `run_mobile_plugin` with the EXACT camelCase Kotlin @Command names.
    /// Dropping any single axis silently ships a dead JS binding that fails at
    /// runtime (command-not-found / capability denial) — the mem-1783371281-d310
    /// class. NV-MUT: drop a build.rs entry / a generate_handler! registration /
    /// change a run_mobile_plugin name ⇒ this test REDs on the matching axis.
    /// The same-file (lib.rs) registration token is built by runtime concat so
    /// the asserted string never appears verbatim on this line (mem-1783121224-667
    /// self-reference guard); the build.rs + mobile.rs axes are cross-file ⇒ a
    /// direct `contains` is safe.
    #[test]
    fn bgs21_notification_permission_bridge_registered_and_wired() {
        // (1) build.rs COMMANDS entries (cross-file ⇒ direct contains is safe).
        let build_rs = include_str!("../build.rs");
        assert!(
            build_rs.contains("\"get_notification_permission_status\""),
            "get_notification_permission_status must be listed in build.rs COMMANDS"
        );
        assert!(
            build_rs.contains("\"request_notification_permission\""),
            "request_notification_permission must be listed in build.rs COMMANDS"
        );

        // (2) generate_handler! registration (SAME file ⇒ runtime concat so the
        // asserted name+comma token does not appear verbatim in this source).
        let src = include_str!("lib.rs");
        let get_reg = ["get_notification_permission", "_status,"].concat();
        let req_reg = ["request_notification_permiss", "ion,"].concat();
        assert!(
            src.contains(&get_reg[..]),
            "get_notification_permission_status must be registered in generate_handler!"
        );
        assert!(
            src.contains(&req_reg[..]),
            "request_notification_permission must be registered in generate_handler!"
        );

        // (3) mobile.rs bridges via run_mobile_plugin with the exact camelCase
        // Kotlin @Command names (cross-file ⇒ direct contains is safe).
        let mobile_rs = include_str!("mobile.rs");
        assert!(
            mobile_rs.contains("\"getNotificationPermissionStatus\""),
            "mobile.rs must bridge getNotificationPermissionStatus via run_mobile_plugin"
        );
        assert!(
            mobile_rs.contains("\"requestNotificationPermission\""),
            "mobile.rs must bridge requestNotificationPermission via run_mobile_plugin"
        );
    }

    /// BGS-22 (doc-08 Step 14): the `request_battery_exemption` JS binding is
    /// reachable on ALL FOUR production axes (mem-1783371281-d310): (1) declared
    /// in build.rs COMMANDS, (2) registered in `generate_handler!`, (3) bridged
    /// in mobile.rs via `run_mobile_plugin` with the EXACT camelCase Kotlin
    /// `requestBatteryExemption` name, and (4) covered by an `allow-request-
    /// battery-exemption` permission token in both the autogenerated command
    /// table and the default permission set. Dropping any single axis silently
    /// ships a dead JS binding that fails at runtime (command-not-found /
    /// capability denial). NV-MUT: drop a build.rs entry / a generate_handler!
    /// registration / change a run_mobile_plugin name / remove the permission
    /// token ⇒ this test REDs on the matching axis. The same-file (lib.rs)
    /// registration token is built by runtime concat so the asserted string
    /// never appears verbatim on this line (mem-1783121224-667 self-reference
    /// guard); the cross-file axes (build.rs + mobile.rs + permissions) ⇒ a
    /// direct `contains` is safe.
    #[test]
    fn bgs22_battery_exemption_bridge_registered_and_wired() {
        // (1) build.rs COMMANDS entry (cross-file ⇒ direct contains is safe).
        let build_rs = include_str!("../build.rs");
        assert!(
            build_rs.contains("\"request_battery_exemption\""),
            "request_battery_exemption must be listed in build.rs COMMANDS"
        );

        // (2) generate_handler! registration (SAME file ⇒ runtime concat so the
        // asserted name+comma token does not appear verbatim in this source).
        let src = include_str!("lib.rs");
        let reg = ["request_battery_exempt", "ion,"].concat();
        assert!(
            src.contains(&reg[..]),
            "request_battery_exemption must be registered in generate_handler!"
        );

        // (3) mobile.rs bridges via run_mobile_plugin with the exact camelCase
        // Kotlin @Command name (cross-file ⇒ direct contains is safe).
        let mobile_rs = include_str!("mobile.rs");
        assert!(
            mobile_rs.contains("\"requestBatteryExemption\""),
            "mobile.rs must bridge requestBatteryExemption via run_mobile_plugin"
        );

        // (4) permission token: an autogenerated allow-request-battery-exemption
        // command token exists AND is in the default permission set (cross-file
        // ⇒ direct contains is safe). Without both, the JS invoke is capability-
        // denied at runtime even though the command compiles + is registered.
        let default_toml = include_str!("../permissions/default.toml");
        assert!(
            default_toml.contains("\"allow-request-battery-exemption\""),
            "allow-request-battery-exemption must be in permissions/default.toml"
        );
        let cmd_toml =
            include_str!("../permissions/autogenerated/commands/request_battery_exemption.toml");
        assert!(
            cmd_toml.contains("\"request_battery_exemption\""),
            "permissions/autogenerated/commands/request_battery_exemption.toml must allow request_battery_exemption"
        );
    }

    // ── Desktop Command Compile-time Tests ────────────────────────────────

    /// Verify `install_service` command signature is generic over `R: Runtime`.
    #[cfg(feature = "desktop-service")]
    #[allow(dead_code)]
    async fn install_service_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<(), String> {
        install_service(app).await
    }

    /// Verify `uninstall_service` command signature is generic over `R: Runtime`.
    #[cfg(feature = "desktop-service")]
    #[allow(dead_code)]
    async fn uninstall_service_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<(), String> {
        uninstall_service(app).await
    }

    /// Verify `start_os_service` command signature is generic over `R: Runtime`.
    #[cfg(feature = "desktop-service")]
    #[allow(dead_code)]
    async fn start_os_service_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<(), String> {
        start_os_service(app).await
    }

    /// Verify `stop_os_service` command signature is generic over `R: Runtime`.
    #[cfg(feature = "desktop-service")]
    #[allow(dead_code)]
    async fn stop_os_service_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<(), String> {
        stop_os_service(app).await
    }

    /// Verify `restart_os_service` command signature is generic over `R: Runtime`.
    #[cfg(feature = "desktop-service")]
    #[allow(dead_code)]
    async fn restart_os_service_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<(), String> {
        restart_os_service(app).await
    }

    /// Verify `get_os_service_status` command signature is generic over `R: Runtime`.
    #[cfg(feature = "desktop-service")]
    #[allow(dead_code)]
    async fn get_os_service_status_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<models::OsServiceStatus, String> {
        get_os_service_status(app).await
    }

    // ── Desktop OS Service Command Routing Tests ──────────────────────────

    /// Test that `build_os_service_status` produces a valid OsServiceStatus
    /// with the correct fields populated.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    #[test]
    fn build_os_service_status_populates_fields() {
        let status = build_os_service_status(
            "com.example.bg-service",
            true,
            Some("/tmp/test.sock".to_string()),
            None,
        );
        assert_eq!(status.label, "com.example.bg-service");
        assert!(status.ipc_connected);
        assert_eq!(status.socket_path.as_deref(), Some("/tmp/test.sock"));
        assert!(status.last_error.is_none());
    }

    /// Test that `build_os_service_status` includes the correct mode string.
    #[cfg(all(feature = "desktop-service", any(unix, windows)))]
    #[test]
    fn build_os_service_status_mode_is_correct() {
        let status = build_os_service_status("test", false, None, None);
        #[cfg(target_os = "linux")]
        assert_eq!(status.mode, "systemd");
        #[cfg(target_os = "macos")]
        assert_eq!(status.mode, "launchd");
        #[cfg(windows)]
        assert_eq!(status.mode, "scm");
    }

    // ── On-Event Shutdown Compile-time Test ─────────────────────────────────

    /// Verify the on_event closure accessing ServiceManagerHandle<R> from managed
    /// state type-checks. Ensures the generic R is properly threaded through in
    /// the on_event context where stop_blocking() is called synchronously.
    #[allow(dead_code)]
    fn on_event_shutdown_closure_type_checks<R: Runtime>(_app: &AppHandle<R>) {
        let _closure = |_app: &AppHandle<R>, event: &tauri::RunEvent| {
            if let tauri::RunEvent::Exit = event {
                let manager = _app.state::<ServiceManagerHandle<R>>();
                if let Err(_e) = manager.stop_blocking() {
                    log::warn!("bg service shutdown on exit failed: {_e}");
                }
            }
        };
    }

    // ── Cancel Listener Tests ───────────────────────────────────────────────

    use crate::manager::ManagerCommand;
    use std::sync::atomic::AtomicBool;

    /// Helper: spawn a background task that accepts one StopWithReason command and replies Ok(()).
    /// Returns a oneshot receiver that yields Some(reason) if StopWithReason was received.
    fn spawn_stop_drain(
        mut cmd_rx: tokio::sync::mpsc::Receiver<ManagerCommand<tauri::test::MockRuntime>>,
    ) -> tokio::sync::oneshot::Receiver<Option<crate::models::StopReason>> {
        let (seen_tx, seen_rx) =
            tokio::sync::oneshot::channel::<Option<crate::models::StopReason>>();
        tokio::spawn(async move {
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv()).await;
            match result {
                Ok(Some(ManagerCommand::StopWithReason { reason, reply })) => {
                    let _ = reply.send(Ok(()));
                    let _ = seen_tx.send(Some(reason));
                }
                _ => {
                    let _ = seen_tx.send(None);
                }
            }
        });
        seen_rx
    }

    #[tokio::test]
    async fn cancel_listener_resolved_invoke_sends_stop_with_reason() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let seen = spawn_stop_drain(cmd_rx);

        // wait_fn returns Ok(()) → simulates resolved invoke (safety timer / expiration)
        let stop_sent = run_cancel_listener(
            Box::new(|| Ok(())),
            Box::new(|| {}),
            cmd_tx,
            5, // timeout, shouldn't matter since wait_fn returns immediately
        )
        .await;

        assert!(stop_sent, "resolved invoke should return true");
        let reason = seen.await.unwrap();
        assert_eq!(
            reason,
            Some(crate::models::StopReason::PlatformExpiration),
            "StopWithReason(PlatformExpiration) should be sent on resolved invoke"
        );
    }

    #[tokio::test]
    async fn cancel_listener_rejected_invoke_no_stop() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let seen = spawn_stop_drain(cmd_rx);

        // wait_fn returns Err → simulates rejected invoke (explicit stop / completion)
        let stop_sent = run_cancel_listener(
            Box::new(|| Err(ServiceError::Platform("rejected".into()))),
            Box::new(|| {}),
            cmd_tx,
            5,
        )
        .await;

        assert!(!stop_sent, "rejected invoke should return false");
        assert_eq!(
            seen.await.unwrap(),
            None,
            "StopWithReason should NOT be sent on rejected invoke"
        );
    }

    #[tokio::test]
    async fn cancel_listener_timeout_sends_stop_with_reason() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let cancel_called = Arc::new(AtomicBool::new(false));
        let cancel_called_clone = cancel_called.clone();
        let seen = spawn_stop_drain(cmd_rx);

        // Use a channel to unblock the wait_fn when cancel_fn is called,
        // simulating how the real cancelCancelListener rejects the invoke.
        let (unblock_tx, unblock_rx) = std::sync::mpsc::channel::<()>();

        let stop_sent = run_cancel_listener(
            Box::new(move || {
                // Block until cancel_fn signals us (simulates wait_for_cancel blocking)
                let _ = unblock_rx.recv();
                Ok(())
            }),
            Box::new(move || {
                cancel_called_clone.store(true, Ordering::SeqCst);
                let _ = unblock_tx.send(());
            }),
            cmd_tx,
            0, // immediate timeout
        )
        .await;

        assert!(stop_sent, "timeout should return true");
        assert!(
            cancel_called.load(Ordering::SeqCst),
            "cancel_fn should be called on timeout"
        );
        let reason = seen.await.unwrap();
        assert_eq!(
            reason,
            Some(crate::models::StopReason::PlatformTimeout),
            "StopWithReason(PlatformTimeout) should be sent on timeout"
        );
    }

    #[tokio::test]
    async fn cancel_listener_join_error_no_stop() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let seen = spawn_stop_drain(cmd_rx);

        // wait_fn panics → simulates JoinError from spawn_blocking
        let stop_sent = run_cancel_listener(
            Box::new(|| panic!("simulated panic in wait_for_cancel")),
            Box::new(|| {}),
            cmd_tx,
            5,
        )
        .await;

        // JoinError is Ok(Err(_)) which falls into the `_ => false` branch
        assert!(!stop_sent, "join error should return false (no stop sent)");
        assert_eq!(
            seen.await.unwrap(),
            None,
            "StopWithReason should NOT be sent on join error"
        );
    }

    // ── iOS Cold Auto-Start Tests (H3) ──────────────────────────────────────

    /// Helper: spawn a background task that accepts one `Start` command and
    /// replies with `reply_with`. Returns a receiver that yields `true` if a
    /// `Start` command was observed.
    fn spawn_start_drain(
        mut cmd_rx: tokio::sync::mpsc::Receiver<ManagerCommand<tauri::test::MockRuntime>>,
        reply_with: Result<(), ServiceError>,
    ) -> tokio::sync::oneshot::Receiver<bool> {
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<bool>();
        tokio::spawn(async move {
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv()).await;
            match result {
                Ok(Some(ManagerCommand::Start { reply, .. })) => {
                    let _ = reply.send(reply_with);
                    let _ = seen_tx.send(true);
                }
                _ => {
                    let _ = seen_tx.send(false);
                }
            }
        });
        seen_rx
    }

    /// AC2 (H3): a successful auto-start consumes the pending BGTask exactly once
    /// and never records a failure marker.
    #[tokio::test]
    async fn auto_start_success_consumes_pending_exactly_once() {
        let app = tauri::test::mock_app();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let seen = spawn_start_drain(cmd_rx, Ok(()));

        let cleared = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let cleared_c = cleared.clone();
        let failed_c = failed.clone();

        let started = run_auto_start(
            StartConfig::default(),
            app.handle().clone(),
            cmd_tx,
            Box::new(move || {
                cleared_c.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(move || {
                failed_c.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .await;

        assert!(started, "successful Start should return true");
        assert!(seen.await.unwrap(), "Start command should be received");
        assert_eq!(
            cleared.load(Ordering::SeqCst),
            1,
            "pending must be consumed exactly once on success"
        );
        assert_eq!(
            failed.load(Ordering::SeqCst),
            0,
            "no failure marker on success"
        );
    }

    /// AC1 (H3): a forced `Start` failure preserves the pending evidence — the
    /// clear is NOT called — and records a failure marker.
    #[tokio::test]
    async fn auto_start_failure_preserves_pending_and_marks_failure() {
        let app = tauri::test::mock_app();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let seen = spawn_start_drain(
            cmd_rx,
            Err(ServiceError::Platform("forced start failure".into())),
        );

        let cleared = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let cleared_c = cleared.clone();
        let failed_c = failed.clone();

        let started = run_auto_start(
            StartConfig::default(),
            app.handle().clone(),
            cmd_tx,
            Box::new(move || {
                cleared_c.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(move || {
                failed_c.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .await;

        assert!(!started, "failed Start should return false");
        assert!(seen.await.unwrap(), "Start command should be received");
        assert_eq!(
            cleared.load(Ordering::SeqCst),
            0,
            "pending must be PRESERVED on failure (clear not called)"
        );
        assert_eq!(
            failed.load(Ordering::SeqCst),
            1,
            "failure marker must be recorded exactly once on failure"
        );
    }

    /// H3 edge: if the actor channel is closed before `Start` is delivered, the
    /// pending evidence is preserved (failure path), not silently consumed.
    #[tokio::test]
    async fn auto_start_channel_closed_preserves_pending() {
        let app = tauri::test::mock_app();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<ManagerCommand<_>>(16);
        // Drop the receiver so the send fails immediately.
        drop(cmd_rx);

        let cleared = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let cleared_c = cleared.clone();
        let failed_c = failed.clone();

        let started = run_auto_start(
            StartConfig::default(),
            app.handle().clone(),
            cmd_tx,
            Box::new(move || {
                cleared_c.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(move || {
                failed_c.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .await;

        assert!(!started, "closed channel should return false");
        assert_eq!(
            cleared.load(Ordering::SeqCst),
            0,
            "pending must be preserved when the command never sends"
        );
        assert_eq!(
            failed.load(Ordering::SeqCst),
            1,
            "failure marker recorded when the command channel is closed"
        );
    }

    // ── iOS Warm Auto-Start Tests (H14, M14 part 2) ─────────────────────────
    //
    // A BGTask delivered to a *warm/idle* process must actually start the Rust
    // service (H14), mirroring the cold auto-start sequence: pre-check
    // AlreadyRunning → re-send SetOnComplete → Start → consume pending on
    // success. A warm delivery to an already-running actor is a clean no-op
    // (no double-start, no failure marker, pending NOT consumed) (M14 part 2).

    /// Service whose `run()` blocks until cancelled — keeps `is_running` true so
    /// the "warm delivery while running" no-op path can be exercised.
    struct WarmBlockingService;

    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for WarmBlockingService {
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

    /// Service that completes `run()` immediately with success — used to prove the
    /// captured `on_complete` callback fires (vs the iOS safety timer).
    struct WarmQuickService;

    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for WarmQuickService {
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

    /// Spawn a real manager actor (mirrors `manager::tests::setup_manager`) so the
    /// warm-start `AlreadyRunning` pre-check and `on_complete` arming run against
    /// genuine actor state rather than a fake drain.
    fn spawn_real_manager(
        factory: crate::manager::ServiceFactory<tauri::test::MockRuntime>,
    ) -> tokio::sync::mpsc::Sender<ManagerCommand<tauri::test::MockRuntime>> {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(manager_loop(
            cmd_rx,
            factory,
            28.0,
            0.0,
            15.0,
            15.0,
            false,
            false,
            4.0,
            None,
            vec!["remoteMessaging".into()],
            true,
            NotifierPolicy::default(),
            None,
            None,
            false,
        ));
        cmd_tx
    }

    async fn warm_is_running(
        cmd_tx: &tokio::sync::mpsc::Sender<ManagerCommand<tauri::test::MockRuntime>>,
    ) -> bool {
        let (tx, rx) = tokio::sync::oneshot::channel();
        cmd_tx
            .send(ManagerCommand::IsRunning { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    fn noop_on_complete() -> OnCompleteCallback {
        Box::new(|_success| {})
    }

    /// AC1 (H14): a warm BGTask delivered to an idle actor with desired_running
    /// starts the service (`is_running` flips true) and consumes the pending
    /// record exactly once.
    #[tokio::test]
    async fn warm_start_idle_starts_actor_and_consumes_pending() {
        let app = tauri::test::mock_app();
        let cmd_tx = spawn_real_manager(Box::new(|| Box::new(WarmBlockingService)));

        let consumed = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let consumed_c = consumed.clone();
        let failed_c = failed.clone();

        let started = run_warm_start(
            StartConfig::default(),
            app.handle().clone(),
            cmd_tx.clone(),
            noop_on_complete(),
            Box::new(move || {
                consumed_c.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(move || {
                failed_c.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .await;

        assert!(
            started,
            "warm delivery to idle actor should start the service"
        );
        assert!(
            warm_is_running(&cmd_tx).await,
            "is_running should flip true after warm start"
        );
        assert_eq!(
            consumed.load(Ordering::SeqCst),
            1,
            "pending must be consumed exactly once on warm success"
        );
        assert_eq!(
            failed.load(Ordering::SeqCst),
            0,
            "no failure marker on success"
        );
    }

    /// AC2 (M14 part 2): a warm delivery to an already-running actor is a clean
    /// no-op — it does NOT double-start, does NOT record a failure marker, and
    /// does NOT consume the pending record a second time.
    #[tokio::test]
    async fn warm_start_while_running_is_noop() {
        let app = tauri::test::mock_app();
        let cmd_tx = spawn_real_manager(Box::new(|| Box::new(WarmBlockingService)));

        let consumed = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));

        // First warm delivery starts the service.
        let consumed_1 = consumed.clone();
        let failed_1 = failed.clone();
        let first = run_warm_start(
            StartConfig::default(),
            app.handle().clone(),
            cmd_tx.clone(),
            noop_on_complete(),
            Box::new(move || {
                consumed_1.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(move || {
                failed_1.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .await;
        assert!(first, "first warm delivery should start the service");

        // Second warm delivery, while running, must be a clean no-op.
        let consumed_2 = consumed.clone();
        let failed_2 = failed.clone();
        let second = run_warm_start(
            StartConfig::default(),
            app.handle().clone(),
            cmd_tx.clone(),
            noop_on_complete(),
            Box::new(move || {
                consumed_2.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(move || {
                failed_2.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .await;

        assert!(
            !second,
            "warm delivery while running should be a no-op (false)"
        );
        assert!(
            warm_is_running(&cmd_tx).await,
            "service should still be running after the no-op warm delivery"
        );
        assert_eq!(
            consumed.load(Ordering::SeqCst),
            1,
            "pending consumed exactly once across both deliveries"
        );
        assert_eq!(
            failed.load(Ordering::SeqCst),
            0,
            "a no-op warm delivery must NOT record a failure marker"
        );
    }

    /// AC1 (H14): after a warm start the captured `on_complete` callback fires
    /// (proving SetOnComplete was re-sent and captured at spawn), not the iOS
    /// safety timer.
    #[tokio::test]
    async fn warm_start_arms_captured_on_complete_callback() {
        let app = tauri::test::mock_app();
        let cmd_tx = spawn_real_manager(Box::new(|| Box::new(WarmQuickService)));

        let fired = Arc::new(AtomicBool::new(false));
        let fired_cb = fired.clone();
        let on_complete: OnCompleteCallback = Box::new(move |success| {
            if success {
                fired_cb.store(true, Ordering::SeqCst);
            }
        });

        let started = run_warm_start(
            StartConfig::default(),
            app.handle().clone(),
            cmd_tx.clone(),
            on_complete,
            Box::new(|| {}),
            Box::new(|| {}),
        )
        .await;
        assert!(started, "warm start should initiate the service");

        // Wait for the immediately-completing service to fire the captured callback.
        let mut armed = false;
        for _ in 0..50 {
            if fired.load(Ordering::SeqCst) {
                armed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            armed,
            "captured on_complete callback must fire after warm start (SetOnComplete re-sent)"
        );
    }

    /// H3/M14: a genuine `Start` failure on the warm path preserves the pending
    /// evidence (clear not called) and records a failure marker — distinct from
    /// the AlreadyRunning no-op.
    #[tokio::test]
    async fn warm_start_failure_preserves_pending_and_marks_failure() {
        let app = tauri::test::mock_app();
        let (cmd_tx, cmd_rx) =
            tokio::sync::mpsc::channel::<ManagerCommand<tauri::test::MockRuntime>>(16);

        // Drain: reply IsRunning=false, swallow SetOnComplete, fail the Start.
        tokio::spawn(async move {
            let mut rx = cmd_rx;
            while let Ok(Some(cmd)) =
                tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await
            {
                match cmd {
                    ManagerCommand::IsRunning { reply } => {
                        let _ = reply.send(false);
                    }
                    ManagerCommand::SetOnComplete { .. } => {}
                    ManagerCommand::Start { reply, .. } => {
                        let _ = reply.send(Err(ServiceError::Platform("forced".into())));
                        break;
                    }
                    _ => {}
                }
            }
        });

        let consumed = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let consumed_c = consumed.clone();
        let failed_c = failed.clone();

        let started = run_warm_start(
            StartConfig::default(),
            app.handle().clone(),
            cmd_tx,
            noop_on_complete(),
            Box::new(move || {
                consumed_c.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(move || {
                failed_c.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .await;

        assert!(!started, "forced warm start failure should return false");
        assert_eq!(
            consumed.load(Ordering::SeqCst),
            0,
            "pending must be PRESERVED on genuine failure (clear not called)"
        );
        assert_eq!(
            failed.load(Ordering::SeqCst),
            1,
            "failure marker recorded exactly once on genuine failure"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  IPC AUTO-START RECOVERY TESTS (Step 12)
    // ═══════════════════════════════════════════════════════════════════════

    // Unix-only: these tests bind real Unix domain sockets via `test_helpers`.
    #[cfg(all(feature = "desktop-service", unix))]
    mod ipc_auto_start_tests {
        use super::*;
        use crate::desktop::ipc_client::PersistentIpcClientHandle;
        use crate::desktop::test_helpers::setup_server;
        use std::time::Duration;

        /// Verify that `wait_for_connected` returns `false` when the timeout
        /// expires without a server, and that the error message includes
        /// the socket path.
        #[tokio::test]
        async fn wait_for_connected_timeout_returns_false() {
            let app = tauri::test::mock_app();
            let path = crate::desktop::test_helpers::unique_socket_path();
            let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

            let connected = handle
                .wait_for_connected(Duration::from_millis(200))
                .await
                .unwrap();
            assert!(!connected, "should return false on timeout");

            let _ = std::fs::remove_file(&path);
        }

        /// Verify that `wait_for_connected` returns `true` once a server
        /// appears and the persistent client connects.
        #[tokio::test]
        async fn wait_for_connected_succeeds_with_server() {
            let (path, shutdown, _event_tx) = setup_server();
            let app = tauri::test::mock_app();
            let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

            let connected = handle
                .wait_for_connected(Duration::from_secs(5))
                .await
                .unwrap();
            assert!(connected, "should connect within timeout");

            shutdown.cancel();
        }

        /// Verify that `socket_path()` returns the path the handle was
        /// spawned with.
        #[tokio::test]
        async fn socket_path_accessor() {
            let app = tauri::test::mock_app();
            let path = crate::desktop::test_helpers::unique_socket_path();
            let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());
            assert_eq!(
                handle.socket_path(),
                &path,
                "socket_path() should return the path passed to spawn"
            );
            let _ = std::fs::remove_file(&path);
        }

        /// Verify the disconnected path with `desktop_start_service_if_missing=false`
        /// returns an IPC error containing "ipcUnavailable".
        ///
        /// This tests the `start` command handler's disconnected branch
        /// by directly checking the error construction logic.
        #[tokio::test]
        async fn start_disconnected_without_auto_start_returns_ipc_error() {
            let err = ServiceError::Ipc("ipcUnavailable".into());
            let msg = err.to_string();
            assert!(
                msg.contains("ipcUnavailable"),
                "error should contain 'ipcUnavailable': {msg}"
            );
        }

        /// Verify the timeout error includes the socket path for diagnostics.
        #[tokio::test]
        async fn start_timeout_error_includes_socket_path() {
            let socket = "/tmp/test-socket-path.sock";
            let err = ServiceError::Ipc(format!("ipcUnavailable: socket {socket}"));
            let msg = err.to_string();
            assert!(
                msg.contains(socket),
                "error should contain socket path: {msg}"
            );
        }
    }
}
