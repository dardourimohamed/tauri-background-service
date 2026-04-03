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

    /// Text shown in the Android persistent notification.
    /// `None` on desktop platforms.
    pub service_label: Option<String>,

    /// Android foreground service type (e.g. "dataSync", "specialUse").
    /// `None` on desktop platforms.
    pub foreground_service_type: Option<String>,
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
}

fn default_ios_safety_timeout() -> f64 {
    28.0
}

fn default_ios_cancel_listener_timeout_secs() -> u64 {
    14400
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

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            ios_safety_timeout_secs: default_ios_safety_timeout(),
            ios_cancel_listener_timeout_secs: default_ios_cancel_listener_timeout_secs(),
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
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.ios_safety_timeout_secs, 30.0);
    }

    #[test]
    fn plugin_config_default_impl() {
        let config = PluginConfig::default();
        assert_eq!(config.ios_safety_timeout_secs, 28.0);
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
            ios_safety_timeout_secs: 28.0,
            ios_cancel_listener_timeout_secs: 3600,
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.ios_cancel_listener_timeout_secs, 3600);
    }

    // --- StartKeepaliveArgs tests ---

    #[test]
    fn start_keepalive_args_with_timeout() {
        let args = StartKeepaliveArgs {
            label: "Test",
            foreground_service_type: "dataSync",
            ios_safety_timeout_secs: Some(15.0),
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
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(
            !json.contains("iosSafetyTimeoutSecs"),
            "JSON should NOT contain iosSafetyTimeoutSecs when None: {json}"
        );
    }

    #[test]
    fn start_keepalive_args_camel_case_keys() {
        let args = StartKeepaliveArgs {
            label: "Test",
            foreground_service_type: "specialUse",
            ios_safety_timeout_secs: None,
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(json.contains("\"label\""), "label: {json}");
        assert!(
            json.contains("\"foregroundServiceType\""),
            "foregroundServiceType: {json}"
        );
    }

    use tauri::AppHandle;

    // --- ServiceContext new fields tests ---

    /// Compile-time + runtime test: ServiceContext accepts the new optional fields.
    #[allow(dead_code)]
    fn service_context_new_fields_default_to_none<R: Runtime>(app: AppHandle<R>) {
        let ctx = ServiceContext {
            notifier: Notifier { app: app.clone() },
            app,
            shutdown: CancellationToken::new(),
            service_label: None,
            foreground_service_type: None,
        };
        assert_eq!(ctx.service_label, None);
        assert_eq!(ctx.foreground_service_type, None);
    }

    /// Compile-time + runtime test: ServiceContext carries label and type values.
    #[allow(dead_code)]
    fn service_context_new_fields_with_values<R: Runtime>(app: AppHandle<R>) {
        let ctx = ServiceContext {
            notifier: Notifier { app: app.clone() },
            app,
            shutdown: CancellationToken::new(),
            service_label: Some("Syncing".into()),
            foreground_service_type: Some("dataSync".into()),
        };
        assert_eq!(ctx.service_label.as_deref(), Some("Syncing"));
        assert_eq!(ctx.foreground_service_type.as_deref(), Some("dataSync"));
    }
}
