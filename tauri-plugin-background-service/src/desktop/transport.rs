//! Platform-agnostic IPC transport layer.
//!
//! Provides type aliases, framing functions, and platform-specific connection
//! primitives that abstract over Unix domain sockets (Linux/macOS) and named
//! pipes (Windows).
//!
//! Only available when the `desktop-service` Cargo feature is enabled.

// Platform-specific submodules are declared from mod.rs with #[cfg] gates.
// This file provides the generic framing functions used by all platforms.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::desktop::ipc::MAX_FRAME_SIZE;

// ── Generic framing ─────────────────────────────────────────────────────────

/// Read a single length-prefixed frame from an async reader.
///
/// Returns the payload bytes (without the 4-byte length prefix).
/// Returns `None` on clean EOF (0 bytes available for the length read).
///
/// # Errors
///
/// Returns an error string for:
/// - Frames exceeding [`MAX_FRAME_SIZE`]
/// - Zero-length frames (protocol violation)
/// - I/O errors
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>, String> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(format!("read frame: {e}")),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(format!("frame too large: {len}"));
    }
    if len == 0 {
        return Err("zero-length frame".into());
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|e| format!("read payload: {e}"))?;
    Ok(Some(payload))
}

/// Write a pre-encoded frame (with length prefix) to an async writer.
///
/// The caller is responsible for encoding the frame (adding the 4-byte
/// big-endian length prefix) before calling this function.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &[u8],
) -> Result<(), String> {
    writer
        .write_all(frame)
        .await
        .map_err(|e| format!("write frame: {e}"))?;
    Ok(())
}

// ── Platform-specific re-exports ─────────────────────────────────────────────
//
// Consumers import from `transport` instead of the platform submodule directly,
// making it easy to swap Unix ↔ Windows when the named-pipe transport lands.

#[cfg(unix)]
pub use crate::desktop::transport_unix::{
    accept, bind, cleanup, connect, peer_cred_check, split, TransportListener, TransportReadHalf,
    TransportStream, TransportWriteHalf,
};

#[cfg(windows)]
pub use crate::desktop::transport_windows::{
    accept, bind, cleanup, connect, peer_cred_check, split, TransportListener, TransportReadHalf,
    TransportStream, TransportWriteHalf,
};
