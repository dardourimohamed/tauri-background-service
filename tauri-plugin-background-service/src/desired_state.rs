//! Desired-state persistence for background service reliability.
//!
//! The [`DesiredState`] struct captures the user's intent for whether the background
//! service should be running, along with recovery metadata. Platform-specific backends
//! implement [`DesiredStateBackend`] to persist this state across process kills and
//! device reboots.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Persistent desired-state for the background service.
///
/// Captures the user's intent (`desired_running`) and recovery metadata so that
/// platform-specific backends can restore service state after process death or reboot.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DesiredState {
    /// Whether the user wants the service running.
    pub desired_running: bool,
    /// Last `StartConfig` used to start the service (JSON-serialized).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_start_config: Option<serde_json::Value>,
    /// Epoch millis when the service was last started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_start_epoch_ms: Option<u64>,
    /// Epoch millis of the last heartbeat from the service task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_epoch_ms: Option<u64>,
    /// Last native platform state (e.g. "timeout", "expired").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_native_state: Option<String>,
    /// Last platform-specific error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_platform_error: Option<String>,
    /// How many restart attempts have been made.
    #[serde(default)]
    pub restart_attempt: u32,
    /// Whether a recovery is pending (e.g. after boot).
    #[serde(default)]
    pub recovery_pending: bool,
    /// Why recovery was initiated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_reason: Option<String>,
}

/// Backend for persisting desired-state across process restarts.
///
/// Each platform provides its own implementation:
/// - **Desktop**: [`FileDesiredStateBackend`] (JSON file).
/// - **Android**: `DurableState` in Kotlin (via `SharedPreferences`).
/// - **iOS**: `UserDefaults` in Swift.
pub trait DesiredStateBackend: Send + Sync {
    /// Load the persisted desired state.
    ///
    /// Returns the default state if no persisted data exists.
    fn load(&self) -> Result<DesiredState, String>;
    /// Save the desired state.
    fn save(&self, state: &DesiredState) -> Result<(), String>;
    /// Clear persisted state (delete storage).
    fn clear(&self) -> Result<(), String>;
}

const FILE_NAME: &str = "bg-desired-state.json";

/// File-based desired-state backend for desktop platforms.
///
/// Stores a JSON file at `{dir}/bg-desired-state.json`.
pub struct FileDesiredStateBackend {
    path: PathBuf,
}

impl FileDesiredStateBackend {
    /// Create a new backend that reads/writes to `dir/bg-desired-state.json`.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }
}

impl DesiredStateBackend for FileDesiredStateBackend {
    fn load(&self) -> Result<DesiredState, String> {
        match fs::read_to_string(&self.path) {
            Ok(data) => serde_json::from_str(&data).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DesiredState::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn save(&self, state: &DesiredState) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
        fs::write(&self.path, json).map_err(|e| e.to_string())
    }

    fn clear(&self) -> Result<(), String> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DesiredState struct tests ---

    #[test]
    fn desired_state_default_values() {
        let state = DesiredState::default();
        assert!(!state.desired_running);
        assert_eq!(state.last_start_config, None);
        assert_eq!(state.last_start_epoch_ms, None);
        assert_eq!(state.last_heartbeat_epoch_ms, None);
        assert_eq!(state.last_native_state, None);
        assert_eq!(state.last_platform_error, None);
        assert_eq!(state.restart_attempt, 0);
        assert!(!state.recovery_pending);
        assert_eq!(state.recovery_reason, None);
    }

    #[test]
    fn desired_state_serde_roundtrip() {
        let state = DesiredState {
            desired_running: true,
            last_start_config: Some(serde_json::json!({"serviceLabel":"test"})),
            last_start_epoch_ms: Some(1700000000000),
            last_heartbeat_epoch_ms: Some(1700000001000),
            last_native_state: Some("running".into()),
            last_platform_error: None,
            restart_attempt: 2,
            recovery_pending: true,
            recovery_reason: Some("boot".into()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let de: DesiredState = serde_json::from_str(&json).unwrap();
        assert_eq!(de, state);
    }

    #[test]
    fn desired_state_json_keys_camel_case() {
        let state = DesiredState {
            desired_running: true,
            last_start_config: Some(serde_json::json!({"serviceLabel":"test"})),
            last_start_epoch_ms: Some(1700000000000),
            last_heartbeat_epoch_ms: Some(1700000001000),
            last_native_state: Some("running".into()),
            last_platform_error: Some("err".into()),
            restart_attempt: 1,
            recovery_pending: true,
            recovery_reason: Some("boot".into()),
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"desiredRunning\":"), "{json}");
        assert!(json.contains("\"lastStartConfig\":"), "{json}");
        assert!(json.contains("\"lastStartEpochMs\":"), "{json}");
        assert!(json.contains("\"lastHeartbeatEpochMs\":"), "{json}");
        assert!(json.contains("\"lastNativeState\":"), "{json}");
        assert!(json.contains("\"lastPlatformError\":"), "{json}");
        assert!(json.contains("\"restartAttempt\":"), "{json}");
        assert!(json.contains("\"recoveryPending\":"), "{json}");
        assert!(json.contains("\"recoveryReason\":"), "{json}");
    }

    #[test]
    fn desired_state_default_serde_roundtrip() {
        let state = DesiredState::default();
        let json = serde_json::to_string(&state).unwrap();
        let de: DesiredState = serde_json::from_str(&json).unwrap();
        assert_eq!(de, state);
    }

    // --- FileDesiredStateBackend tests ---

    fn temp_dir() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    #[test]
    fn file_backend_roundtrip() {
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());

        let state = DesiredState {
            desired_running: true,
            last_start_config: Some(
                serde_json::json!({"serviceLabel":"Syncing","foregroundServiceType":"dataSync"}),
            ),
            last_start_epoch_ms: Some(1700000000000),
            last_heartbeat_epoch_ms: Some(1700000005000),
            last_native_state: Some("running".into()),
            last_platform_error: None,
            restart_attempt: 0,
            recovery_pending: false,
            recovery_reason: None,
        };

        backend.save(&state).unwrap();
        let loaded = backend.load().unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn file_backend_load_missing_file_returns_default() {
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());

        // No file written — should return default.
        let loaded = backend.load().unwrap();
        assert_eq!(loaded, DesiredState::default());
    }

    #[test]
    fn file_backend_clear_loads_default() {
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());

        let state = DesiredState {
            desired_running: true,
            ..Default::default()
        };
        backend.save(&state).unwrap();

        backend.clear().unwrap();
        let loaded = backend.load().unwrap();
        assert_eq!(loaded, DesiredState::default());
    }

    #[test]
    fn file_backend_clear_removes_file() {
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());

        let state = DesiredState {
            desired_running: true,
            ..Default::default()
        };
        backend.save(&state).unwrap();
        assert!(dir.join(FILE_NAME).exists());

        backend.clear().unwrap();
        assert!(!dir.join(FILE_NAME).exists());
    }

    #[test]
    fn file_backend_clear_when_missing_is_ok() {
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());

        // Clear without ever saving — should succeed.
        backend.clear().unwrap();
    }

    #[test]
    fn file_backend_save_creates_parent_dir() {
        let dir = temp_dir();
        let nested = dir.join("sub").join("dir");
        let backend = FileDesiredStateBackend::new(nested);

        let state = DesiredState::default();
        backend.save(&state).unwrap();
        let loaded = backend.load().unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn file_backend_overwrite_on_save() {
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());

        let state1 = DesiredState {
            desired_running: true,
            ..Default::default()
        };
        backend.save(&state1).unwrap();

        let state2 = DesiredState {
            desired_running: false,
            restart_attempt: 5,
            ..Default::default()
        };
        backend.save(&state2).unwrap();

        let loaded = backend.load().unwrap();
        assert_eq!(loaded, state2);
        assert_ne!(loaded, state1);
    }

    // --- Trait object safety test ---

    #[test]
    fn backend_is_object_safe() {
        let dir = temp_dir();
        let backend: Box<dyn DesiredStateBackend> = Box::new(FileDesiredStateBackend::new(dir));
        let state = DesiredState::default();
        backend.save(&state).unwrap();
        let loaded = backend.load().unwrap();
        assert_eq!(loaded, state);
    }
}
