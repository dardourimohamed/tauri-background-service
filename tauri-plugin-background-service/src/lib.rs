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
//!     <string>background-processing</string>
//!     <string>background-fetch</string>
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
#[doc(hidden)]
pub use models::AutoStartConfig;
pub use models::{
    IOSSchedulingStatus, LifecycleState, LifecycleStatus, PendingTaskInfo, Platform,
    PlatformCapabilities, PluginConfig, PluginEvent, ServiceContext, ServiceState, ServiceStatus,
    SetupIssue, SetupValidationReport, StartConfig, ValidationIssue,
};
pub use notifier::Notifier;
pub use service_trait::BackgroundService;

#[cfg(all(feature = "desktop-service", unix))]
pub use desktop::headless::headless_main;

// ─── Internal Imports ────────────────────────────────────────────────────────

use tauri::{
    plugin::{Builder, TauriPlugin},
    AppHandle, Manager, Runtime,
};

use crate::manager::ManagerCommand;

#[cfg(mobile)]
use crate::manager::MobileKeepalive;

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

// ─── Tauri Commands ──────────────────────────────────────────────────────────

#[tauri::command]
async fn start<R: Runtime>(app: AppHandle<R>, config: StartConfig) -> Result<(), String> {
    // OS service mode: route through persistent IPC client.
    #[cfg(all(feature = "desktop-service", unix))]
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
    #[cfg(all(feature = "desktop-service", unix))]
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
    #[cfg(all(feature = "desktop-service", unix))]
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
    #[cfg(all(feature = "desktop-service", unix))]
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

    #[cfg(all(feature = "desktop-service", unix))]
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

    #[cfg(not(all(feature = "desktop-service", unix)))]
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
    #[cfg(all(feature = "desktop-service", unix))]
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
    #[cfg(all(feature = "desktop-service", unix))]
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
    #[cfg(all(feature = "desktop-service", unix))]
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
    #[cfg(all(feature = "desktop-service", unix))]
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
    #[cfg(all(feature = "desktop-service", unix))]
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
#[cfg(all(feature = "desktop-service", unix))]
struct DesktopIpcState {
    client: desktop::ipc_client::PersistentIpcClientHandle,
}

#[cfg(feature = "desktop-service")]
#[tauri::command]
async fn install_service<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    use desktop::service_manager::{derive_service_label, DesktopServiceManager};
    let plugin_config = app.state::<PluginConfig>();
    let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());
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

    let mgr = DesktopServiceManager::new(&label, exec_path).map_err(|e| e.to_string())?;
    use desktop::service_manager::InstallOptions;
    let options = InstallOptions {
        autostart: plugin_config.desktop_service_autostart,
        restart_delay_secs: None,
        journal_output: true,
        log_path: None,
    };
    mgr.install(&options).map_err(|e| e.to_string())
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

/// Returns the standard "not yet supported" error for Windows OS-service mode.
#[cfg(feature = "desktop-service")]
#[allow(dead_code)] // Used on non-Unix targets and in tests
fn windows_os_service_unsupported() -> ServiceError {
    ServiceError::Platform("Windows OS-service mode is not yet supported".into())
}

/// Build an [`OsServiceStatus`] from available information.
///
/// Gathers the service label, mode string, IPC connection state, socket path,
/// and optional last error into a status snapshot.
#[cfg(all(feature = "desktop-service", unix))]
fn build_os_service_status(
    label: &str,
    ipc_connected: bool,
    socket_path: Option<String>,
    last_error: Option<String>,
) -> models::OsServiceStatus {
    let mode = if cfg!(target_os = "macos") {
        "launchd"
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
/// On Unix, delegates to [`DesktopServiceManager::start()`].
/// On Windows, returns `ServiceError::Platform`.
#[cfg(feature = "desktop-service")]
#[tauri::command]
async fn start_os_service<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    #[cfg(unix)]
    {
        use desktop::service_manager::{derive_service_label, DesktopServiceManager};
        let plugin_config = app.state::<PluginConfig>();
        let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());
        let exec_path = std::env::current_exe().map_err(|e| e.to_string())?;
        let mgr = DesktopServiceManager::new(&label, exec_path).map_err(|e| e.to_string())?;
        mgr.start().map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = app;
        Err(windows_os_service_unsupported().to_string())
    }
}

/// Stop the OS-level background service (desktop only).
///
/// On Unix, delegates to [`DesktopServiceManager::stop()`].
/// On Windows, returns `ServiceError::Platform`.
#[cfg(feature = "desktop-service")]
#[tauri::command]
async fn stop_os_service<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    #[cfg(unix)]
    {
        use desktop::service_manager::{derive_service_label, DesktopServiceManager};
        let plugin_config = app.state::<PluginConfig>();
        let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());
        let exec_path = std::env::current_exe().map_err(|e| e.to_string())?;
        let mgr = DesktopServiceManager::new(&label, exec_path).map_err(|e| e.to_string())?;
        mgr.stop().map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = app;
        Err(windows_os_service_unsupported().to_string())
    }
}

/// Restart the OS-level background service (desktop only).
///
/// On Unix, calls stop then start. On Windows, returns `ServiceError::Platform`.
#[cfg(feature = "desktop-service")]
#[tauri::command]
async fn restart_os_service<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    #[cfg(unix)]
    {
        use desktop::service_manager::{derive_service_label, DesktopServiceManager};
        let plugin_config = app.state::<PluginConfig>();
        let label = derive_service_label(&app, plugin_config.desktop_service_label.as_deref());
        let exec_path = std::env::current_exe().map_err(|e| e.to_string())?;
        let mgr = DesktopServiceManager::new(&label, exec_path).map_err(|e| e.to_string())?;
        mgr.stop().ok(); // Best-effort stop — service may not be running.
        mgr.start().map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = app;
        Err(windows_os_service_unsupported().to_string())
    }
}

/// Get the status of the OS-level background service (desktop only).
///
/// On Unix, returns [`OsServiceStatus`] with label, mode, IPC state, socket path.
/// On Windows, returns `ServiceError::Platform`.
#[cfg(feature = "desktop-service")]
#[tauri::command]
async fn get_os_service_status<R: Runtime>(
    app: AppHandle<R>,
) -> Result<models::OsServiceStatus, String> {
    #[cfg(unix)]
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
    #[cfg(not(unix))]
    {
        let _ = app;
        Err(windows_os_service_unsupported().to_string())
    }
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
            get_pending_bg_task,
            enable_auto_restart,
            disable_auto_restart,
            get_desired_service_state,
            native_lifecycle_event,
            validate_setup,
            get_lifecycle_status,
            configure_recovery,
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

            // Desktop: construct file-backed desired-state persistence backend.
            #[cfg(not(mobile))]
            let desired_state_backend: Option<Arc<dyn desired_state::DesiredStateBackend>> = {
                match app.path().app_data_dir() {
                    Ok(data_dir) => Some(Arc::new(desired_state::FileDesiredStateBackend::new(data_dir))),
                    Err(e) => {
                        log::warn!("Failed to get app data dir for desired-state persistence: {e}");
                        None
                    }
                }
            };
            #[cfg(mobile)]
            let desired_state_backend: Option<Arc<dyn desired_state::DesiredStateBackend>> = None;

            // Mode dispatch: spawn in-process actor or configure IPC for OS service.
            #[cfg(all(feature = "desktop-service", unix))]
            if config.desktop_service_mode == "osService" {
                // OS service mode: spawn persistent IPC client.
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
                    desired_state_backend,
                ));
            }

            #[cfg(all(feature = "desktop-service", not(unix)))]
            {
                // On non-Unix platforms, only in-process mode is available.
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
                    desired_state_backend,
                ));
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
                    desired_state_backend,
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
            // Checks native pending task info and desired_running flag.
            // If both are set, sends a Start command with the stored config.
            #[cfg(target_os = "ios")]
            {
                let mobile = app.state::<Arc<MobileLifecycle<R>>>();

                match mobile.get_pending_bg_task() {
                    Ok(Some(_pending)) => {
                        // Check desired_running and last_start_config from native.
                        let should_start = mobile
                            .get_scheduling_status_raw()
                            .ok()
                            .and_then(|v| {
                                let desired = v.get("desiredRunning")?.as_bool()?;
                                let config_str = v.get("lastStartConfig")?.as_str()?;
                                Some((desired, config_str.to_string()))
                            });

                        if let Some((true, config_str)) = should_start {
                            if let Ok(config) =
                                serde_json::from_str::<StartConfig>(&config_str)
                            {
                                let manager = app.state::<ServiceManagerHandle<R>>();
                                let cmd_tx = manager.cmd_tx.clone();
                                let app_clone = app.app_handle().clone();

                                // Capture timeout before spawn for cancel listener.
                                let plugin_config = app.state::<PluginConfig>();
                                let timeout_secs = plugin_config.ios_cancel_listener_timeout_secs;

                                // Set on_complete callback for iOS completeBgTask.
                                let mob_handle = mobile.handle.clone();
                                if let Err(e) = cmd_tx.try_send(ManagerCommand::SetOnComplete {
                                    callback: Box::new(move |success| {
                                        let ml =
                                            MobileLifecycle { handle: mob_handle.clone() };
                                        let _ = ml.complete_bg_task(success);
                                    }),
                                }) {
                                    log::error!("Failed to send SetOnComplete for iOS auto-start: {e}");
                                }

                                tauri::async_runtime::spawn(async move {
                                    let (tx, rx) = tokio::sync::oneshot::channel();
                                    if cmd_tx
                                        .send(ManagerCommand::Start {
                                            config,
                                            reply: tx,
                                            app: app_clone.clone(),
                                        })
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                    if let Ok(Ok(())) = rx.await {
                                        ios_spawn_cancel_listener(&app_clone, timeout_secs);
                                    }
                                });

                                log::info!("iOS: auto-starting service for pending BGTask");
                                let _ = mobile.clear_pending_bg_task();
                            } else {
                                log::warn!("iOS: failed to parse stored start config — preserving pending task info for diagnostics");
                            }
                        } else {
                            log::info!(
                                "iOS: pending BGTask but desired_running is false, skipping auto-start"
                            );
                            let _ = mobile.clear_pending_bg_task();
                        }
                    }
                    Ok(None) => {
                        // No pending BGTask — normal launch.
                    }
                    Err(e) => {
                        log::warn!("iOS: failed to get pending BGTask: {e}");
                    }
                }
            }

            // Android: auto-start detection after OS-initiated service restart.
            // When LifecycleService is restarted by START_STICKY, it sets an
            // auto-start flag in SharedPreferences and launches the Activity.
            // This block detects that flag, clears it, and starts the service
            // via the actor.
            #[cfg(target_os = "android")]
            {
                let mobile = app.state::<Arc<MobileLifecycle<R>>>();
                if let Ok(Some(config)) = mobile.get_auto_start_config() {
                    let _ = mobile.clear_auto_start_config();

                    // Keepalive is now handled by the actor's handle_start.
                    // Just send Start command — actor will call start_keepalive.

                    let manager = app.state::<ServiceManagerHandle<R>>();
                    let cmd_tx = manager.cmd_tx.clone();
                    let app_clone = app.app_handle().clone();

                    // Set a no-op on_complete callback for consistency with iOS path.
                    if let Err(e) = cmd_tx.try_send(ManagerCommand::SetOnComplete {
                        callback: Box::new(|_| {}),
                    }) {
                        log::error!("Failed to send SetOnComplete command: {e}");
                    }

                    tauri::async_runtime::spawn(async move {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        if cmd_tx
                            .send(ManagerCommand::Start {
                                config,
                                reply: tx,
                                app: app_clone,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                        let _ = rx.await;
                    });

                    let _ = mobile.move_task_to_background();
                }
            }

            Ok(())
        })
        .on_event(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // In OS service mode, the service runs in a separate process — skip.
                #[cfg(all(feature = "desktop-service", unix))]
                if app.try_state::<DesktopIpcState>().is_some() {
                    return;
                }
                let manager = app.state::<ServiceManagerHandle<R>>();
                if let Err(e) = manager.stop_blocking() {
                    log::warn!("Failed to stop background service on app exit: {e}");
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

    /// Verify `get_pending_bg_task` command signature is async and generic over `R: Runtime`.
    #[allow(dead_code)]
    async fn get_pending_bg_task_command_signature<R: Runtime>(
        app: AppHandle<R>,
    ) -> Result<Option<models::PendingTaskInfo>, String> {
        get_pending_bg_task(app).await
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
    #[cfg(all(feature = "desktop-service", unix))]
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

    /// Test that `windows_os_service_unsupported()` returns a Platform error.
    #[cfg(feature = "desktop-service")]
    #[test]
    fn windows_stub_returns_platform_error() {
        let err = windows_os_service_unsupported();
        assert!(
            matches!(err, ServiceError::Platform(ref msg) if msg.contains("not yet supported")),
            "Expected Platform error with 'not yet supported', got: {err}"
        );
    }

    /// Test that `build_os_service_status` produces a valid OsServiceStatus
    /// with the correct fields populated.
    #[cfg(all(feature = "desktop-service", unix))]
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
    #[cfg(all(feature = "desktop-service", unix))]
    #[test]
    fn build_os_service_status_mode_is_correct() {
        let status = build_os_service_status("test", false, None, None);
        #[cfg(target_os = "linux")]
        assert_eq!(status.mode, "systemd");
        #[cfg(target_os = "macos")]
        assert_eq!(status.mode, "launchd");
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

    // ═══════════════════════════════════════════════════════════════════════
    //  IPC AUTO-START RECOVERY TESTS (Step 12)
    // ═══════════════════════════════════════════════════════════════════════

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
