//! Unix domain socket transport implementation.
//!
//! Provides [`bind`], [`connect`], [`peer_cred_check`], [`cleanup`], and
//! [`split`] on top of `tokio::net::UnixListener` / `UnixStream`.

use std::path::PathBuf;

use tokio::net::{UnixListener, UnixStream};

use crate::error::ServiceError;

// ── Type aliases ────────────────────────────────────────────────────────────

/// Platform-specific listener type (Unix: [`UnixListener`]).
pub type TransportListener = UnixListener;

/// Platform-specific stream type (Unix: [`UnixStream`]).
pub type TransportStream = UnixStream;

/// Read half of a transport stream.
pub type TransportReadHalf = tokio::io::ReadHalf<TransportStream>;

/// Write half of a transport stream.
pub type TransportWriteHalf = tokio::io::WriteHalf<TransportStream>;

// ── Transport operations ────────────────────────────────────────────────────

/// Bind a listener at the given socket path.
///
/// Removes any stale socket file at the given path before binding.
/// Refuses to bind if the path is a symlink (prevents symlink race attacks).
pub fn bind(path: PathBuf) -> Result<TransportListener, ServiceError> {
    // Check for symlinks (including dangling ones) and remove stale sockets.
    // Use symlink_metadata directly — do NOT gate on path.exists(), which
    // follows symlinks and returns false for dangling ones.
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ServiceError::Ipc(
                    "refusing to bind: socket path is a symlink".into(),
                ));
            }
            // Remove stale socket file from a previous run.
            std::fs::remove_file(&path)
                .map_err(|e| ServiceError::Ipc(format!("remove stale socket: {e}")))?;
        }
        Err(_) => {
            // Path does not exist — proceed to bind.
        }
    }
    UnixListener::bind(&path).map_err(|e| ServiceError::Ipc(format!("bind failed: {e}")))
}

/// Connect to a Unix domain socket at the given path.
pub async fn connect(path: &PathBuf) -> Result<TransportStream, ServiceError> {
    UnixStream::connect(path)
        .await
        .map_err(|e| ServiceError::Ipc(format!("connect failed: {e}")))
}

/// Pure allow/deny decision for a peer UID against our own UID.
///
/// Returns `true` iff `peer_uid == my_uid`. Extracted as a pure two-argument
/// function (deterministic, no hidden I/O) so that BOTH the Linux
/// (`SO_PEERCRED`) and macOS (`getpeereid`) arms of [`peer_cred_check`] route
/// the decision through ONE site. Each arm only extracts `peer_uid` via its
/// platform syscall; the equality check is centralized here so the two arms
/// can never drift out of sync (symmetric-wiring hazard).
fn peer_uid_allowed(peer_uid: libc::uid_t, my_uid: libc::uid_t) -> bool {
    peer_uid == my_uid
}

/// Check that the peer on the given stream has the same UID as the current
/// process. Rejects connections from different users.
///
/// On Linux this uses `getsockopt(SO_PEERCRED)`, on macOS `getpeereid()`.
/// On other Unix platforms, a warning is logged and the check is skipped.
pub fn peer_cred_check(stream: &TransportStream) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let peer_uid = unsafe {
            let mut creds: libc::ucred = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let ret = libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut creds as *mut _ as *mut _,
                &mut len,
            );
            if ret == -1 {
                log::warn!("IPC: failed to get peer credentials, rejecting connection");
                return false;
            }
            creds.uid
        };
        let my_uid = unsafe { libc::getuid() };
        if !peer_uid_allowed(peer_uid, my_uid) {
            log::warn!("IPC connection rejected: uid mismatch ({peer_uid} != {my_uid})");
            return false;
        }
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let mut peer_uid: libc::uid_t = 0;
        let mut peer_gid: libc::gid_t = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) } != 0 {
            log::warn!("IPC: failed to get peer credentials via getpeereid, rejecting connection");
            return false;
        }
        let my_uid = unsafe { libc::getuid() };
        if !peer_uid_allowed(peer_uid, my_uid) {
            log::warn!("IPC connection rejected: uid mismatch ({peer_uid} != {my_uid})");
            return false;
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = stream;
        log::warn!("IPC: no peer credential check available on this platform");
    }

    true
}

/// Remove the socket file at the given path.
///
/// Used during graceful shutdown to clean up the listener socket.
pub fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
}

/// Accept a client connection on the Unix domain socket listener.
///
/// Wraps [`UnixListener::accept`] and discards the peer address.
pub async fn accept(listener: &mut TransportListener) -> Result<TransportStream, std::io::Error> {
    let (stream, _addr) = listener.accept().await?;
    Ok(stream)
}

/// Split a transport stream into read and write halves using
/// [`tokio::io::split`] (not `into_split`) for cross-platform compatibility.
pub fn split(stream: TransportStream) -> (TransportReadHalf, TransportWriteHalf) {
    tokio::io::split(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BGS-29: the control socket must reject a foreign UID.
    ///
    /// `peer_uid_allowed` is platform-independent integer equality, so this
    /// single test pins the allow/deny decision for BOTH the Linux
    /// (SO_PEERCRED) and macOS (getpeereid) arms — each arm only extracts
    /// `peer_uid` via its platform syscall; the decision is centralized in
    /// `peer_uid_allowed`. `wrapping_add`/`wrapping_sub` avoid a root/0
    /// collision and u32 overflow.
    #[test]
    fn bgs29_control_socket_rejects_foreign_uid() {
        let my_uid: libc::uid_t = unsafe { libc::getuid() };
        // Same-UID connection is allowed.
        assert!(
            peer_uid_allowed(my_uid, my_uid),
            "same-UID peer must be allowed"
        );
        // A foreign UID (my_uid + 1) is rejected.
        assert!(
            !peer_uid_allowed(my_uid.wrapping_add(1), my_uid),
            "foreign UID (my_uid + 1) must be rejected"
        );
        // A different foreign UID (my_uid - 1) is also rejected.
        assert!(
            !peer_uid_allowed(my_uid.wrapping_sub(1), my_uid),
            "foreign UID (my_uid - 1) must be rejected"
        );
    }
}
