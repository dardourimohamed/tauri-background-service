//! Data types shared between the plugin's Rust core and the JS/Tauri layer.
//!
//! - [`ServiceContext`] is passed to every [`BackgroundService`](crate::BackgroundService) method.
//! - [`StartConfig`] and [`PluginConfig`] control service and plugin behaviour.
//! - [`PluginEvent`] represents events emitted to the JavaScript front-end.

use serde::{Deserialize, Serialize};
use tauri::Runtime;
use tokio_util::sync::CancellationToken;

use crate::error::ServiceError;
use crate::notifier::Notifier;

/// The 14 valid Android foreground service types.
///
/// Sourced from <https://developer.android.com/about/versions/14/changes/fg-types>.
/// Unknown types are rejected with [`ServiceError::Platform`] at both the Rust
/// and Kotlin layers.
pub const VALID_FOREGROUND_SERVICE_TYPES: &[&str] = &[
    "dataSync",
    "mediaPlayback",
    "phoneCall",
    "location",
    "connectedDevice",
    "mediaProjection",
    "camera",
    "microphone",
    "health",
    "remoteMessaging",
    "systemExempted",
    "shortService",
    "specialUse",
    "mediaProcessing",
];

/// Validate a foreground service type against the allowlist.
///
/// Returns `Ok(())` if the type is one of the 14 valid Android foreground
/// service types, or `Err(ServiceError::Platform)` for unknown strings.
pub fn validate_foreground_service_type(t: &str) -> Result<(), ServiceError> {
    if VALID_FOREGROUND_SERVICE_TYPES.contains(&t) {
        Ok(())
    } else {
        Err(ServiceError::Platform(format!(
            "invalid foreground_service_type '{}'. Valid types: {:?}",
            t, VALID_FOREGROUND_SERVICE_TYPES
        )))
    }
}

/// Passed into both `init` and `run`.
/// Gives your service everything it needs to interact with the outside world.
pub struct ServiceContext<R: Runtime> {
    /// Fire a local notification. Works on all platforms.
    pub notifier: Notifier<R>,

    /// Emit an event to the JS UI layer.
    pub app: tauri::AppHandle<R>,

    /// Cancelled when `stopService()` is called.
    pub shutdown: CancellationToken,

    /// Text shown in the Android persistent notification.
    /// Only available on mobile platforms.
    #[cfg(mobile)]
    pub service_label: String,

    /// Android foreground service type (e.g. "dataSync", "specialUse").
    /// Only available on mobile platforms.
    #[cfg(mobile)]
    pub foreground_service_type: String,
}

/// Optional startup configuration forwarded from JS through the plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartConfig {
    /// Text shown in the Android persistent foreground notification.
    #[serde(default = "default_label")]
    pub service_label: String,

    /// Android foreground service type (e.g. "dataSync", "specialUse").
    #[serde(default = "default_foreground_service_type")]
    pub foreground_service_type: String,
}

fn default_label() -> String {
    "Service running".into()
}

fn default_foreground_service_type() -> String {
    "dataSync".into()
}

/// Plugin-level configuration, deserialized from the Tauri plugin config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginConfig {
    /// iOS safety timeout in seconds for the expiration handler.
    /// Default: 28.0 (Apple recommends keeping BG tasks under ~30s).
    #[serde(default = "default_ios_safety_timeout")]
    pub ios_safety_timeout_secs: f64,

    /// iOS cancel listener timeout in seconds.
    /// Default: 14400 (4 hours). Balances leak risk vs. service lifetime.
    #[serde(default = "default_ios_cancel_listener_timeout_secs")]
    pub ios_cancel_listener_timeout_secs: u64,

    /// iOS BGProcessingTask safety timeout in seconds.
    /// Default: 0.0 (no cap). Processing tasks can run for minutes/hours,
    /// so unlike BGAppRefreshTask (28s default), this defaults to uncapped.
    /// Set to a positive value to impose a hard cap on processing task duration.
    #[serde(default = "default_ios_processing_safety_timeout_secs")]
    pub ios_processing_safety_timeout_secs: f64,

    /// iOS `BGAppRefreshTask` earliest begin date in minutes from now.
    /// Default: 15.0. Controls how soon iOS can launch the refresh task.
    #[serde(default = "default_ios_earliest_refresh_begin_minutes")]
    pub ios_earliest_refresh_begin_minutes: f64,

    /// iOS `BGProcessingTask` earliest begin date in minutes from now.
    /// Default: 15.0. Controls how soon iOS can launch the processing task.
    #[serde(default = "default_ios_earliest_processing_begin_minutes")]
    pub ios_earliest_processing_begin_minutes: f64,

    /// iOS `BGProcessingTask` requires external power.
    /// Default: false. When true, iOS only launches the processing task
    /// while the device is connected to power.
    #[serde(default)]
    pub ios_requires_external_power: bool,

    /// iOS `BGProcessingTask` requires network connectivity.
    /// Default: false. When true, iOS only launches the processing task
    /// when the device has network access.
    #[serde(default)]
    pub ios_requires_network_connectivity: bool,

    /// Capacity for the manager command channel (mpsc).
    /// Default: 16. Increase for high-throughput scenarios with many
    /// concurrent start/stop/is-running calls.
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,

    /// Desktop service mode: "inProcess" (default) or "osService".
    /// Controls whether the background service runs in-process or as a
    /// registered OS service/daemon.
    #[cfg(feature = "desktop-service")]
    #[serde(default = "default_desktop_service_mode")]
    pub desktop_service_mode: String,

    /// Optional custom label for the desktop OS service registration.
    /// When `None`, the label is auto-derived from the app identifier.
    #[cfg(feature = "desktop-service")]
    #[serde(default)]
    pub desktop_service_label: Option<String>,
}

fn default_ios_safety_timeout() -> f64 {
    28.0
}

fn default_ios_cancel_listener_timeout_secs() -> u64 {
    14400
}

fn default_ios_processing_safety_timeout_secs() -> f64 {
    0.0
}

fn default_ios_earliest_refresh_begin_minutes() -> f64 {
    15.0
}

fn default_ios_earliest_processing_begin_minutes() -> f64 {
    15.0
}

fn default_channel_capacity() -> usize {
    16
}

#[cfg(feature = "desktop-service")]
fn default_desktop_service_mode() -> String {
    "inProcess".into()
}

impl Default for StartConfig {
    fn default() -> Self {
        Self {
            service_label: default_label(),
            foreground_service_type: default_foreground_service_type(),
        }
    }
}

/// Lifecycle state of the background service.
///
/// Exposed via the `get-service-state` command to provide
/// fine-grained status beyond a simple boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ServiceState {
    /// No service has been started, or the last run has fully cleaned up.
    Idle,
    /// `init()` is in progress (between `Start` and successful `init()`).
    Initializing,
    /// `init()` succeeded; `run()` is executing.
    Running,
    /// Service stopped (by `stop()`, natural completion, or error).
    Stopped,
}

/// Snapshot of the service lifecycle status.
///
/// Returned by the `get-service-state` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    /// Current lifecycle state.
    pub state: ServiceState,
    /// Last error message, if the service stopped due to an error.
    pub last_error: Option<String>,
}

/// Built-in event types emitted by the runner itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
#[non_exhaustive]
pub enum PluginEvent {
    /// init() completed successfully
    Started,
    /// run() returned or was cancelled
    Stopped { reason: String },
    /// init() or run() returned an error
    Error { message: String },
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            ios_safety_timeout_secs: default_ios_safety_timeout(),
            ios_cancel_listener_timeout_secs: default_ios_cancel_listener_timeout_secs(),
            ios_processing_safety_timeout_secs: default_ios_processing_safety_timeout_secs(),
            ios_earliest_refresh_begin_minutes: default_ios_earliest_refresh_begin_minutes(),
            ios_earliest_processing_begin_minutes: default_ios_earliest_processing_begin_minutes(),
            ios_requires_external_power: false,
            ios_requires_network_connectivity: false,
            channel_capacity: default_channel_capacity(),
            #[cfg(feature = "desktop-service")]
            desktop_service_mode: default_desktop_service_mode(),
            #[cfg(feature = "desktop-service")]
            desktop_service_label: None,
        }
    }
}

/// Arguments sent to the native `startKeepalive` handler.
///
/// Lives in `models.rs` (not `mobile.rs`) so serde tests run on all platforms.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct StartKeepaliveArgs<'a> {
    pub label: &'a str,
    pub foreground_service_type: &'a str,
    /// iOS safety timeout in seconds. Only sent to iOS; `None` omits the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ios_safety_timeout_secs: Option<f64>,
    /// iOS BGProcessingTask safety timeout in seconds. Only sent to iOS; `None` omits the key.
    /// When `Some(positive)`, caps the processing task duration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ios_processing_safety_timeout_secs: Option<f64>,
    /// iOS BGAppRefreshTask earliest begin date in minutes. Only sent to iOS; `None` omits the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ios_earliest_refresh_begin_minutes: Option<f64>,
    /// iOS BGProcessingTask earliest begin date in minutes. Only sent to iOS; `None` omits the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ios_earliest_processing_begin_minutes: Option<f64>,
    /// iOS BGProcessingTask requires external power. Only sent to iOS; `None` omits the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ios_requires_external_power: Option<bool>,
    /// iOS BGProcessingTask requires network connectivity. Only sent to iOS; `None` omits the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ios_requires_network_connectivity: Option<bool>,
}

/// Auto-start config returned by the Kotlin bridge.
///
/// Deserialized from SharedPreferences values read by `getAutoStartConfig`.
/// Only used on Android (the iOS path doesn't have auto-start).
#[doc(hidden)]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoStartConfig {
    pub pending: bool,
    pub label: Option<String>,
    pub service_type: Option<String>,
}

impl AutoStartConfig {
    /// Convert to `StartConfig` if auto-start is pending and label is available.
    pub fn into_start_config(self) -> Option<StartConfig> {
        if self.pending {
            self.label.map(|label| StartConfig {
                service_label: label,
                foreground_service_type: self
                    .service_type
                    .unwrap_or_else(default_foreground_service_type),
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- StartConfig tests ---

    #[test]
    fn start_config_default_label() {
        let config = StartConfig::default();
        assert_eq!(config.service_label, "Service running");
    }

    #[test]
    fn start_config_custom_label() {
        let config = StartConfig {
            service_label: "Syncing data".into(),
            ..Default::default()
        };
        assert_eq!(config.service_label, "Syncing data");
    }

    #[test]
    fn start_config_serde_roundtrip_default() {
        let config = StartConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let de: StartConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.service_label, config.service_label);
    }

    #[test]
    fn start_config_serde_roundtrip_custom() {
        let config = StartConfig {
            service_label: "My service".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: StartConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.service_label, "My service");
    }

    #[test]
    fn start_config_deserialize_missing_field_uses_default() {
        // An empty JSON object should produce the default label
        let json = "{}";
        let de: StartConfig = serde_json::from_str(json).unwrap();
        assert_eq!(de.service_label, "Service running");
    }

    #[test]
    fn start_config_json_key_is_camel_case() {
        let config = StartConfig {
            service_label: "test".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("serviceLabel"),
            "JSON should use camelCase: {json}"
        );
    }

    // --- PluginEvent tests ---

    #[test]
    fn plugin_event_started_serde_roundtrip() {
        let event = PluginEvent::Started;
        let json = serde_json::to_string(&event).unwrap();
        let de: PluginEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(de, PluginEvent::Started));
    }

    #[test]
    fn plugin_event_stopped_serde_roundtrip() {
        let event = PluginEvent::Stopped {
            reason: "cancelled".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let de: PluginEvent = serde_json::from_str(&json).unwrap();
        match de {
            PluginEvent::Stopped { reason } => assert_eq!(reason, "cancelled"),
            other => panic!("Expected Stopped, got {other:?}"),
        }
    }

    #[test]
    fn plugin_event_error_serde_roundtrip() {
        let event = PluginEvent::Error {
            message: "init failed".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let de: PluginEvent = serde_json::from_str(&json).unwrap();
        match de {
            PluginEvent::Error { message } => assert_eq!(message, "init failed"),
            other => panic!("Expected Error, got {other:?}"),
        }
    }

    #[test]
    fn plugin_event_tagged_json_format() {
        let event = PluginEvent::Started;
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"started\""), "Tagged JSON: {json}");
    }

    #[test]
    fn plugin_event_stopped_json_keys_camel_case() {
        let event = PluginEvent::Stopped {
            reason: "done".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"stopped\""), "Tag: {json}");
        assert!(json.contains("\"reason\":\"done\""), "Reason: {json}");
    }

    #[test]
    fn plugin_event_error_json_keys_camel_case() {
        let event = PluginEvent::Error {
            message: "oops".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"error\""), "Tag: {json}");
        assert!(json.contains("\"message\":\"oops\""), "Message: {json}");
    }

    // --- StartConfig foreground_service_type tests ---

    #[test]
    fn start_config_default_service_type() {
        let config = StartConfig::default();
        assert_eq!(config.foreground_service_type, "dataSync");
    }

    #[test]
    fn start_config_custom_service_type() {
        let config = StartConfig {
            service_label: "test".into(),
            foreground_service_type: "specialUse".into(),
        };
        assert_eq!(config.foreground_service_type, "specialUse");
    }

    #[test]
    fn start_config_serde_roundtrip_service_type() {
        let config = StartConfig {
            service_label: "test".into(),
            foreground_service_type: "specialUse".into(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: StartConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.foreground_service_type, "specialUse");
    }

    #[test]
    fn start_config_deserialize_missing_service_type() {
        let json = r#"{"serviceLabel":"test"}"#;
        let de: StartConfig = serde_json::from_str(json).unwrap();
        assert_eq!(de.foreground_service_type, "dataSync");
    }

    #[test]
    fn start_config_deserialize_special_use() {
        let json = r#"{"serviceLabel":"test","foregroundServiceType":"specialUse"}"#;
        let de: StartConfig = serde_json::from_str(json).unwrap();
        assert_eq!(de.foreground_service_type, "specialUse");
    }

    #[test]
    fn start_config_unrecognized_type_rejected_by_validation() {
        // Deserialization still passes through any string.
        let json = r#"{"serviceLabel":"test","foregroundServiceType":"customType"}"#;
        let de: StartConfig = serde_json::from_str(json).unwrap();
        assert_eq!(de.foreground_service_type, "customType");
        // But validation rejects it.
        let result = validate_foreground_service_type(&de.foreground_service_type);
        assert!(
            result.is_err(),
            "validation should reject unrecognized type"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("customType"),
            "error should mention the invalid type: {err_msg}"
        );
    }

    #[test]
    fn start_config_json_key_is_camel_case_service_type() {
        let config = StartConfig {
            service_label: "test".into(),
            foreground_service_type: "specialUse".into(),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("foregroundServiceType"),
            "JSON should use camelCase: {json}"
        );
    }

    // --- AutoStartConfig tests ---

    #[test]
    fn auto_start_config_pending_with_label_returns_start_config() {
        let json = r#"{"pending": true, "label": "Syncing"}"#;
        let config: AutoStartConfig = serde_json::from_str(json).unwrap();
        let result = config.into_start_config();
        assert!(result.is_some());
        let start_config = result.unwrap();
        assert_eq!(start_config.service_label, "Syncing");
        assert_eq!(start_config.foreground_service_type, "dataSync");
    }

    #[test]
    fn auto_start_config_not_pending_returns_none() {
        let json = r#"{"pending": false, "label": null}"#;
        let config: AutoStartConfig = serde_json::from_str(json).unwrap();
        let result = config.into_start_config();
        assert!(result.is_none());
    }

    #[test]
    fn auto_start_config_pending_no_label_returns_none() {
        let json = r#"{"pending": true, "label": null}"#;
        let config: AutoStartConfig = serde_json::from_str(json).unwrap();
        let result = config.into_start_config();
        assert!(result.is_none());
    }

    #[test]
    fn auto_start_config_with_service_type_preserves_it() {
        let json = r#"{"pending":true,"label":"test","serviceType":"specialUse"}"#;
        let config: AutoStartConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.service_type, Some("specialUse".to_string()));
        let result = config.into_start_config();
        assert!(result.is_some());
        let start_config = result.unwrap();
        assert_eq!(start_config.foreground_service_type, "specialUse");
    }

    #[test]
    fn auto_start_config_without_service_type_uses_default() {
        let json = r#"{"pending":true,"label":"test"}"#;
        let config: AutoStartConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.service_type, None);
        let result = config.into_start_config();
        assert!(result.is_some());
        assert_eq!(result.unwrap().foreground_service_type, "dataSync");
    }

    #[test]
    fn auto_start_config_null_service_type_uses_default() {
        let json = r#"{"pending":true,"label":"test","serviceType":null}"#;
        let config: AutoStartConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.service_type, None);
        let result = config.into_start_config();
        assert!(result.is_some());
        assert_eq!(result.unwrap().foreground_service_type, "dataSync");
    }

    // --- PluginConfig tests ---

    #[test]
    fn plugin_config_default_ios_safety_timeout() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ios_safety_timeout_secs, 28.0);
    }

    #[test]
    fn plugin_config_custom_ios_safety_timeout() {
        let json = r#"{"iosSafetyTimeoutSecs":15.0}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ios_safety_timeout_secs, 15.0);
    }

    #[test]
    fn plugin_config_serde_roundtrip_preserves_value() {
        let config = PluginConfig {
            ios_safety_timeout_secs: 30.0,
            ios_cancel_listener_timeout_secs: 14400,
            ios_processing_safety_timeout_secs: 0.0,
            ios_earliest_refresh_begin_minutes: 20.0,
            ios_earliest_processing_begin_minutes: 30.0,
            ios_requires_external_power: true,
            ios_requires_network_connectivity: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.ios_safety_timeout_secs, 30.0);
        assert_eq!(de.ios_earliest_refresh_begin_minutes, 20.0);
        assert_eq!(de.ios_earliest_processing_begin_minutes, 30.0);
        assert!(de.ios_requires_external_power);
        assert!(de.ios_requires_network_connectivity);
    }

    #[test]
    fn plugin_config_default_impl() {
        let config = PluginConfig::default();
        assert_eq!(config.ios_safety_timeout_secs, 28.0);
        assert_eq!(config.channel_capacity, 16);
    }

    #[test]
    fn plugin_config_default_cancel_timeout() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ios_cancel_listener_timeout_secs, 14400);
    }

    #[test]
    fn plugin_config_custom_cancel_timeout() {
        let json = r#"{"iosCancelListenerTimeoutSecs":7200}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ios_cancel_listener_timeout_secs, 7200);
    }

    #[test]
    fn plugin_config_cancel_timeout_serde_roundtrip() {
        let config = PluginConfig {
            ios_cancel_listener_timeout_secs: 3600,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.ios_cancel_listener_timeout_secs, 3600);
    }

    // --- PluginConfig ios_processing_safety_timeout_secs tests ---

    #[test]
    fn plugin_config_processing_timeout_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ios_processing_safety_timeout_secs, 0.0);
    }

    #[test]
    fn plugin_config_processing_timeout_custom() {
        let json = r#"{"iosProcessingSafetyTimeoutSecs":60.0}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ios_processing_safety_timeout_secs, 60.0);
    }

    #[test]
    fn plugin_config_processing_timeout_serde_roundtrip() {
        let config = PluginConfig {
            ios_processing_safety_timeout_secs: 120.0,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.ios_processing_safety_timeout_secs, 120.0);
    }

    // --- StartKeepaliveArgs tests ---

    #[test]
    fn start_keepalive_args_with_timeout() {
        let args = StartKeepaliveArgs {
            label: "Test",
            foreground_service_type: "dataSync",
            ios_safety_timeout_secs: Some(15.0),
            ios_processing_safety_timeout_secs: None,
            ios_earliest_refresh_begin_minutes: None,
            ios_earliest_processing_begin_minutes: None,
            ios_requires_external_power: None,
            ios_requires_network_connectivity: None,
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(
            json.contains("\"iosSafetyTimeoutSecs\":15.0"),
            "JSON should contain iosSafetyTimeoutSecs: {json}"
        );
    }

    #[test]
    fn start_keepalive_args_without_timeout() {
        let args = StartKeepaliveArgs {
            label: "Test",
            foreground_service_type: "dataSync",
            ios_safety_timeout_secs: None,
            ios_processing_safety_timeout_secs: None,
            ios_earliest_refresh_begin_minutes: None,
            ios_earliest_processing_begin_minutes: None,
            ios_requires_external_power: None,
            ios_requires_network_connectivity: None,
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(
            !json.contains("iosSafetyTimeoutSecs"),
            "JSON should NOT contain iosSafetyTimeoutSecs when None: {json}"
        );
    }

    #[test]
    fn start_keepalive_args_processing_timeout() {
        let args = StartKeepaliveArgs {
            label: "Test",
            foreground_service_type: "dataSync",
            ios_safety_timeout_secs: None,
            ios_processing_safety_timeout_secs: Some(60.0),
            ios_earliest_refresh_begin_minutes: None,
            ios_earliest_processing_begin_minutes: None,
            ios_requires_external_power: None,
            ios_requires_network_connectivity: None,
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(
            json.contains("\"iosProcessingSafetyTimeoutSecs\":60.0"),
            "JSON should contain iosProcessingSafetyTimeoutSecs: {json}"
        );
    }

    #[test]
    fn start_keepalive_args_no_processing_timeout() {
        let args = StartKeepaliveArgs {
            label: "Test",
            foreground_service_type: "dataSync",
            ios_safety_timeout_secs: None,
            ios_processing_safety_timeout_secs: None,
            ios_earliest_refresh_begin_minutes: None,
            ios_earliest_processing_begin_minutes: None,
            ios_requires_external_power: None,
            ios_requires_network_connectivity: None,
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(
            !json.contains("iosProcessingSafetyTimeoutSecs"),
            "JSON should NOT contain iosProcessingSafetyTimeoutSecs when None: {json}"
        );
    }

    #[test]
    fn start_keepalive_args_camel_case_keys() {
        let args = StartKeepaliveArgs {
            label: "Test",
            foreground_service_type: "specialUse",
            ios_safety_timeout_secs: None,
            ios_processing_safety_timeout_secs: None,
            ios_earliest_refresh_begin_minutes: None,
            ios_earliest_processing_begin_minutes: None,
            ios_requires_external_power: None,
            ios_requires_network_connectivity: None,
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(json.contains("\"label\""), "label: {json}");
        assert!(
            json.contains("\"foregroundServiceType\""),
            "foregroundServiceType: {json}"
        );
    }

    #[test]
    fn start_keepalive_args_scheduling_intervals() {
        let args = StartKeepaliveArgs {
            label: "Test",
            foreground_service_type: "dataSync",
            ios_safety_timeout_secs: None,
            ios_processing_safety_timeout_secs: None,
            ios_earliest_refresh_begin_minutes: Some(30.0),
            ios_earliest_processing_begin_minutes: Some(60.0),
            ios_requires_external_power: None,
            ios_requires_network_connectivity: None,
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(
            json.contains("\"iosEarliestRefreshBeginMinutes\":30.0"),
            "JSON should contain iosEarliestRefreshBeginMinutes: {json}"
        );
        assert!(
            json.contains("\"iosEarliestProcessingBeginMinutes\":60.0"),
            "JSON should contain iosEarliestProcessingBeginMinutes: {json}"
        );
    }

    #[test]
    fn start_keepalive_args_processing_options() {
        let args = StartKeepaliveArgs {
            label: "Test",
            foreground_service_type: "dataSync",
            ios_safety_timeout_secs: None,
            ios_processing_safety_timeout_secs: None,
            ios_earliest_refresh_begin_minutes: None,
            ios_earliest_processing_begin_minutes: None,
            ios_requires_external_power: Some(true),
            ios_requires_network_connectivity: Some(true),
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(
            json.contains("\"iosRequiresExternalPower\":true"),
            "JSON should contain iosRequiresExternalPower: {json}"
        );
        assert!(
            json.contains("\"iosRequiresNetworkConnectivity\":true"),
            "JSON should contain iosRequiresNetworkConnectivity: {json}"
        );
    }

    // --- PluginConfig new scheduling fields tests ---

    #[test]
    fn plugin_config_earliest_refresh_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ios_earliest_refresh_begin_minutes, 15.0);
    }

    #[test]
    fn plugin_config_earliest_processing_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ios_earliest_processing_begin_minutes, 15.0);
    }

    #[test]
    fn plugin_config_requires_external_power_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(!config.ios_requires_external_power);
    }

    #[test]
    fn plugin_config_requires_network_connectivity_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(!config.ios_requires_network_connectivity);
    }

    #[test]
    fn plugin_config_custom_scheduling_intervals() {
        let json =
            r#"{"iosEarliestRefreshBeginMinutes":30.0,"iosEarliestProcessingBeginMinutes":60.0}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ios_earliest_refresh_begin_minutes, 30.0);
        assert_eq!(config.ios_earliest_processing_begin_minutes, 60.0);
    }

    #[test]
    fn plugin_config_custom_processing_options() {
        let json = r#"{"iosRequiresExternalPower":true,"iosRequiresNetworkConnectivity":true}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(config.ios_requires_external_power);
        assert!(config.ios_requires_network_connectivity);
    }

    // --- PluginConfig channel_capacity tests ---

    #[test]
    fn plugin_config_channel_capacity_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.channel_capacity, 16);
    }

    #[test]
    fn plugin_config_channel_capacity_custom() {
        let json = r#"{"channelCapacity":32}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.channel_capacity, 32);
    }

    #[test]
    fn plugin_config_channel_capacity_serde_roundtrip() {
        let config = PluginConfig {
            channel_capacity: 64,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.channel_capacity, 64);
    }

    #[test]
    fn plugin_config_channel_capacity_json_key_camel_case() {
        let config = PluginConfig {
            channel_capacity: 32,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("channelCapacity"),
            "JSON should use camelCase: {json}"
        );
    }

    // --- PluginConfig desktop fields tests (feature-gated) ---

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_mode_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.desktop_service_mode, "inProcess");
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_mode_custom() {
        let json = r#"{"desktopServiceMode":"osService"}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.desktop_service_mode, "osService");
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_mode_serde_roundtrip() {
        let config = PluginConfig {
            desktop_service_mode: "osService".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.desktop_service_mode, "osService");
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_label_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.desktop_service_label, None);
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_label_custom() {
        let json = r#"{"desktopServiceLabel":"my.svc"}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.desktop_service_label, Some("my.svc".to_string()));
    }

    use tauri::AppHandle;

    // --- ServiceContext mobile fields tests ---

    /// Compile-time + runtime test: ServiceContext mobile fields are String.
    #[cfg(mobile)]
    #[allow(dead_code)]
    fn service_context_mobile_fields_with_values<R: Runtime>(app: AppHandle<R>) {
        let ctx = ServiceContext {
            notifier: Notifier { app: app.clone() },
            app,
            shutdown: CancellationToken::new(),
            service_label: "Syncing".into(),
            foreground_service_type: "dataSync".into(),
        };
        assert_eq!(ctx.service_label, "Syncing");
        assert_eq!(ctx.foreground_service_type, "dataSync");
    }

    /// Compile-time + runtime test: ServiceContext on desktop has no mobile fields.
    #[cfg(not(mobile))]
    #[allow(dead_code)]
    fn service_context_desktop_no_mobile_fields<R: Runtime>(app: AppHandle<R>) {
        let ctx = ServiceContext {
            notifier: Notifier { app: app.clone() },
            app,
            shutdown: CancellationToken::new(),
        };
        // Compiles — service_label and foreground_service_type are absent.
        let _ = ctx;
    }

    // --- Foreground service type validation tests ---

    #[test]
    fn validate_data_sync_passes() {
        assert!(
            validate_foreground_service_type("dataSync").is_ok(),
            "dataSync should be valid"
        );
    }

    #[test]
    fn validate_special_use_passes() {
        assert!(
            validate_foreground_service_type("specialUse").is_ok(),
            "specialUse should be valid"
        );
    }

    #[test]
    fn validate_invalid_type_returns_platform_error() {
        let result = validate_foreground_service_type("invalidType");
        assert!(result.is_err(), "invalidType should be rejected");
        match result {
            Err(crate::error::ServiceError::Platform(msg)) => {
                assert!(
                    msg.contains("invalidType"),
                    "error should mention the type: {msg}"
                );
            }
            other => panic!("Expected Platform error, got: {other:?}"),
        }
    }

    #[test]
    fn validate_all_14_types_pass() {
        for &t in VALID_FOREGROUND_SERVICE_TYPES {
            assert!(
                validate_foreground_service_type(t).is_ok(),
                "{t} should be valid"
            );
        }
    }

    #[test]
    fn valid_types_count_is_14() {
        assert_eq!(
            VALID_FOREGROUND_SERVICE_TYPES.len(),
            14,
            "should have exactly 14 valid types"
        );
    }

    #[test]
    fn validate_empty_string_returns_error() {
        let result = validate_foreground_service_type("");
        assert!(result.is_err(), "empty string should be rejected");
    }

    #[test]
    fn validate_case_sensitive() {
        // "DataSync" (capitalized) should NOT pass — case-sensitive.
        let result = validate_foreground_service_type("DataSync");
        assert!(
            result.is_err(),
            "validation should be case-sensitive: DataSync should fail"
        );
    }

    // --- ServiceState serde tests ---

    #[test]
    fn service_state_idle_serde_roundtrip() {
        let state = ServiceState::Idle;
        let json = serde_json::to_string(&state).unwrap();
        let de: ServiceState = serde_json::from_str(&json).unwrap();
        assert_eq!(de, ServiceState::Idle);
    }

    #[test]
    fn service_state_initializing_serde_roundtrip() {
        let state = ServiceState::Initializing;
        let json = serde_json::to_string(&state).unwrap();
        let de: ServiceState = serde_json::from_str(&json).unwrap();
        assert_eq!(de, ServiceState::Initializing);
    }

    #[test]
    fn service_state_running_serde_roundtrip() {
        let state = ServiceState::Running;
        let json = serde_json::to_string(&state).unwrap();
        let de: ServiceState = serde_json::from_str(&json).unwrap();
        assert_eq!(de, ServiceState::Running);
    }

    #[test]
    fn service_state_stopped_serde_roundtrip() {
        let state = ServiceState::Stopped;
        let json = serde_json::to_string(&state).unwrap();
        let de: ServiceState = serde_json::from_str(&json).unwrap();
        assert_eq!(de, ServiceState::Stopped);
    }

    #[test]
    fn service_state_json_values_are_camel_case() {
        assert_eq!(
            serde_json::to_string(&ServiceState::Idle).unwrap(),
            "\"idle\""
        );
        assert_eq!(
            serde_json::to_string(&ServiceState::Initializing).unwrap(),
            "\"initializing\""
        );
        assert_eq!(
            serde_json::to_string(&ServiceState::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&ServiceState::Stopped).unwrap(),
            "\"stopped\""
        );
    }

    // --- ServiceStatus serde tests ---

    #[test]
    fn service_status_serde_roundtrip_idle() {
        let status = ServiceStatus {
            state: ServiceState::Idle,
            last_error: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        let de: ServiceStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(de.state, ServiceState::Idle);
        assert_eq!(de.last_error, None);
    }

    #[test]
    fn service_status_serde_roundtrip_with_error() {
        let status = ServiceStatus {
            state: ServiceState::Stopped,
            last_error: Some("init failed".into()),
        };
        let json = serde_json::to_string(&status).unwrap();
        let de: ServiceStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(de.state, ServiceState::Stopped);
        assert_eq!(de.last_error, Some("init failed".into()));
    }

    #[test]
    fn service_status_json_keys_camel_case() {
        let status = ServiceStatus {
            state: ServiceState::Running,
            last_error: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"state\":"), "state key: {json}");
        assert!(json.contains("\"lastError\":"), "lastError key: {json}");
    }

    #[test]
    fn service_status_json_null_last_error() {
        let status = ServiceStatus {
            state: ServiceState::Idle,
            last_error: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            json.contains("\"lastError\":null"),
            "lastError should be null: {json}"
        );
    }
}
