#![doc(html_root_url = "https://docs.rs/tauri-plugin-background-service/0.2.2")]

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

pub mod error;
pub mod manager;
pub mod models;
pub mod notifier;
pub mod service_trait;

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
pub use models::{PluginConfig, PluginEvent, ServiceContext, StartConfig};
pub use notifier::Notifier;
pub use service_trait::BackgroundService;

#[cfg(feature = "desktop-service")]
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

#[cfg(mobile)]
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
/// 1. **Resolved invoke** (safety timer / expiration) → `Ok(())` → send `Stop`.
/// 2. **Timeout** (default: 4h) → call `cancel_cancel_listener` to unblock the
///    thread, then send `Stop`.
/// 3. **Rejected invoke** (explicit stop / natural completion) → `Err` → no action.
///
/// Core cancel listener logic, extracted for testability.
///
/// - `wait_fn`: blocking function simulating `wait_for_cancel` (returns `Ok(())` on resolve,
///   `Err` on reject).
/// - `cancel_fn`: called on timeout to unblock the `wait_fn` thread.
/// - `cmd_tx`: channel to send `Stop` command on resolve/timeout.
/// - `timeout_secs`: how long to wait before treating the listener as timed out.
///
/// Returns `true` if a `Stop` was sent, `false` otherwise.
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
        Ok(Ok(Ok(()))) | Err(_) => {
            // On timeout, unblock the spawn_blocking thread first.
            if result.is_err() {
                cancel_fn();
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx.send(ManagerCommand::Stop { reply: tx }).await;
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
    #[cfg(feature = "desktop-service")]
    if let Some(ipc_state) = app.try_state::<DesktopIpcState>() {
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
    #[cfg(feature = "desktop-service")]
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
    #[cfg(feature = "desktop-service")]
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

// ─── Desktop OS Service State & Commands ──────────────────────────────────────

/// Managed state indicating OS service mode via IPC.
///
/// When present as managed state, the `start`/`stop`/`is_running` commands
/// route through the persistent IPC client instead of the in-process actor loop.
#[cfg(feature = "desktop-service")]
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
    mgr.install().map_err(|e| e.to_string())
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
            #[cfg(feature = "desktop-service")]
            install_service,
            #[cfg(feature = "desktop-service")]
            uninstall_service,
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

            // Mode dispatch: spawn in-process actor or configure IPC for OS service.
            #[cfg(feature = "desktop-service")]
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

    // ── Desktop IPC State Tests ─────────────────────────────────────────

    /// Verify PersistentIpcClientHandle can be constructed.
    #[cfg(feature = "desktop-service")]
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

    // ── Cancel Listener Tests ───────────────────────────────────────────────

    use crate::manager::ManagerCommand;
    use std::sync::atomic::AtomicBool;

    /// Helper: spawn a background task that accepts one Stop command and replies Ok(()).
    /// Returns a oneshot receiver that yields true if Stop was received.
    fn spawn_stop_drain(
        mut cmd_rx: tokio::sync::mpsc::Receiver<ManagerCommand<tauri::test::MockRuntime>>,
    ) -> tokio::sync::oneshot::Receiver<bool> {
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<bool>();
        tokio::spawn(async move {
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv()).await;
            match result {
                Ok(Some(ManagerCommand::Stop { reply })) => {
                    let _ = reply.send(Ok(()));
                    let _ = seen_tx.send(true);
                }
                _ => {
                    let _ = seen_tx.send(false);
                }
            }
        });
        seen_rx
    }

    #[tokio::test]
    async fn cancel_listener_resolved_invoke_sends_stop() {
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
        assert!(
            seen.await.unwrap(),
            "Stop command should be sent on resolved invoke"
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
        assert!(
            !seen.await.unwrap(),
            "Stop command should NOT be sent on rejected invoke"
        );
    }

    #[tokio::test]
    async fn cancel_listener_timeout_sends_stop() {
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
        assert!(
            seen.await.unwrap(),
            "Stop command should be sent on timeout"
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
        assert!(
            !seen.await.unwrap(),
            "Stop command should NOT be sent on join error"
        );
    }
}
