//! IPC protocol types and framing for desktop OS service mode.
//!
//! The desktop OS service uses length-prefixed JSON over Unix domain sockets
//! (Linux/macOS) or named pipes (Windows) for IPC between the GUI process
//! and the headless service process.

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

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
    /// Query the current service lifecycle state.
    GetState,
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

/// Tagged wrapper for all IPC messages on the wire.
///
/// Uses `#[serde(tag = "kind")]` for single-point deserialization — no
/// trial-and-error dispatch needed. The reader deserializes once and matches
/// on the variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum IpcMessage {
    /// A request from the GUI process to the headless service.
    Request(IpcRequest),
    /// A response from the headless service to the GUI process.
    Response(IpcResponse),
    /// An event streamed from the headless service to the GUI process.
    Event(IpcEvent),
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

/// Decode an [`IpcMessage`] from raw payload bytes.
///
/// Takes payload bytes (without length prefix) and deserializes as
/// [`IpcMessage`]. The caller matches on the variant to extract the
/// concrete type. Length-prefix reading and frame-size validation are
/// handled by `read_frame` / `read_frame_from` at the stream level.
pub fn decode_frame(payload: &[u8]) -> Result<IpcMessage, serde_json::Error> {
    serde_json::from_slice(payload)
}

/// Derive the IPC socket path for a given service label.
///
/// - Linux: `$XDG_RUNTIME_DIR/{label}.sock` (fallback: `/run/user/{uid}/{label}.sock`)
/// - macOS: `/tmp/{label}.sock`
/// - Windows: `\\.\pipe\{label}`
///
/// # Errors
///
/// Returns `ServiceError::Init` if the label is empty, contains path
/// separators, or contains `..` components.
pub fn socket_path(label: &str) -> Result<std::path::PathBuf, ServiceError> {
    sanitize_label(label)?;
    #[cfg(target_os = "linux")]
    {
        let dir = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
        Ok(std::path::PathBuf::from(format!("{dir}/{label}.sock")))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(std::path::PathBuf::from(format!("/tmp/{label}.sock")))
    }
    #[cfg(windows)]
    {
        Ok(std::path::PathBuf::from(format!(r"\\.\pipe\{label}")))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Ok(std::path::PathBuf::from(format!("/tmp/{label}.sock")))
    }
}

/// Validate that a service label does not contain path traversal characters.
fn sanitize_label(label: &str) -> Result<std::path::PathBuf, ServiceError> {
    if label.is_empty() {
        return Err(ServiceError::Init("service label must not be empty".into()));
    }
    if label.contains('/') || label.contains('\\') {
        return Err(ServiceError::Init(format!(
            "service label must not contain path separators: {label}"
        )));
    }
    if label.contains("..") {
        return Err(ServiceError::Init(format!(
            "service label must not contain '..': {label}"
        )));
    }
    Ok(std::path::PathBuf::from(label))
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

    // --- IpcRequest::GetState serde tests ---

    #[test]
    fn ipc_request_get_state_serde_roundtrip() {
        let req = IpcRequest::GetState;
        let json = serde_json::to_string(&req).unwrap();
        let de: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(de, IpcRequest::GetState));
    }

    #[test]
    fn ipc_request_get_state_json_tag() {
        let req = IpcRequest::GetState;
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            json.contains("\"type\":\"getState\""),
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
        assert!(!json.contains("\"data\""), "Should skip null data: {json}");
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
        let msg = IpcMessage::Request(req);
        let encoded = encode_frame(&msg).unwrap();
        let decoded = decode_frame(&encoded[4..]).unwrap();
        match decoded {
            IpcMessage::Request(IpcRequest::Start { config }) => {
                assert_eq!(config.service_label, "Service running");
            }
            other => panic!("Expected Request(Start), got {other:?}"),
        }
    }

    #[test]
    fn ipc_frame_encode_decode_response() {
        let resp = IpcResponse {
            ok: true,
            data: Some(serde_json::json!(42)),
            error: None,
        };
        let msg = IpcMessage::Response(resp);
        let encoded = encode_frame(&msg).unwrap();
        let decoded = decode_frame(&encoded[4..]).unwrap();
        match decoded {
            IpcMessage::Response(r) => {
                assert!(r.ok);
                assert_eq!(r.data.unwrap(), 42);
            }
            other => panic!("Expected Response, got {other:?}"),
        }
    }

    #[test]
    fn ipc_frame_encode_decode_event() {
        let event = IpcEvent::Stopped {
            reason: "done".into(),
        };
        let msg = IpcMessage::Event(event);
        let encoded = encode_frame(&msg).unwrap();
        let decoded = decode_frame(&encoded[4..]).unwrap();
        match decoded {
            IpcMessage::Event(IpcEvent::Stopped { reason }) => assert_eq!(reason, "done"),
            other => panic!("Expected Event(Stopped), got {other:?}"),
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
    fn ipc_frame_decode_payload_without_length_prefix() {
        // Verify decode_frame works with payload-only bytes (no length prefix).
        let resp = IpcResponse {
            ok: true,
            data: Some(serde_json::json!({"status": "ok"})),
            error: None,
        };
        let msg = IpcMessage::Response(resp);
        let payload = serde_json::to_vec(&msg).unwrap();
        let decoded = decode_frame(&payload).unwrap();
        match decoded {
            IpcMessage::Response(r) => {
                assert!(r.ok);
                assert_eq!(r.data.unwrap()["status"], "ok");
            }
            other => panic!("Expected Response, got {other:?}"),
        }
    }

    #[test]
    fn ipc_frame_malformed_json() {
        let payload = b"{invalid";
        let result = decode_frame(payload);
        assert!(result.is_err(), "Expected JSON error for malformed payload");
    }

    // --- IpcMessage wrapper enum tests ---

    #[test]
    fn ipc_message_response_roundtrip() {
        let msg = IpcMessage::Response(IpcResponse {
            ok: true,
            data: Some(serde_json::json!({"running": true})),
            error: None,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"kind\":\"response\""), "kind tag: {json}");
        let de: IpcMessage = serde_json::from_str(&json).unwrap();
        match de {
            IpcMessage::Response(resp) => {
                assert!(resp.ok);
                assert_eq!(resp.data.unwrap()["running"], true);
            }
            other => panic!("Expected Response, got {other:?}"),
        }
    }

    #[test]
    fn ipc_message_event_roundtrip() {
        let msg = IpcMessage::Event(IpcEvent::Stopped {
            reason: "cancelled".into(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"kind\":\"event\""), "kind tag: {json}");
        let de: IpcMessage = serde_json::from_str(&json).unwrap();
        match de {
            IpcMessage::Event(IpcEvent::Stopped { reason }) => {
                assert_eq!(reason, "cancelled");
            }
            other => panic!("Expected Event, got {other:?}"),
        }
    }

    #[test]
    fn ipc_message_ambiguous_frame_deterministic() {
        // A frame with fields that overlap between Response and Event
        // must be deserialized deterministically based on the "kind" tag.
        let json = r#"{"kind":"event","type":"started","ok":true}"#;
        let de: IpcMessage = serde_json::from_str(json).unwrap();
        match de {
            IpcMessage::Event(IpcEvent::Started) => {} // correct
            other => panic!("Expected Event::Started, got {other:?}"),
        }

        // Same frame with kind=response must deserialize as Response
        let json2 = r#"{"kind":"response","ok":true,"data":{"type":"started"}}"#;
        let de2: IpcMessage = serde_json::from_str(json2).unwrap();
        match de2 {
            IpcMessage::Response(resp) => {
                assert!(resp.ok);
            }
            other => panic!("Expected Response, got {other:?}"),
        }
    }

    #[test]
    fn ipc_message_unknown_kind_rejected() {
        let json = r#"{"kind":"unknown","ok":true}"#;
        let result: Result<IpcMessage, _> = serde_json::from_str(json);
        assert!(result.is_err(), "Expected error for unknown kind value");
    }

    // --- socket_path tests ---

    #[test]
    fn socket_path_unix_format() {
        let path = socket_path("com.example.svc").unwrap();
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
        let path = socket_path("my-app").unwrap();
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
    fn socket_path_rejects_slash_in_label() {
        let result = socket_path("../etc/passwd");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("path separators"), "Error: {err}");
    }

    #[test]
    fn socket_path_rejects_dotdot_in_label() {
        let result = socket_path("..");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("'..'"), "Error: {err}");
    }

    #[test]
    fn socket_path_rejects_backslash_in_label() {
        let result = socket_path("foo\\bar");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("path separators"), "Error: {err}");
    }

    #[test]
    fn socket_path_rejects_empty_label() {
        let result = socket_path("");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("empty"), "Error: {err}");
    }
}
