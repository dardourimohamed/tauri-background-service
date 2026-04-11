//! Desktop OS service management.
//!
//! This module provides support for running the background service as an
//! OS-level service (systemd on Linux, launchd on macOS, Windows Service)
//! with IPC between the GUI process and the headless service process.
//!
//! Only available when the `desktop-service` Cargo feature is enabled.

pub mod headless;
pub mod ipc;
pub mod ipc_client;
pub mod ipc_server;
pub mod service_manager;
pub mod transport;

// Platform-specific transport implementation (submodule of transport).
#[cfg(unix)]
pub mod transport_unix;
#[cfg(windows)]
pub mod transport_windows;

#[cfg(test)]
pub mod test_helpers;
