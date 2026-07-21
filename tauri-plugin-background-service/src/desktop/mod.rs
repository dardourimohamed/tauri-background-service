//! Desktop OS service management.
//!
//! This module provides support for running the background service as a
//! Unix OS-level service (systemd user service on Linux, launchd agent on
//! macOS). DESK-01: Windows daemon support has been removed — the Windows
//! default-DACL named-pipe transport was unauthenticated and the LocalSystem
//! service installation path was unsafe to ship. Windows remains supported
//! via the in-process backend; OS-service commands return an
//! unsupported-platform error on Windows.
//!
//! Only available when the `desktop-service` Cargo feature is enabled.

pub mod env_checks;
pub mod service_manager;
pub mod transport;

// IPC modules, generic over the platform transport (Unix domain sockets).
// Windows is intentionally excluded (DESK-01): the daemon path was removed.
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

#[cfg(all(test, unix))]
pub mod test_helpers;
