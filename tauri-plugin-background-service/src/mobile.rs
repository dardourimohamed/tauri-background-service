//! Mobile lifecycle bridge — only compiled on Android and iOS targets.
//!
//! Provides [`MobileLifecycle`] which wraps native keepalive calls via
//! `run_mobile_plugin`:
//!
//! - **Android** — Foreground service with persistent notification.
//! - **iOS** — `BGTaskScheduler` with expiration handler.
//!
//! This module is gated behind `#[cfg(mobile)]` in [`crate::lib`].

use serde::Serialize;
use tauri::{
    plugin::{PluginApi, PluginHandle},
    AppHandle, Runtime,
};

use crate::error::ServiceError;
use crate::manager::MobileKeepalive;
use crate::models::{
    AutoStartConfig, IOSSchedulingStatus, PendingTaskInfo, StartConfig, StartKeepaliveArgs,
};

/// Rust-side bridge to native mobile keepalive code.
///
/// Only compiled on mobile targets (`#[cfg(mobile)]` in lib.rs).
/// Calls through to Kotlin (Android) and Swift (iOS) via `run_mobile_plugin`.
pub struct MobileLifecycle<R: Runtime> {
    pub handle: PluginHandle<R>,
}

impl<R: Runtime> MobileLifecycle<R> {
    /// Start the OS-specific keepalive mechanism.
    ///
    /// - Android: starts a Foreground Service with `label` as notification text.
    /// - iOS: schedules a `BGAppRefreshTask` (and optionally a `BGProcessingTask`).
    ///
    /// `ios_processing_safety_timeout_secs` caps the processing task duration on iOS.
    /// When `None`, the processing task has no safety cap.
    ///
    /// On iOS, returns `Ok(Some(IOSSchedulingStatus))` with the scheduling result.
    /// On Android, returns `Ok(None)` (no structured result).
    /// When both iOS scheduling attempts fail, Swift rejects the invoke with
    /// `"schedulerUnavailable"`, which maps to `Err(ServiceError::Platform)`.
    #[allow(clippy::too_many_arguments)]
    pub fn start_keepalive(
        &self,
        label: &str,
        foreground_service_type: &str,
        ios_safety_timeout_secs: Option<f64>,
        ios_processing_safety_timeout_secs: Option<f64>,
        ios_earliest_refresh_begin_minutes: Option<f64>,
        ios_earliest_processing_begin_minutes: Option<f64>,
        ios_requires_external_power: Option<bool>,
        ios_requires_network_connectivity: Option<bool>,
    ) -> Result<Option<IOSSchedulingStatus>, ServiceError> {
        let result: serde_json::Value = self
            .handle
            .run_mobile_plugin(
                "startKeepalive",
                StartKeepaliveArgs {
                    label,
                    foreground_service_type,
                    ios_safety_timeout_secs,
                    ios_processing_safety_timeout_secs,
                    ios_earliest_refresh_begin_minutes,
                    ios_earliest_processing_begin_minutes,
                    ios_requires_external_power,
                    ios_requires_network_connectivity,
                },
            )
            .map_err(|e| ServiceError::Platform(e.to_string()))?;

        // On iOS, the result is a structured scheduling status dict.
        // On Android, the result is null (Value::Null).
        if let Ok(status) = serde_json::from_value::<IOSSchedulingStatus>(result) {
            if status.refresh_error.is_some() {
                log::warn!(
                    "iOS BGAppRefreshTask scheduling error: {:?}",
                    status.refresh_error
                );
            }
            if status.processing_error.is_some() {
                log::warn!(
                    "iOS BGProcessingTask scheduling error: {:?}",
                    status.processing_error
                );
            }
            Ok(Some(status))
        } else {
            Ok(None)
        }
    }

    /// Stop the OS-specific keepalive mechanism.
    ///
    /// - Android: stops the Foreground Service.
    /// - iOS: cancels the scheduled background task.
    pub fn stop_keepalive(&self) -> Result<(), ServiceError> {
        self.handle
            .run_mobile_plugin::<()>("stopKeepalive", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }

    /// Notify the native layer that the background service's `run()` completed.
    ///
    /// - iOS: calls `setTaskCompleted` on the stored BGTask and schedules the next one.
    pub fn complete_bg_task(&self, success: bool) -> Result<(), ServiceError> {
        self.handle
            .run_mobile_plugin::<()>("completeBgTask", CompleteBgTaskArgs { success })
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }

    /// Block until the native layer signals cancellation (e.g. iOS expiration handler).
    ///
    /// Uses the Pending Invoke pattern — the native side stores the Invoke without
    /// resolving it, which blocks this thread via `run_mobile_plugin`'s `rx.recv()`.
    /// When the expiration handler fires, it resolves the Invoke, unblocking this call.
    pub fn wait_for_cancel(&self) -> Result<(), ServiceError> {
        self.handle
            .run_mobile_plugin::<()>("waitForCancel", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }

    /// Reject the pending cancel invoke to unblock the `spawn_blocking` thread.
    ///
    /// Called from Rust when the cancel listener timeout fires (default: 4h).
    /// The Swift `cancelCancelListener` method rejects the stored invoke,
    /// which causes `wait_for_cancel` to return `Err` on the blocked thread.
    pub fn cancel_cancel_listener(&self) -> Result<(), ServiceError> {
        self.handle
            .run_mobile_plugin::<()>("cancelCancelListener", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }

    /// Check if the service was auto-started by OS restart.
    ///
    /// Reads auto-start config from SharedPreferences via the Kotlin bridge.
    /// Returns `Some(StartConfig)` if auto-start is pending and a label is available.
    pub fn get_auto_start_config(&self) -> Result<Option<StartConfig>, ServiceError> {
        let config: AutoStartConfig = self
            .handle
            .run_mobile_plugin("getAutoStartConfig", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(config.into_start_config())
    }

    /// Clear the auto-start flag after processing.
    ///
    /// Called from the plugin setup closure after auto-start has been handled.
    pub fn clear_auto_start_config(&self) -> Result<(), ServiceError> {
        self.handle
            .run_mobile_plugin::<()>("clearAutoStartConfig", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }

    /// Move the Activity to background after auto-start.
    ///
    /// Hides the briefly-visible Activity that was launched by the OS restart.
    pub fn move_task_to_background(&self) -> Result<(), ServiceError> {
        self.handle
            .run_mobile_plugin::<()>("moveTaskToBackground", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }

    /// Query the iOS scheduling status from the native layer.
    ///
    /// Calls `getSchedulingStatus` via `run_mobile_plugin` on the native side.
    /// Returns the structured scheduling result on iOS, or `Ok(None)` on Android.
    pub fn get_scheduling_status(&self) -> Result<Option<IOSSchedulingStatus>, ServiceError> {
        let result: serde_json::Value = self
            .handle
            .run_mobile_plugin("getSchedulingStatus", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;

        serde_json::from_value::<IOSSchedulingStatus>(result)
            .map(Some)
            .map_err(|e| ServiceError::Platform(e.to_string()))
    }

    /// Get the raw scheduling status as a JSON value.
    ///
    /// The native `getSchedulingStatus` returns more fields than
    /// `IOSSchedulingStatus` captures (e.g. `desiredRunning`, `lastStartConfig`).
    /// This method returns the full raw response for internal use.
    pub fn get_scheduling_status_raw(&self) -> Result<serde_json::Value, ServiceError> {
        self.handle
            .run_mobile_plugin("getSchedulingStatus", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))
    }

    /// Query the pending BGTask info from the native layer.
    ///
    /// Returns `Some(PendingTaskInfo)` if the app was launched by iOS for a
    /// background task, or `None` if no pending task exists.
    pub fn get_pending_bg_task(&self) -> Result<Option<PendingTaskInfo>, ServiceError> {
        let result: serde_json::Value = self
            .handle
            .run_mobile_plugin("getPendingBgTask", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;

        if result["taskKind"].is_null() {
            Ok(None)
        } else {
            serde_json::from_value::<PendingTaskInfo>(result)
                .map(Some)
                .map_err(|e| ServiceError::Platform(e.to_string()))
        }
    }

    /// Clear the pending BGTask info after Rust has processed the auto-start.
    pub fn clear_pending_bg_task(&self) -> Result<(), ServiceError> {
        self.handle
            .run_mobile_plugin::<()>("clearPendingBgTask", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }
}

/// Arguments sent to the native `completeBgTask` handler.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteBgTaskArgs {
    success: bool,
}

impl<R: Runtime> MobileKeepalive for MobileLifecycle<R> {
    #[allow(clippy::too_many_arguments)]
    fn start_keepalive(
        &self,
        label: &str,
        foreground_service_type: &str,
        ios_safety_timeout_secs: Option<f64>,
        ios_processing_safety_timeout_secs: Option<f64>,
        ios_earliest_refresh_begin_minutes: Option<f64>,
        ios_earliest_processing_begin_minutes: Option<f64>,
        ios_requires_external_power: Option<bool>,
        ios_requires_network_connectivity: Option<bool>,
    ) -> Result<(), ServiceError> {
        self.start_keepalive(
            label,
            foreground_service_type,
            ios_safety_timeout_secs,
            ios_processing_safety_timeout_secs,
            ios_earliest_refresh_begin_minutes,
            ios_earliest_processing_begin_minutes,
            ios_requires_external_power,
            ios_requires_network_connectivity,
        )
        .map(|_| ())
    }

    fn stop_keepalive(&self) -> Result<(), ServiceError> {
        self.stop_keepalive()
    }
}

/// Canonical Tauri v2 mobile init function.
///
/// Registers the plugin with the appropriate native layer:
/// - Android: `app.tauri.backgroundservice.BackgroundServicePlugin`
/// - iOS: uses the `init_plugin_background_service` binding macro
pub fn init<R: Runtime, C: serde::de::DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> Result<MobileLifecycle<R>, ServiceError> {
    #[cfg(target_os = "android")]
    let handle = api
        .register_android_plugin("app.tauri.backgroundservice", "BackgroundServicePlugin")
        .map_err(|e| ServiceError::Platform(e.to_string()))?;
    #[cfg(target_os = "ios")]
    let handle = api
        .register_ios_plugin(crate::init_plugin_background_service)
        .map_err(|e| ServiceError::Platform(e.to_string()))?;
    Ok(MobileLifecycle { handle })
}
