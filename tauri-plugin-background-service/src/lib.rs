#![doc(html_root_url = "https://docs.rs/tauri-plugin-background-service/0.2.0")]

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
#[cfg(target_os = "ios")]
fn ios_spawn_cancel_listener<R: Runtime>(app: &AppHandle<R>, timeout_secs: u64) {
    let mobile = app.state::<Arc<MobileLifecycle<R>>>();
    let mobile_handle = mobile.handle.clone();
    let manager = app.state::<ServiceManagerHandle<R>>();
    let cmd_tx = manager.cmd_tx.clone();

    tokio::spawn(async move {
        let handle = tokio::task::spawn_blocking(move || {
            let mob = MobileLifecycle {
                handle: mobile_handle,
            };
            mob.wait_for_cancel()
        });
        // Safety timeout prevents indefinite thread leaks if iOS
        // invoke is never resolved (e.g., iOS kills the app).
        let result = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), handle).await;
        if let Ok(Ok(Ok(()))) = result {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx.send(ManagerCommand::Stop { reply: tx }).await;
            let _ = rx.await;
        }
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
        return ipc_state.client.start(config).await.map_err(|e| e.to_string());
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

    rx.await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;

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

    rx.await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())
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
            let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
            let handle = ServiceManagerHandle::new(cmd_tx);
            app.manage(handle);

            let config = api.config().clone();
            app.manage(config.clone());

            let ios_safety_timeout_secs = config.ios_safety_timeout_secs;
            let ios_processing_safety_timeout_secs = config.ios_processing_safety_timeout_secs;

            // Mode dispatch: spawn in-process actor or configure IPC for OS service.
            #[cfg(feature = "desktop-service")]
            if config.desktop_service_mode == "osService" {
                // OS service mode: spawn persistent IPC client.
                let label = desktop::service_manager::derive_service_label(
                    app,
                    config.desktop_service_label.as_deref(),
                );
                let socket_path = desktop::ipc::socket_path(&label);
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
                ));
            }

            #[cfg(mobile)]
            {
                let lifecycle = mobile::init(app, api)?;
                let lifecycle_arc = std::sync::Arc::new(lifecycle);

                // Send SetMobile to actor so keepalive is managed by the actor.
                let mobile_trait: Arc<dyn MobileKeepalive> = lifecycle_arc.clone();
                let _ = cmd_tx.try_send(ManagerCommand::SetMobile { mobile: mobile_trait });

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
                    let app_clone = app.handle().clone();

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
        async fn init(
            &mut self,
            _ctx: &ServiceContext<tauri::Wry>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        async fn run(
            &mut self,
            _ctx: &ServiceContext<tauri::Wry>,
        ) -> Result<(), ServiceError> {
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
    fn init_with_service_returns_tauri_plugin<R: Runtime, S, F>(factory: F) -> TauriPlugin<R, PluginConfig>
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

}
