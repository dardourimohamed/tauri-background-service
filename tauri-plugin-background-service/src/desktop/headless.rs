//! Headless sidecar entry point for desktop OS service mode.
//!
//! The [`headless_main`] function serves as the entry point for the sidecar
//! binary that runs the background service as an OS-level service. It parses
//! CLI arguments, binds the IPC socket, spawns the service manager actor loop,
//! and runs the IPC server until shutdown.
//!
//! # Usage
//!
//! ```rust,ignore
//! // src/headless.rs (in the user's app crate)
//! use tauri_plugin_background_service::headless_main;
//!
//! fn main() {
//!     let app = tauri::Builder::default()
//!         .build(tauri::generate_context!())
//!         .expect("failed to build headless app");
//!     headless_main(
//!         || Box::new(MyBackgroundService::new()),
//!         app.handle().clone(),
//!     );
//! }
//! ```

use tauri::{AppHandle, Runtime};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::desktop::ipc::socket_path;
use crate::desktop::ipc_server::IpcServer;
use crate::manager::manager_loop;
use crate::service_trait::BackgroundService;

/// Check if `--validate-service-install` is present in CLI arguments.
///
/// Used by `install_service` to verify the binary handles `--service-label`
/// without actually starting the service.
fn has_validate_flag(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|arg| arg == "--validate-service-install")
}

/// Parse `--service-label <label>` from CLI arguments.
///
/// Returns the label on success, or a descriptive error message on failure.
fn parse_service_label(args: impl Iterator<Item = String>) -> Result<String, String> {
    let mut args = args.skip(1); // skip program name
    while let Some(arg) = args.next() {
        if arg == "--service-label" {
            let value = args
                .next()
                .ok_or_else(|| "--service-label requires a value".to_string())?;
            if value.is_empty() {
                return Err("--service-label value must not be empty".to_string());
            }
            return Ok(value);
        }
    }
    Err("--service-label is required. Usage: <binary> --service-label <label>".to_string())
}

/// Entry point for the headless sidecar binary.
///
/// Parses `--service-label <label>` from CLI arguments, constructs the service
/// manager actor loop, binds the IPC socket, and runs the IPC server until
/// either the server shuts down or `SIGINT` (Ctrl+C) is received.
///
/// # Arguments
///
/// * `factory` — Factory closure that creates a fresh `Box<dyn BackgroundService<R>>`
///   per start. Must match the same factory used in the GUI app's `init_with_service()`.
/// * `app` — A minimal headless `AppHandle<R>`. Constructed via
///   `tauri::Builder::default().build(tauri::generate_context!())` with no
///   webview features enabled.
///
/// # Panics / Exit
///
/// Prints an error message to stderr and exits with code 1 if:
/// - `--service-label` is missing or invalid
/// - The tokio runtime fails to initialize
/// - The IPC socket fails to bind
pub fn headless_main<F, R>(factory: F, app: AppHandle<R>)
where
    F: Fn() -> Box<dyn BackgroundService<R>> + Send + Sync + 'static,
    R: Runtime,
{
    let label = parse_service_label(std::env::args()).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    // Early-exit for install validation: the GUI process spawns us with
    // --validate-service-install to confirm we handle --service-label.
    // Exit immediately before binding sockets or spawning tasks.
    if has_validate_flag(std::env::args()) {
        println!("ok");
        std::process::exit(0);
    }

    tauri::async_runtime::block_on(async move {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        tauri::async_runtime::spawn(manager_loop(
            cmd_rx,
            Box::new(factory),
            0.0,
            0.0,
            0.0,
            0.0,
            false,
            false,
        ));

        let path = match socket_path(&label) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: invalid service label: {e}");
                return;
            }
        };
        let server = match IpcServer::bind(path, cmd_tx, app) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: failed to bind IPC socket: {e}");
                return;
            }
        };
        let shutdown = CancellationToken::new();

        tokio::select! {
            _ = server.run(shutdown.clone()) => {}
            _ = tokio::signal::ctrl_c() => {
                shutdown.cancel();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── AC1: CLI arg parsing works ─────────────────────────────────────

    #[test]
    fn headless_main_parses_service_label() {
        let args = vec![
            "my-app-headless".to_string(),
            "--service-label".to_string(),
            "com.example.svc".to_string(),
        ];
        let label = parse_service_label(args.into_iter()).unwrap();
        assert_eq!(label, "com.example.svc");
    }

    #[test]
    fn headless_main_parses_label_with_other_args() {
        let args = vec![
            "my-app-headless".to_string(),
            "--verbose".to_string(),
            "--service-label".to_string(),
            "com.example.svc".to_string(),
            "--other".to_string(),
        ];
        let label = parse_service_label(args.into_iter()).unwrap();
        assert_eq!(label, "com.example.svc");
    }

    // ── AC2: Missing label produces error ──────────────────────────────

    #[test]
    fn headless_main_rejects_missing_label() {
        let args = vec!["my-app-headless".to_string()];
        let result = parse_service_label(args.into_iter());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("--service-label"),
            "Error should mention --service-label: {err}"
        );
    }

    #[test]
    fn headless_main_rejects_label_without_value() {
        let args = vec!["my-app-headless".to_string(), "--service-label".to_string()];
        let result = parse_service_label(args.into_iter());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("value"),
            "Error should mention missing value: {err}"
        );
    }

    #[test]
    fn headless_main_rejects_empty_label() {
        let args = vec![
            "my-app-headless".to_string(),
            "--service-label".to_string(),
            "".to_string(),
        ];
        let result = parse_service_label(args.into_iter());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("empty"),
            "Error should mention empty value: {err}"
        );
    }

    // ── AC3: --validate-service-install flag detection ────────────────────

    #[test]
    fn validate_flag_detected_when_present() {
        let args = vec![
            "my-app-headless".to_string(),
            "--service-label".to_string(),
            "com.example.svc".to_string(),
            "--validate-service-install".to_string(),
        ];
        assert!(has_validate_flag(args.into_iter()));
    }

    #[test]
    fn validate_flag_absent() {
        let args = vec![
            "my-app-headless".to_string(),
            "--service-label".to_string(),
            "com.example.svc".to_string(),
        ];
        assert!(!has_validate_flag(args.into_iter()));
    }
}
