pub mod error;
pub mod models;
pub mod notifier;
pub mod runner;
pub mod service_trait;

#[cfg(mobile)]
pub mod mobile;

// ─── Public API Surface ──────────────────────────────────────────────────────

pub use error::ServiceError;
pub use models::{AutoStartConfig, PluginEvent, ServiceContext, StartConfig};
pub use notifier::Notifier;
pub use runner::ServiceRunner;
pub use service_trait::BackgroundService;

// ─── Internal Imports ────────────────────────────────────────────────────────

use std::sync::Arc;

use tauri::{
    plugin::{Builder, TauriPlugin},
    AppHandle, Manager, Runtime,
};

#[cfg(mobile)]
use mobile::MobileLifecycle;


// ─── iOS Plugin Binding ──────────────────────────────────────────────────────
// Must be at module level. Referenced by mobile::init() when registering
// the iOS plugin. Only compiled when targeting iOS.

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_background_service);

// ─── Service Factory Holder ──────────────────────────────────────────────────

/// Type-erased factory closure: creates `Box<dyn BackgroundService<R>>` on demand.
///
/// Stored in [`ServiceRunnerHolder`] so commands can create service instances
/// without knowing the concrete type.
type ServiceFactory<R> = Box<dyn Fn() -> Box<dyn BackgroundService<R>> + Send + Sync>;

/// Holds a [`ServiceRunner`] and a [`ServiceFactory`], stored as managed state.
struct ServiceRunnerHolder<R: Runtime> {
    runner: ServiceRunner,
    factory: ServiceFactory<R>,
}

impl<R: Runtime> ServiceRunnerHolder<R> {
    /// Invoke the factory to produce a fresh service, then delegate to the runner.
    fn start(&self, app: AppHandle<R>, config: StartConfig) -> Result<(), ServiceError> {
        let service = (self.factory)();
        self.runner.start_boxed(app, service, config)
    }
}

// ─── Tauri Commands ──────────────────────────────────────────────────────────

#[tauri::command]
async fn start<R: Runtime>(app: AppHandle<R>, config: StartConfig) -> Result<(), String> {
    #[cfg(mobile)]
    app.state::<MobileLifecycle<R>>()
        .start_keepalive(&config.service_label, &config.foreground_service_type)
        .map_err(|e| e.to_string())?;

    // iOS: wire on_complete callback BEFORE starting the service.
    // The callback must be set before holder.start() because start_boxed()
    // captures the callback via take() at spawn time. If set after start(),
    // the callback is always None and completeBgTask never fires.
    #[cfg(target_os = "ios")]
    {
        let mobile = app.state::<MobileLifecycle<R>>();
        let mobile_handle = mobile.handle.clone();
        let holder = app.state::<Arc<ServiceRunnerHolder<R>>>();

        // Set on_complete callback: notifies Swift when run() finishes
        let mob_for_complete = MobileLifecycle {
            handle: mobile_handle.clone(),
        };
        holder.runner.set_on_complete(Box::new(move |success| {
            let _ = mob_for_complete.complete_bg_task(success);
        }));
    }

    app.state::<Arc<ServiceRunnerHolder<R>>>()
        .start(app.clone(), config)
        .map_err(|e| e.to_string())?;

    // iOS: spawn wait_for_cancel listener AFTER starting the service.
    // This blocking thread waits for the iOS expiration signal (waitForCancel).
    // When expiration fires, stop the runner to trigger graceful cancellation.
    #[cfg(target_os = "ios")]
    {
        let mobile = app.state::<MobileLifecycle<R>>();
        let mobile_handle = mobile.handle.clone();

        // Spawn blocking thread: waits for iOS expiration signal
        let app_for_cancel = app.clone();
        tokio::task::spawn_blocking(move || {
            let mob = MobileLifecycle {
                handle: mobile_handle,
            };
            match mob.wait_for_cancel() {
                Ok(()) => {
                    // Expiration signaled — stop the runner
                    let holder = app_for_cancel.state::<Arc<ServiceRunnerHolder<R>>>();
                    let _ = holder.runner.stop();
                }
                Err(_) => {
                    // Invoke rejected (normal completion) — exit thread
                }
            }
        });
    }

    Ok(())
}

#[tauri::command]
async fn stop<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    app.state::<Arc<ServiceRunnerHolder<R>>>()
        .runner
        .stop()
        .map_err(|e| e.to_string())?;

    #[cfg(mobile)]
    app.state::<MobileLifecycle<R>>()
        .stop_keepalive()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
fn is_running<R: Runtime>(app: AppHandle<R>) -> bool {
    app.state::<Arc<ServiceRunnerHolder<R>>>()
        .runner
        .is_running()
}

// ─── Plugin Builder ──────────────────────────────────────────────────────────

/// Create the Tauri plugin with your service factory.
///
/// ```rust,ignore
/// // MyService must implement BackgroundService<R>
/// tauri::Builder::default()
///     .plugin(tauri_plugin_background_service::init_with_service(|| MyService::new()))
/// ```
pub fn init_with_service<R, S, F>(factory: F) -> TauriPlugin<R>
where
    R: Runtime,
    S: BackgroundService<R>,
    F: Fn() -> S + Send + Sync + 'static,
{
    let boxed_factory: ServiceFactory<R> = Box::new(move || Box::new(factory()));

    Builder::new("background-service")
        .invoke_handler(tauri::generate_handler![start, stop, is_running])
        .setup(move |app, _api| {
            app.manage(Arc::new(ServiceRunnerHolder {
                runner: ServiceRunner::new(),
                factory: boxed_factory,
            }));

            #[cfg(mobile)]
            {
                let lifecycle = mobile::init(app, _api)?;
                app.manage(lifecycle);
            }

            // Android: auto-start detection after OS-initiated service restart.
            // When LifecycleService is restarted by START_STICKY, it sets an
            // auto-start flag in SharedPreferences and launches the Activity.
            // This block detects that flag, clears it, and starts the service.
            #[cfg(target_os = "android")]
            {
                let mobile = app.state::<MobileLifecycle<R>>();
                if let Ok(Some(config)) = mobile.get_auto_start_config() {
                    let _ = mobile.clear_auto_start_config();
                    let _ = mobile.start_keepalive(&config.service_label, &config.foreground_service_type);
                    let holder = app.state::<Arc<ServiceRunnerHolder<R>>>();
                    let _ = holder.start(app.clone(), config);
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
    fn service_runner_holder_constructs() {
        let holder = ServiceRunnerHolder::<tauri::Wry> {
            runner: ServiceRunner::new(),
            factory: Box::new(|| Box::new(DummyService)),
        };
        assert!(!holder.runner.is_running());
    }

    #[test]
    fn factory_produces_boxed_service() {
        let factory: ServiceFactory<tauri::Wry> = Box::new(|| Box::new(DummyService));
        let _service: Box<dyn BackgroundService<tauri::Wry>> = factory();
    }

    #[test]
    fn holder_factory_creates_fresh_instances() {
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = count.clone();

        let factory: ServiceFactory<tauri::Wry> = Box::new(move || {
            count_clone.fetch_add(1, Ordering::SeqCst);
            Box::new(DummyService)
        });

        let holder = ServiceRunnerHolder::<tauri::Wry> {
            runner: ServiceRunner::new(),
            factory,
        };

        let _ = (holder.factory)();
        let _ = (holder.factory)();

        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    // ── Compile-time Tests ───────────────────────────────────────────────

    /// Verify `init_with_service` returns `TauriPlugin<R>`.
    #[allow(dead_code)]
    fn init_with_service_returns_tauri_plugin<R: Runtime, S, F>(factory: F) -> TauriPlugin<R>
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

    /// Verify `is_running` command signature is generic over `R: Runtime`.
    #[allow(dead_code)]
    fn is_running_command_signature<R: Runtime>(app: AppHandle<R>) -> bool {
        is_running(app)
    }
}
