//! Windows named pipe transport implementation.
//!
//! Provides [`bind`], [`accept`], [`connect`], [`peer_cred_check`], [`cleanup`],
//! and [`split`] on top of `tokio::net::windows::named_pipe`.
//!
//! Windows named pipes use a different connection model than Unix sockets:
//! - The server creates pipe **instances** (not a single listener socket).
//! - Each instance handles exactly one client connection.
//! - [`TransportListener`] holds the pipe path and the current pending instance.
//! - [`accept`] connects the pending instance, creates the next, and returns the
//!   connected server as a [`TransportStream`].
//!
//! Only available when the `desktop-service` Cargo feature is enabled and the
//! target platform is Windows.

use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};

use crate::error::ServiceError;

// ── TransportStream ─────────────────────────────────────────────────────────

/// Platform-specific stream type (Windows: wrapper for named pipe endpoints).
///
/// On Windows, the server and client ends of a named pipe are different types
/// (`NamedPipeServer` and `NamedPipeClient`). This enum unifies them behind a
/// single type that implements `AsyncRead + AsyncWrite`.
pub enum TransportStream {
    /// Server-side named pipe (from [`accept`]).
    Server(NamedPipeServer),
    /// Client-side named pipe (from [`connect`]).
    Client(NamedPipeClient),
}

impl AsyncRead for TransportStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TransportStream::Server(s) => Pin::new(s).poll_read(cx, buf),
            TransportStream::Client(c) => Pin::new(c).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TransportStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            TransportStream::Server(s) => Pin::new(s).poll_write(cx, buf),
            TransportStream::Client(c) => Pin::new(c).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TransportStream::Server(s) => Pin::new(s).poll_flush(cx),
            TransportStream::Client(c) => Pin::new(c).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TransportStream::Server(s) => Pin::new(s).poll_shutdown(cx),
            TransportStream::Client(c) => Pin::new(c).poll_shutdown(cx),
        }
    }
}

// ── Type aliases ────────────────────────────────────────────────────────────

/// Read half of a transport stream.
pub type TransportReadHalf = tokio::io::ReadHalf<TransportStream>;

/// Write half of a transport stream.
pub type TransportWriteHalf = tokio::io::WriteHalf<TransportStream>;

// ── TransportListener ───────────────────────────────────────────────────────

/// Windows named pipe listener.
///
/// Holds the pipe path and a pending server instance. Each call to [`accept`]
/// connects the current instance and creates a new one for the next client.
/// This follows the tokio-recommended pattern of always having at least one
/// pipe instance available to prevent `ERROR_PIPE_BUSY` / `NotFound` errors.
pub struct TransportListener {
    pipe_path: String,
    pending: Option<NamedPipeServer>,
}

// ── Transport operations ────────────────────────────────────────────────────

/// Create a named pipe listener at the given path.
///
/// The path should be in Windows named pipe format (e.g. `\\.\pipe\label`),
/// which is what [`crate::desktop::ipc::socket_path`] returns on Windows.
///
/// Creates the first pipe instance immediately with `first_pipe_instance(true)`.
pub fn bind(path: PathBuf) -> Result<TransportListener, ServiceError> {
    let pipe_path = path.to_string_lossy().into_owned();
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_path)
        .map_err(|e| ServiceError::Ipc(format!("bind failed: {e}")))?;
    Ok(TransportListener {
        pipe_path,
        pending: Some(server),
    })
}

/// Accept a client connection on the named pipe listener.
///
/// Connects the current pending server instance, creates a new instance for
/// the next client, and returns the connected server as a [`TransportStream`].
pub async fn accept(listener: &mut TransportListener) -> Result<TransportStream, std::io::Error> {
    let server = listener.pending.take().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotConnected, "no pending pipe instance")
    })?;

    // Wait for a client to connect.
    server.connect().await?;

    // Create the next instance immediately so new clients never see NotFound.
    let next = ServerOptions::new().create(&listener.pipe_path)?;
    listener.pending = Some(next);

    Ok(TransportStream::Server(server))
}

/// Connect to a named pipe at the given path.
///
/// Retries up to 5 times on `ERROR_PIPE_BUSY` with exponential backoff
/// (50ms, 100ms, 200ms, 400ms, 800ms).
pub async fn connect(path: &PathBuf) -> Result<TransportStream, ServiceError> {
    const ERROR_PIPE_BUSY: i32 = 231;

    for attempt in 0u32..5 {
        match ClientOptions::new().open(path) {
            Ok(client) => return Ok(TransportStream::Client(client)),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                let delay = std::time::Duration::from_millis(50 * 2u64.pow(attempt));
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(ServiceError::Ipc(format!("connect failed: {e}"))),
        }
    }

    Err(ServiceError::Ipc(
        "connect failed: pipe busy after retries".into(),
    ))
}

/// Check peer credentials on a named pipe connection.
///
/// On Windows, named pipe security is enforced by the OS through the pipe's
/// discretionary access control list (DACL). By default, only the creating
/// user and local administrators can connect. This function always returns
/// `true` — the security check happens at connection time, not afterward.
pub fn peer_cred_check(_stream: &TransportStream) -> bool {
    true
}

/// Clean up the named pipe at the given path.
///
/// Named pipes are kernel objects, not filesystem entries — there is nothing
/// to remove. This function is a no-op.
pub fn cleanup(_path: &PathBuf) {
    // Named pipes are not filesystem objects — nothing to remove.
}

/// Split a transport stream into read and write halves using
/// [`tokio::io::split`] (not `into_split`) for cross-platform compatibility.
pub fn split(stream: TransportStream) -> (TransportReadHalf, TransportWriteHalf) {
    tokio::io::split(stream)
}
