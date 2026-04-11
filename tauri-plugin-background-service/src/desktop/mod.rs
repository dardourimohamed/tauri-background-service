//! Desktop OS service management.
//!
//! This module provides support for running the background service as an
//! OS-level service (systemd on Linux, launchd on macOS, Windows Service).
//!
//! # Platform Support
//!
//! - **OS service mode (IPC)**: Unix-only (Linux, macOS). The IPC transport
//!   uses Unix domain sockets. Windows named pipe support is not yet implemented.
//! - **Service install/uninstall**: All desktop platforms via the `service-manager` crate.
//!
//! Only available when the `desktop-service` Cargo feature is enabled.

pub mod service_manager;
pub mod transport;

// Unix-only IPC modules.
#[cfg(unix)]
pub mod headless;
#[cfg(unix)]
pub mod ipc;
#[cfg(unix)]
pub mod ipc_client;
#[cfg(unix)]
pub mod ipc_server;

// Platform-specific transport implementation (submodule of transport).
#[cfg(unix)]
pub mod transport_unix;
#[cfg(windows)]
pub mod transport_windows;

#[cfg(all(test, unix))]
pub mod test_helpers;
