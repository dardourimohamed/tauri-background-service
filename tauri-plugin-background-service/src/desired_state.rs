//! Desired-state persistence for background service reliability.
//!
//! The [`DesiredState`] struct captures the user's intent for whether the background
//! service should be running, along with recovery metadata. Platform-specific backends
//! implement [`DesiredStateBackend`] to persist this state across process kills and
//! device reboots.

use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
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
const TEMP_SUFFIX: &str = ".tmp";

/// File-based desired-state backend for desktop platforms.
///
/// Stores a JSON file at `{dir}/bg-desired-state.json`. Writes are atomic:
/// [`save`](FileDesiredStateBackend::save) serializes to a sibling
/// `{canonical}.tmp` file, flushes+fsyncs it, then renames it over the
/// canonical path. A crash mid-write leaves either the previous canonical
/// file or a stale temp file, never a truncated canonical file.
pub struct FileDesiredStateBackend {
    path: PathBuf,
}

impl FileDesiredStateBackend {
    /// Construct a backend that reads/writes `{dir}/bg-desired-state.json`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let mut path = dir.into();
        path.push(FILE_NAME);
        Self { path }
    }

    /// Path to the sibling temp file used as the staging target for atomic
    /// saves. Exposed for tests.
    fn temp_path(&self) -> PathBuf {
        // Append a suffix (keeps the original extension so the file remains
        // recognizable and lives on the same filesystem as the canonical
        // path — required for atomic rename).
        let mut p = self.path.clone().into_os_string();
        p.push(TEMP_SUFFIX);
        PathBuf::from(p)
    }

    /// Best-effort cleanup of a stale temp file left by a previous crashed
    /// save. Failures are ignored: a stale temp does not affect correctness,
    /// only disk hygiene.
    fn clean_stale_temp(temp_path: &Path) {
        let _ = fs::remove_file(temp_path);
    }
}

impl DesiredStateBackend for FileDesiredStateBackend {
    fn load(&self) -> Result<DesiredState, String> {
        match fs::read_to_string(&self.path) {
            Ok(data) => serde_json::from_str(&data).map_err(|e| {
                // CORE-04: a malformed canonical file is surfaced as an
                // error rather than silently swallowed. The caller decides
                // recovery policy (the manager logs and falls back to
                // default). We do NOT auto-restore from the stale temp
                // here — a malformed canonical signals an external
                // corruption that should be observable.
                format!("malformed desired-state at {}: {e}", self.path.display())
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No canonical file. Clean up any stale temp from a prior
                // crashed save so it does not accumulate.
                Self::clean_stale_temp(&self.temp_path());
                Ok(DesiredState::default())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn save(&self, state: &DesiredState) -> Result<(), String> {
        // CORE-04: transactional write. Serialize once, write to a sibling
        // temp file, flush+fsync, then rename over the canonical path.
        // rename is atomic on Unix and (since Windows NT) is atomic on the
        // same filesystem via MoveFileEx with REPLACE_EXISTING. A crash at
        // any point before the rename leaves the canonical file untouched.
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        let json = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
        let temp_path = self.temp_path();

        // Write + flush + fsync the temp file before rename. fsync ensures
        // the file contents (not just the directory entry) survive a crash.
        {
            let mut file = File::create(&temp_path).map_err(|e| e.to_string())?;
            file.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
            file.flush().map_err(|e| e.to_string())?;
            // fsync_data is best-effort: on filesystems where it fails
            // (e.g. some network FS), the subsequent rename still provides
            // atomicity, only durability is weakened.
            if let Err(e) = file.sync_all() {
                log::warn!("fsync of desired-state temp failed: {e}");
            }
        }

        // Atomic replace. On Windows pre-2008 this would fail if the target
        // exists; modern Windows (Vista+) supports REPLACE_EXISTING.
        fs::rename(&temp_path, &self.path).map_err(|e| {
            // If rename failed, leave no stale temp behind.
            Self::clean_stale_temp(&temp_path);
            e.to_string()
        })
    }

    fn clear(&self) -> Result<(), String> {
        // Remove the canonical file (idempotent on NotFound) and any
        // stale temp.
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }?;
        Self::clean_stale_temp(&self.temp_path());
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
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

    // ── CORE-04: transactional save contract ──────────────────────────

    #[test]
    fn core04_save_leaves_no_temp_after_success() {
        // After a successful save, the sibling temp file must be gone —
        // it has been renamed over the canonical path.
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());
        backend
            .save(&DesiredState {
                desired_running: true,
                ..Default::default()
            })
            .unwrap();
        assert!(dir.join(FILE_NAME).exists(), "canonical must exist");
        assert!(
            !dir.join(FILE_NAME.to_owned() + TEMP_SUFFIX).exists(),
            "temp must be gone after successful save"
        );
    }

    #[test]
    fn core04_malformed_canonical_surfaces_error_not_default() {
        // A corrupted canonical file must be observable (Err), not silently
        // replaced with Default. This is the CORE-04 "never accept malformed
        // canonical JSON silently" guarantee.
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());
        // Pre-corrupt the canonical file at the exact path save would use.
        std::fs::write(dir.join(FILE_NAME), "{ this is not valid json").unwrap();
        let err = backend.load().unwrap_err();
        assert!(
            err.contains("malformed desired-state"),
            "expected malformed-canonical diagnostic, got: {err}"
        );
    }

    #[test]
    fn core04_stale_temp_does_not_break_subsequent_save() {
        // A stale temp left by a previous crashed save must not corrupt the
        // next save. The new save reuses the temp path via File::create
        // (which truncates) and renames over canonical.
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());
        // Plant a stale temp with garbage.
        std::fs::write(
            dir.join(FILE_NAME.to_owned() + TEMP_SUFFIX),
            "stale garbage from a crashed save",
        )
        .unwrap();

        let state = DesiredState {
            desired_running: true,
            restart_attempt: 7,
            ..Default::default()
        };
        backend.save(&state).unwrap();

        let loaded = backend.load().unwrap();
        assert_eq!(loaded, state);
        assert!(
            !dir.join(FILE_NAME.to_owned() + TEMP_SUFFIX).exists(),
            "stale temp must be cleared by successful save"
        );
    }

    #[test]
    fn core04_canonical_is_never_partial_across_overwrites() {
        // Repeated saves of varying sizes must always leave a canonical
        // file that parses to exactly what was saved. This is the
        // canonical-never-partial guarantee: each save is all-or-nothing.
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());

        for i in 0..20u32 {
            let state = DesiredState {
                desired_running: i % 2 == 0,
                restart_attempt: i,
                recovery_reason: Some(format!("iter-{i}")),
                last_platform_error: Some("x".repeat(i as usize)),
                ..Default::default()
            };
            backend.save(&state).unwrap();

            // The canonical file must always parse cleanly into exactly
            // the saved state — never a prefix of it.
            let raw = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
            let parsed: DesiredState = serde_json::from_str(&raw).unwrap();
            assert_eq!(parsed, state, "iter {i}: canonical must be complete");
            // And load() agrees.
            assert_eq!(backend.load().unwrap(), state, "iter {i}");
            // Each iteration must close cleanly with no temp left behind.
            assert!(
                !dir.join(FILE_NAME.to_owned() + TEMP_SUFFIX).exists(),
                "iter {i}: temp leaked"
            );
        }
    }

    #[test]
    fn core04_clear_removes_stale_temp_too() {
        // clear() must remove both canonical and any leftover temp so
        // disk hygiene is restored after a crash + manual recovery.
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());
        backend
            .save(&DesiredState {
                desired_running: true,
                ..Default::default()
            })
            .unwrap();
        // Plant a stale temp alongside the canonical file.
        std::fs::write(dir.join(FILE_NAME.to_owned() + TEMP_SUFFIX), "stale").unwrap();
        assert!(dir.join(FILE_NAME).exists());
        assert!(dir.join(FILE_NAME.to_owned() + TEMP_SUFFIX).exists());

        backend.clear().unwrap();
        assert!(!dir.join(FILE_NAME).exists());
        assert!(!dir.join(FILE_NAME.to_owned() + TEMP_SUFFIX).exists());
    }

    #[test]
    fn core04_load_missing_file_cleans_stale_temp() {
        // When load() finds no canonical file, it opportunistically
        // cleans a stale temp so a crashed save does not leave litter.
        let dir = temp_dir();
        let backend = FileDesiredStateBackend::new(dir.clone());
        std::fs::write(dir.join(FILE_NAME.to_owned() + TEMP_SUFFIX), "stale").unwrap();
        assert!(!dir.join(FILE_NAME).exists());

        let loaded = backend.load().unwrap();
        assert_eq!(loaded, DesiredState::default());
        assert!(
            !dir.join(FILE_NAME.to_owned() + TEMP_SUFFIX).exists(),
            "load of missing canonical must clean stale temp"
        );
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
