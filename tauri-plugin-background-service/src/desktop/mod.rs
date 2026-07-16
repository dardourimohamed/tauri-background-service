//! Desktop OS service management.
//!
//! This module provides support for running the background service as an
//! OS-level service (systemd on Linux, launchd on macOS, Windows Service).
//!
//! # Platform Support
//!
//! - **OS service mode (IPC)**: all desktop platforms. The IPC transport uses
//!   Unix domain sockets on Linux/macOS and named pipes on Windows.
//! - **Service install/uninstall**: All desktop platforms via the `service-manager` crate.
//!
//! Only available when the `desktop-service` Cargo feature is enabled.

pub mod env_checks;
pub mod service_manager;
pub mod transport;

// IPC modules, generic over the platform transport (Unix domain sockets or
// Windows named pipes).
#[cfg(any(unix, windows))]
pub mod headless;
#[cfg(any(unix, windows))]
pub mod ipc;
#[cfg(any(unix, windows))]
pub mod ipc_client;
#[cfg(any(unix, windows))]
pub mod ipc_server;

// Platform-specific transport implementation (submodule of transport).
#[cfg(unix)]
pub mod transport_unix;
#[cfg(windows)]
pub mod transport_windows;

#[cfg(all(test, unix))]
pub mod test_helpers;
