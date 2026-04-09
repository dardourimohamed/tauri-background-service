//! Example background service demonstrating the trait implementation pattern.
//!
//! This example shows how to implement [`BackgroundService`] on your own struct
//! with `init()` and `run()` async methods, using `tokio::select!` for
//! cooperative cancellation.

use async_trait::async_trait;
use std::time::Duration;
use tauri::{Emitter, Runtime};
use tauri_plugin_background_service::{BackgroundService, ServiceContext, ServiceError};

/// A minimal example service that ticks periodically and emits events.
///
/// Demonstrates:
/// - Implementing `BackgroundService<R>` on your own struct
/// - Using `tokio::select!` with `ctx.shutdown.cancelled()` for clean shutdown
/// - Emitting custom events to the JS layer via `ctx.app.emit()`
/// - Sending local notifications via `ctx.notifier.show()`
#[allow(dead_code)]
struct ExampleService {
    tick_count: u64,
}

impl ExampleService {
    #[allow(dead_code)]
    fn new() -> Self {
        Self { tick_count: 0 }
    }
}

#[async_trait]
impl<R: Runtime> BackgroundService<R> for ExampleService {
    async fn init(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        log::info!("ExampleService initialized");
        Ok(())
    }

    async fn run(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        let mut interval = tokio::time::interval(Duration::from_secs(10));

        loop {
            tokio::select! {
                _ = ctx.shutdown.cancelled() => {
                    log::info!(
                        "ExampleService shutting down after {} ticks",
                        self.tick_count
                    );
                    break;
                }
                _ = interval.tick() => {
                    self.tick_count += 1;
                    log::info!("Tick #{}", self.tick_count);

                    // Show a notification on the first tick
                    if self.tick_count == 1 {
                        ctx.notifier
                            .show("Background Service", "Service is running");
                    }

                    // Emit a custom event to the JS layer
                    let _ = ctx.app.emit("background-service://tick", self.tick_count);
                }
            }
        }

        Ok(())
    }
}

fn main() {
    println!("Example BackgroundService implementation");
    println!();
    println!("To use in your Tauri app:");
    println!("  tauri::Builder::default()");
    println!("      .plugin(tauri_plugin_notification::init())");
    println!("      .plugin(tauri_plugin_background_service::init_with_service(");
    println!("          || ExampleService::new()))");
}
