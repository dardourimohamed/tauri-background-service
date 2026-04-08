//! IPC protocol types and framing for desktop OS service mode.
//!
//! The desktop OS service uses length-prefixed JSON over Unix domain sockets
//! (Linux/macOS) or named pipes (Windows) for IPC between the GUI process
//! and the headless service process.

use serde::{Deserialize, Serialize};

/// Maximum allowed frame size (16 MB).
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// IPC request sent from the GUI process to the headless service.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
#[non_exhaustive]
pub enum IpcRequest {
    /// Start the background service with the given config.
    Start {
        /// Startup configuration forwarded from the plugin.
        config: crate::models::StartConfig,
    },
    /// Stop the running background service.
    Stop,
    /// Query whether a background service is currently running.
    IsRunning,
}

/// IPC response sent from the headless service to the GUI process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpcResponse {
    /// Whether the request succeeded.
    pub ok: bool,
    /// Optional data payload on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Optional error message on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// IPC event streamed from the headless service to the GUI process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
#[non_exhaustive]
pub enum IpcEvent {
    /// Service started successfully.
    Started,
    /// Service stopped.
    Stopped { reason: String },
    /// Service encountered an error.
    Error { message: String },
}

/// Encode a message into a length-prefixed JSON frame.
///
/// Frame format: `[4-byte big-endian u32 length][JSON payload]`
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, serde_json::Error> {
    let json = serde_json::to_vec(msg)?;
    let len = json.len() as u32;
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// Decode a length-prefixed JSON frame.
///
/// Returns an error if:
/// - The frame is shorter than 4 bytes (missing length prefix)
/// - The payload length exceeds `MAX_FRAME_SIZE`
/// - The JSON payload cannot be deserialized
pub fn decode_frame<T: serde::de::DeserializeOwned>(data: &[u8]) -> Result<T, FrameError> {
    if data.len() < 4 {
        return Err(FrameError::IncompleteLength);
    }
    let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(FrameError::TooLarge {
            size: len,
            max: MAX_FRAME_SIZE,
        });
    }
    let payload = data.get(4..4 + len).ok_or(FrameError::IncompletePayload {
        expected: len,
        available: data.len().saturating_sub(4),
    })?;
    serde_json::from_slice(payload).map_err(FrameError::Json)
}

/// Errors that can occur during frame decoding.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FrameError {
    /// The frame is shorter than 4 bytes.
    #[error("incomplete length prefix: need 4 bytes")]
    IncompleteLength,
    /// The payload exceeds the maximum frame size.
    #[error("frame too large: {size} bytes (max {max})")]
    TooLarge { size: usize, max: usize },
    /// The available data is shorter than the declared payload length.
    #[error("incomplete payload: expected {expected} bytes, got {available}")]
    IncompletePayload { expected: usize, available: usize },
    /// The JSON payload could not be deserialized.
    #[error("JSON decode error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Derive the IPC socket path for a given service label.
///
/// - Linux: `$XDG_RUNTIME_DIR/{label}.sock` (fallback: `/run/user/{uid}/{label}.sock`)
/// - macOS: `/tmp/{label}.sock`
/// - Windows: `\\.\pipe\{label}`
///
/// # Panics
///
/// Panics if the label contains path separators (`/`, `\`) or `..` components,
/// which could be used for path traversal attacks.
pub fn socket_path(label: &str) -> std::path::PathBuf {
    sanitize_label(label);
    #[cfg(target_os = "linux")]
    {
        let dir = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
        std::path::PathBuf::from(format!("{dir}/{label}.sock"))
    }
    #[cfg(target_os = "macos")]
    {
        std::path::PathBuf::from(format!("/tmp/{label}.sock"))
    }
    #[cfg(windows)]
    {
        std::path::PathBuf::from(format!(r"\\.\pipe\{label}"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        std::path::PathBuf::from(format!("/tmp/{label}.sock"))
    }
}

/// Validate that a service label does not contain path traversal characters.
fn sanitize_label(label: &str) {
    assert!(
        !label.is_empty(),
        "service label must not be empty"
    );
    assert!(
        !label.contains('/') && !label.contains('\\'),
        "service label must not contain path separators: {label}"
    );
    assert!(
        !label.contains(".."),
        "service label must not contain '..': {label}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- IpcRequest serde roundtrip tests ---

    #[test]
    fn ipc_request_start_serde_roundtrip() {
        let req = IpcRequest::Start {
            config: crate::models::StartConfig {
                service_label: "Syncing".into(),
                foreground_service_type: "dataSync".into(),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        let de: IpcRequest = serde_json::from_str(&json).unwrap();
        match de {
            IpcRequest::Start { config } => {
                assert_eq!(config.service_label, "Syncing");
                assert_eq!(config.foreground_service_type, "dataSync");
            }
            other => panic!("Expected Start, got {other:?}"),
        }
    }

    #[test]
    fn ipc_request_start_json_tag() {
        let req = IpcRequest::Start {
            config: crate::models::StartConfig::default(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"start\""), "Tagged JSON: {json}");
    }

    #[test]
    fn ipc_request_stop_serde_roundtrip() {
        let req = IpcRequest::Stop;
        let json = serde_json::to_string(&req).unwrap();
        let de: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(de, IpcRequest::Stop));
    }

    #[test]
    fn ipc_request_stop_json_tag() {
        let req = IpcRequest::Stop;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"stop\""), "Tagged JSON: {json}");
    }

    #[test]
    fn ipc_request_is_running_serde_roundtrip() {
        let req = IpcRequest::IsRunning;
        let json = serde_json::to_string(&req).unwrap();
        let de: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(de, IpcRequest::IsRunning));
    }

    #[test]
    fn ipc_request_is_running_json_tag() {
        let req = IpcRequest::IsRunning;
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            json.contains("\"type\":\"isRunning\""),
            "Tagged JSON: {json}"
        );
    }

    // --- IpcResponse serde roundtrip tests ---

    #[test]
    fn ipc_response_success_roundtrip() {
        let resp = IpcResponse {
            ok: true,
            data: Some(serde_json::json!({"running": true})),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let de: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(de.ok);
        assert_eq!(de.data.unwrap()["running"], true);
        assert!(de.error.is_none());
    }

    #[test]
    fn ipc_response_error_roundtrip() {
        let resp = IpcResponse {
            ok: false,
            data: None,
            error: Some("Service is already running".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let de: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(!de.ok);
        assert!(de.data.is_none());
        assert_eq!(de.error.unwrap(), "Service is already running");
    }

    #[test]
    fn ipc_response_skips_none_fields() {
        let resp = IpcResponse {
            ok: true,
            data: None,
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            !json.contains("\"data\""),
            "Should skip null data: {json}"
        );
        assert!(
            !json.contains("\"error\""),
            "Should skip null error: {json}"
        );
    }

    #[test]
    fn ipc_response_camel_case_keys() {
        let resp = IpcResponse {
            ok: true,
            data: None,
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"ok\""), "ok key: {json}");
    }

    // --- IpcEvent serde roundtrip tests ---

    #[test]
    fn ipc_event_started_serde_roundtrip() {
        let event = IpcEvent::Started;
        let json = serde_json::to_string(&event).unwrap();
        let de: IpcEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(de, IpcEvent::Started));
    }

    #[test]
    fn ipc_event_started_json_tag() {
        let event = IpcEvent::Started;
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"started\""), "Tagged JSON: {json}");
    }

    #[test]
    fn ipc_event_stopped_serde_roundtrip() {
        let event = IpcEvent::Stopped {
            reason: "cancelled".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let de: IpcEvent = serde_json::from_str(&json).unwrap();
        match de {
            IpcEvent::Stopped { reason } => assert_eq!(reason, "cancelled"),
            other => panic!("Expected Stopped, got {other:?}"),
        }
    }

    #[test]
    fn ipc_event_stopped_json_keys() {
        let event = IpcEvent::Stopped {
            reason: "done".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"stopped\""), "Tag: {json}");
        assert!(json.contains("\"reason\":\"done\""), "Reason: {json}");
    }

    #[test]
    fn ipc_event_error_serde_roundtrip() {
        let event = IpcEvent::Error {
            message: "init failed".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let de: IpcEvent = serde_json::from_str(&json).unwrap();
        match de {
            IpcEvent::Error { message } => assert_eq!(message, "init failed"),
            other => panic!("Expected Error, got {other:?}"),
        }
    }

    #[test]
    fn ipc_event_error_json_keys() {
        let event = IpcEvent::Error {
            message: "oops".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"error\""), "Tag: {json}");
        assert!(json.contains("\"message\":\"oops\""), "Message: {json}");
    }

    // --- Frame encode/decode tests ---

    #[test]
    fn ipc_frame_encode_decode_request() {
        let req = IpcRequest::Start {
            config: crate::models::StartConfig::default(),
        };
        let encoded = encode_frame(&req).unwrap();
        let decoded: IpcRequest = decode_frame(&encoded).unwrap();
        match decoded {
            IpcRequest::Start { config } => {
                assert_eq!(config.service_label, "Service running");
            }
            other => panic!("Expected Start, got {other:?}"),
        }
    }

    #[test]
    fn ipc_frame_encode_decode_response() {
        let resp = IpcResponse {
            ok: true,
            data: Some(serde_json::json!(42)),
            error: None,
        };
        let encoded = encode_frame(&resp).unwrap();
        let decoded: IpcResponse = decode_frame(&encoded).unwrap();
        assert!(decoded.ok);
        assert_eq!(decoded.data.unwrap(), 42);
    }

    #[test]
    fn ipc_frame_encode_decode_event() {
        let event = IpcEvent::Stopped {
            reason: "done".into(),
        };
        let encoded = encode_frame(&event).unwrap();
        let decoded: IpcEvent = decode_frame(&encoded).unwrap();
        match decoded {
            IpcEvent::Stopped { reason } => assert_eq!(reason, "done"),
            other => panic!("Expected Stopped, got {other:?}"),
        }
    }

    #[test]
    fn ipc_frame_length_prefix_is_big_endian() {
        let req = IpcRequest::Stop;
        let encoded = encode_frame(&req).unwrap();
        // First 4 bytes are the length of the JSON payload
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        assert_eq!(len as usize, encoded.len() - 4);
    }

    #[test]
    fn ipc_frame_too_large_rejected() {
        // Create a frame with length prefix claiming > 16MB
        let fake_len = (MAX_FRAME_SIZE + 1) as u32;
        let mut data = vec![0u8; 4 + 1];
        data[0..4].copy_from_slice(&fake_len.to_be_bytes());
        data[4] = b'{';
        let result: Result<IpcRequest, FrameError> = decode_frame(&data);
        match result {
            Err(FrameError::TooLarge { size, max }) => {
                assert_eq!(size, MAX_FRAME_SIZE + 1);
                assert_eq!(max, MAX_FRAME_SIZE);
            }
            other => panic!("Expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn ipc_frame_incomplete_length() {
        let data = [0u8; 3]; // Only 3 bytes, need 4
        let result: Result<IpcRequest, FrameError> = decode_frame(&data);
        assert!(
            matches!(result, Err(FrameError::IncompleteLength)),
            "Expected IncompleteLength, got {result:?}"
        );
    }

    #[test]
    fn ipc_frame_malformed_json() {
        // Valid length prefix (3 bytes) + invalid JSON
        let payload = b"{invalid";
        let mut data = Vec::with_capacity(4 + payload.len());
        data.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        data.extend_from_slice(payload);
        let result: Result<IpcRequest, FrameError> = decode_frame(&data);
        match result {
            Err(FrameError::Json(_)) => {} // expected
            other => panic!("Expected Json error, got {other:?}"),
        }
    }

    #[test]
    fn ipc_frame_incomplete_payload() {
        // Length says 100 bytes but only 1 byte available
        let mut data = vec![0u8; 5];
        data[0..4].copy_from_slice(&100u32.to_be_bytes());
        data[4] = b'{';
        let result: Result<IpcRequest, FrameError> = decode_frame(&data);
        match result {
            Err(FrameError::IncompletePayload {
                expected,
                available,
            }) => {
                assert_eq!(expected, 100);
                assert_eq!(available, 1);
            }
            other => panic!("Expected IncompletePayload, got {other:?}"),
        }
    }

    // --- socket_path tests ---

    #[test]
    fn socket_path_unix_format() {
        let path = socket_path("com.example.svc");
        #[cfg(target_os = "linux")]
        {
            // Should be under XDG_RUNTIME_DIR or /run/user/{uid}
            let path_str = path.to_str().unwrap();
            assert!(
                path_str.ends_with("/com.example.svc.sock"),
                "Expected path ending with /com.example.svc.sock, got: {path_str}"
            );
            if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
                assert!(
                    path_str.starts_with(&xdg),
                    "Expected path under XDG_RUNTIME_DIR ({xdg}), got: {path_str}"
                );
            } else {
                assert!(
                    path_str.contains("/run/user/"),
                    "Expected fallback /run/user/ path, got: {path_str}"
                );
            }
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(path.to_str().unwrap(), "/tmp/com.example.svc.sock");
        }
    }

    #[test]
    fn socket_path_nonempty_label() {
        let path = socket_path("my-app");
        #[cfg(unix)]
        {
            assert!(
                path.to_str().unwrap().ends_with("my-app.sock"),
                "Expected path ending with my-app.sock, got: {:?}",
                path
            );
        }
    }

    #[test]
    #[should_panic(expected = "path separators")]
    fn socket_path_rejects_slash_in_label() {
        let _ = socket_path("../etc/passwd");
    }

    #[test]
    #[should_panic(expected = "must not contain '..'")]
    fn socket_path_rejects_dotdot_in_label() {
        let _ = socket_path("..");
    }

    #[test]
    #[should_panic(expected = "path separators")]
    fn socket_path_rejects_backslash_in_label() {
        let _ = socket_path("foo\\bar");
    }

    #[test]
    #[should_panic(expected = "empty")]
    fn socket_path_rejects_empty_label() {
        let _ = socket_path("");
    }
}
