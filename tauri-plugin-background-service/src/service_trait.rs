//! The [`BackgroundService`] trait — the single entry point for user code.
//!
//! Implement this trait to define a background service. The plugin creates
//! a fresh instance via the factory closure passed to
//! [`init_with_service`](crate::init_with_service) on each start.

use async_trait::async_trait;
use tauri::Runtime;

use crate::error::ServiceError;
use crate::models::ServiceContext;

/// The contract users implement to define a background service.
///
/// Implement `init()` for one-time setup and `run()` for the main loop.
/// Both receive a shared [`ServiceContext`] with a notifier, app handle,
/// and cancellation token.
///
/// # Object Safety
///
/// The trait is object-safe thanks to `#[async_trait]`, enabling the
/// factory pattern: `Box<dyn BackgroundService<R>>`.
#[async_trait]
pub trait BackgroundService<R: Runtime>: Send + 'static {
    /// Called once before `run`. Use for initialisation that requires
    /// the Tauri context (e.g. opening database connections, registering
    /// event listeners).
    async fn init(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError>;

    /// The main service loop. The runner spawns this in a Tokio task.
    /// Use `tokio::select!` with `ctx.shutdown.cancelled()` for
    /// cooperative cancellation.
    async fn run(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::AppHandle;
    use tokio_util::sync::CancellationToken;

    use crate::notifier::Notifier;

    /// Compile-time test: the trait can be implemented on a concrete struct.
    #[allow(dead_code)]
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

    /// Compile-time test: the trait is object-safe — `Box<dyn BackgroundService<R>>` compiles.
    #[allow(dead_code)]
    fn box_dyn_compiles() {
        let _boxed: Box<dyn BackgroundService<tauri::Wry>> = Box::new(DummyService);
    }

    /// Compile-time test: ServiceContext can be constructed with the real Notifier type.
    /// On mobile, includes service_label and foreground_service_type (String).
    /// On desktop, those fields are absent (#[cfg(mobile)]).
    #[allow(dead_code)]
    fn service_context_constructs<R: Runtime>(app: AppHandle<R>) {
        let _ctx = ServiceContext {
            notifier: Notifier { app: app.clone() },
            app,
            shutdown: CancellationToken::new(),
            #[cfg(mobile)]
            service_label: "test".into(),
            #[cfg(mobile)]
            foreground_service_type: "dataSync".into(),
        };
    }
}
