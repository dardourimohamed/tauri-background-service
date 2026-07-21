//! Headless daemon example (DESK-06).
//!
//! Demonstrates the API surface a host application uses to run the plugin's
//! OS-service sidecar via the re-exported `headless_main` entrypoint. Compile
//! with:
//!
//! ```sh
//! cargo build --example headless_daemon --features desktop-service
//! ```
//!
//! In a real host binary, `app` is constructed via
//! `tauri::Builder::default().build(tauri::generate_context!())` (with no
//! webview features). This example uses the Tauri test mock so it compiles
//! and runs without a `tauri.conf.json`; it only wires the factory and
//! invokes `headless_main`, then exits when the IPC server shuts down.
//!
//! The factory here yields a `HeartbeatService` that logs every 5 s —
//! a smoke harness, not a real background workload.

use std::time::Duration;

use async_trait::async_trait;
use tauri_plugin_background_service::{
    headless_main, BackgroundService, ServiceContext, ServiceError,
};

/// Minimal heartbeat service for the headless example.
struct HeartbeatService;

#[async_trait]
impl BackgroundService<tauri::test::MockRuntime> for HeartbeatService {
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
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = ctx.shutdown.cancelled() => {
                    log::info!("headless-daemon: shutdown received, exiting run loop");
                    return Ok(());
                }
                _ = tick.tick() => {
                    log::info!("headless-daemon: heartbeat");
                }
            }
        }
    }
}

fn main() {
    // Real hosts wire env_logger / tracing here; omitted to keep this
    // example self-contained (no extra deps).

    // A real host passes its GUI `AppHandle` here. We use the Tauri test
    // mock so the example is self-contained and compiles without a
    // `tauri.conf.json`. `headless_main` blocks until the IPC server shuts
    // down (e.g. SIGINT in a real deployment).
    let app = tauri::test::mock_app();
    headless_main(|| Box::new(HeartbeatService), app.handle().clone());
    log::info!("headless-daemon: headless_main returned");
}
