use serde::{Deserialize, Serialize};
use tauri::Runtime;
use tokio_util::sync::CancellationToken;

use crate::notifier::Notifier;

/// Passed into both `init` and `run`.
/// Gives your service everything it needs to interact with the outside world.
pub struct ServiceContext<R: Runtime> {
    /// Fire a local notification. Works on all platforms.
    pub notifier: Notifier<R>,

    /// Emit an event to the JS UI layer.
    pub app: tauri::AppHandle<R>,

    /// Cancelled when `stopService()` is called.
    pub shutdown: CancellationToken,
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

impl Default for StartConfig {
    fn default() -> Self {
        Self {
            service_label: default_label(),
            foreground_service_type: default_foreground_service_type(),
        }
    }
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

/// Auto-start config returned by the Kotlin bridge.
///
/// Deserialized from SharedPreferences values read by `getAutoStartConfig`.
/// Only used on Android (the iOS path doesn't have auto-start).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoStartConfig {
    pub pending: bool,
    pub label: Option<String>,
}

impl AutoStartConfig {
    /// Convert to `StartConfig` if auto-start is pending and label is available.
    pub fn into_start_config(self) -> Option<StartConfig> {
        if self.pending {
            self.label.map(|label| StartConfig {
                service_label: label,
                foreground_service_type: default_foreground_service_type(),
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
        assert!(json.contains("serviceLabel"), "JSON should use camelCase: {json}");
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
    fn start_config_unrecognized_type_passes_through() {
        let json = r#"{"serviceLabel":"test","foregroundServiceType":"customType"}"#;
        let de: StartConfig = serde_json::from_str(json).unwrap();
        assert_eq!(de.foreground_service_type, "customType");
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
        assert_eq!(result.unwrap().service_label, "Syncing");
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
}
