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
use crate::manager::{MobileKeepalive, NativeAuthority};
use crate::models::{
    AndroidServiceState, IOSDesiredStateStatus, IOSSchedulingStatus, IosNativeState,
    NotificationPermissionStatus, PendingTaskInfo, StartKeepaliveArgs,
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
        ios_processing_ceiling_multiplier: Option<f64>,
    ) -> Result<Option<IOSSchedulingStatus>, ServiceError> {
        log::info!(
            "MobileLifecycle::start_keepalive: label={}, fgs_type={}",
            label,
            foreground_service_type
        );
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
                    ios_processing_ceiling_multiplier,
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

    /// Request the Android battery-optimization (Doze) exemption (BGS-22, doc-08
    /// Step 14).
    ///
    /// Calls the Kotlin `requestBatteryExemption` @Command via
    /// `run_mobile_plugin`, which fires
    /// `startActivity(ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS)` for this
    /// app's package so the user can grant the Doze exemption. iOS is gated out
    /// at the command layer (no Swift handler exists; there is no Doze
    /// analogue).
    pub fn request_battery_exemption(&self) -> Result<(), ServiceError> {
        self.handle
            .run_mobile_plugin::<()>("requestBatteryExemption", ())
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

    /// Block until iOS delivers a BGTask to the **warm** process (H14).
    ///
    /// Mirrors [`wait_for_cancel`](Self::wait_for_cancel)'s Pending Invoke
    /// pattern: the Swift `waitForBgTask` handler stores the Invoke without
    /// resolving it, blocking this thread via `run_mobile_plugin`'s `rx.recv()`.
    /// When `handleBackgroundTask`/`handleProcessingTask` persists a new pending
    /// task, it resolves the Invoke, unblocking this call so Rust can warm-start.
    ///
    /// iOS-only: the `waitForBgTask` handler is iOS-specific (Android owns its
    /// own Kotlin lifecycle), so this is a no-op `Ok(())` on Android.
    pub fn wait_for_bg_task(&self) -> Result<(), ServiceError> {
        #[cfg(target_os = "ios")]
        {
            self.handle
                .run_mobile_plugin::<()>("waitForBgTask", ())
                .map_err(|e| ServiceError::Platform(e.to_string()))?;
        }
        Ok(())
    }

    /// Reject the pending warm-listener invoke to unblock the `spawn_blocking`
    /// thread on teardown.
    ///
    /// Mirrors [`cancel_cancel_listener`](Self::cancel_cancel_listener): the
    /// Swift `cancelWarmListener` method rejects the stored invoke, causing
    /// `wait_for_bg_task` to return `Err` on the blocked thread so it does not
    /// leak when the warm listener loop is shut down.
    pub fn cancel_warm_listener(&self) -> Result<(), ServiceError> {
        #[cfg(target_os = "ios")]
        {
            self.handle
                .run_mobile_plugin::<()>("cancelWarmListener", ())
                .map_err(|e| ServiceError::Platform(e.to_string()))?;
        }
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

    /// Query the iOS scheduling *submit-result* status from the native layer.
    ///
    /// Calls `getSchedulingStatus` via `run_mobile_plugin` on the native side,
    /// which resolves the submit-result shape (`{refreshScheduled,
    /// processingScheduled, refreshError, processingError}`) of the most recent
    /// scheduling attempt. Returns the typed result on iOS, or `Ok(None)` on
    /// Android (where the call resolves `null`).
    pub fn get_scheduling_status(&self) -> Result<Option<IOSSchedulingStatus>, ServiceError> {
        let result: serde_json::Value = self
            .handle
            .run_mobile_plugin("getSchedulingStatus", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;

        if result.is_null() {
            return Ok(None);
        }
        serde_json::from_value::<IOSSchedulingStatus>(result)
            .map(Some)
            .map_err(|e| ServiceError::Platform(e.to_string()))
    }

    /// Query the Android POST_NOTIFICATIONS permission status (NTF-09).
    ///
    /// Calls `getNotificationPermissionStatus` via `run_mobile_plugin`, which
    /// resolves immediately with `{status: granted|notDetermined|denied}`. This
    /// is the NON-BLOCKING getter — the cfg axis (Android-active) lives on the
    /// `#[tauri::command]` wrapper in `lib.rs`, mirroring `get_scheduling_status`.
    pub fn get_notification_permission_status(
        &self,
    ) -> Result<NotificationPermissionStatus, ServiceError> {
        let result: serde_json::Value = self
            .handle
            .run_mobile_plugin("getNotificationPermissionStatus", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        serde_json::from_value::<NotificationPermissionStatus>(result)
            .map_err(|e| ServiceError::Platform(e.to_string()))
    }

    /// Whether the app may post a full-screen intent (NTF-16, Step 12c).
    ///
    /// Calls `canUseFullScreenIntent` via `run_mobile_plugin`, which resolves
    /// immediately with `{canUse: bool}`. This is an immediate-resolve getter —
    /// like `get_notification_permission_status`, NO `spawn_blocking` is needed.
    /// The bool field is extracted directly (no typed struct required).
    pub fn can_use_full_screen_intent(&self) -> Result<bool, ServiceError> {
        let result: serde_json::Value = self
            .handle
            .run_mobile_plugin("canUseFullScreenIntent", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        result["canUse"]
            .as_bool()
            .ok_or_else(|| ServiceError::Platform("missing canUse".into()))
    }

    /// Open the OS settings page to re-grant USE_FULL_SCREEN_INTENT (NTF-16).
    ///
    /// Calls `openFullScreenIntentSettings` via `run_mobile_plugin`, which
    /// resolves immediately (startActivity) with no payload. Immediate-resolve —
    /// NO `spawn_blocking`. The null result is discarded.
    pub fn open_full_screen_intent_settings(&self) -> Result<(), ServiceError> {
        let _result: serde_json::Value = self
            .handle
            .run_mobile_plugin("openFullScreenIntentSettings", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }

    /// Request the Android POST_NOTIFICATIONS permission (NTF-09).
    ///
    /// Calls `requestNotificationPermission` via `run_mobile_plugin`. On API 33+
    /// the Kotlin command defers resolution to the `@PermissionCallback`
    /// (Step 10a), so `run_mobile_plugin`'s `rx.recv()` BLOCKS for the OS
    /// permission-dialog duration — the `wait_for_cancel` class. The
    /// `#[tauri::command]` wrapper MUST therefore wrap this call in
    /// `tokio::task::spawn_blocking`. Resolves `{status: granted|denied}`.
    pub fn request_notification_permission(
        &self,
    ) -> Result<NotificationPermissionStatus, ServiceError> {
        let result: serde_json::Value = self
            .handle
            .run_mobile_plugin("requestNotificationPermission", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        serde_json::from_value::<NotificationPermissionStatus>(result)
            .map_err(|e| ServiceError::Platform(e.to_string()))
    }

    /// Query the persisted iOS *desired-state* status from the native layer.
    ///
    /// Calls `getDesiredStateStatus` via `run_mobile_plugin`, which resolves the
    /// persisted shape (`{desiredRunning, lastStartConfig, lastScheduleError,
    /// lastTaskKind, lastTaskStartedAt, lastTaskCompletedAt,
    /// lastCompletionReason, notificationGranted}`). The iOS auto-start path reads
    /// `desired_running` + `last_start_config` from this typed DTO;
    /// `last_completion_reason` is the durable "why did the last run end?" fact (M7),
    /// sourced by `get_ios_native_state`; `notification_granted` forwards the deferred
    /// notification-authorization decision (M4) so the Notifier can degrade. Returns
    /// `Ok(None)` on Android (the call resolves `null`).
    pub fn get_desired_state_status(&self) -> Result<Option<IOSDesiredStateStatus>, ServiceError> {
        let result: serde_json::Value = self
            .handle
            .run_mobile_plugin("getDesiredStateStatus", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;

        if result.is_null() {
            return Ok(None);
        }
        serde_json::from_value::<IOSDesiredStateStatus>(result)
            .map(Some)
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

        // H5/M14: gate on `consumed_at`, not only `taskKind.is_null()`, so a
        // consumed/stale record can't re-arm a cold auto-start.
        PendingTaskInfo::from_pending_payload(&result)
            .map_err(|e| ServiceError::Platform(e.to_string()))
    }

    /// Query the Android native service state from the Kotlin bridge.
    ///
    /// Calls `getAndroidServiceState` via `run_mobile_plugin`. Returns the
    /// full native service state on Android. On iOS or when the command
    /// returns null, returns `Ok(None)`.
    ///
    /// L4: gated behind `cfg(target_os = "android")` so iOS never pays a bridge
    /// round-trip for a handler it does not implement. iOS native state is
    /// queried via [`Self::get_ios_native_state`] instead.
    pub fn get_android_service_state(&self) -> Result<Option<AndroidServiceState>, ServiceError> {
        #[cfg(not(target_os = "android"))]
        {
            Ok(None)
        }
        #[cfg(target_os = "android")]
        {
            let result: serde_json::Value = self
                .handle
                .run_mobile_plugin("getAndroidServiceState", ())
                .map_err(|e| ServiceError::Platform(e.to_string()))?;

            if result.is_null() {
                Ok(None)
            } else {
                serde_json::from_value::<AndroidServiceState>(result)
                    .map(Some)
                    .map_err(|e| ServiceError::Platform(e.to_string()))
            }
        }
    }

    /// Assemble the iOS native background-task snapshot (H6) from the typed
    /// status queries (`getDesiredStateStatus` + `getSchedulingStatus` +
    /// `getPendingBgTask`).
    ///
    /// iOS-only: returns `Ok(None)` on Android (which owns its own Kotlin
    /// foreground-service authority). The "active task" is inferred from the
    /// persisted start/complete timestamps; `in_budget` is `false` only when a
    /// scheduling attempt was made and both task types failed to schedule.
    pub fn get_ios_native_state(&self) -> Result<Option<IosNativeState>, ServiceError> {
        #[cfg(target_os = "ios")]
        {
            let Some(desired) = self.get_desired_state_status()? else {
                return Ok(None);
            };
            let sched = self.get_scheduling_status()?;
            let pending = self.get_pending_bg_task()?;

            // A BGTask is "active" when it started more recently than it last
            // completed (or has started but never completed).
            let active_task_kind =
                match (desired.last_task_started_at, desired.last_task_completed_at) {
                    (Some(started), Some(completed)) if started > completed => {
                        desired.last_task_kind.clone()
                    }
                    (Some(_), None) => desired.last_task_kind.clone(),
                    _ => None,
                };

            // "scheduled?" + split "last-failed?" (M7) come straight from the
            // submit-result snapshot — kept distinct per task type rather than
            // collapsed into one aggregate error.
            let refresh_scheduled = sched.as_ref().is_some_and(|s| s.refresh_scheduled);
            let processing_scheduled = sched.as_ref().is_some_and(|s| s.processing_scheduled);
            let last_refresh_error = sched.as_ref().and_then(|s| s.refresh_error.clone());
            let last_processing_error = sched.as_ref().and_then(|s| s.processing_error.clone());

            // Out of budget only when a scheduling attempt was made and neither
            // task scheduled successfully; otherwise assume budget remains.
            let in_budget = match &sched {
                Some(s) => {
                    s.refresh_scheduled
                        || s.processing_scheduled
                        || (s.refresh_error.is_none() && s.processing_error.is_none())
                }
                None => true,
            };

            Ok(Some(IosNativeState {
                desired_running: desired.desired_running,
                refresh_scheduled,
                processing_scheduled,
                active_task_kind,
                pending_task: pending,
                last_completed_at: desired.last_task_completed_at,
                // "why?" — the durable last-completion reason persisted by the
                // native layer (survives `scheduleNext`'s outcome consume).
                last_completion_reason: desired.last_completion_reason.clone(),
                last_refresh_error,
                last_processing_error,
                in_budget,
            }))
        }
        #[cfg(not(target_os = "ios"))]
        {
            Ok(None)
        }
    }

    /// Clear the pending BGTask info after Rust has processed the auto-start.
    pub fn clear_pending_bg_task(&self) -> Result<(), ServiceError> {
        self.handle
            .run_mobile_plugin::<()>("clearPendingBgTask", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }

    /// Record a failure marker for the pending BGTask when a cold auto-start
    /// fails (H3).
    ///
    /// On failure the pending record is deliberately **not** cleared so the
    /// evidence survives; this stamps `lastFailedPendingAt` in `UserDefaults` so
    /// the failure is observable for diagnostics without consuming the task.
    ///
    /// iOS-only: the `recordFailedPending` handler is iOS-specific, so this is a
    /// no-op on Android (which owns its own Kotlin lifecycle).
    pub fn record_failed_pending(&self) -> Result<(), ServiceError> {
        #[cfg(target_os = "ios")]
        {
            self.handle
                .run_mobile_plugin::<()>("recordFailedPending", ())
                .map_err(|e| ServiceError::Platform(e.to_string()))?;
        }
        Ok(())
    }

    /// Swap the foreground service type of the running service (spec 08 C6,
    /// Step 15) — Android: sends `ACTION_UPDATE_TYPE` to the running
    /// LifecycleService without restarting the headless core.
    ///
    /// Android-only (M5): iOS has no `updateForegroundServiceType` native
    /// handler, so the body is a no-op there (the caller also gates this behind
    /// `enforces_foreground_service_type`, so iOS never reaches it).
    pub fn update_keepalive_type(&self, foreground_service_type: &str) -> Result<(), ServiceError> {
        log::info!(
            "MobileLifecycle::update_keepalive_type: fgs_type={}",
            foreground_service_type
        );
        #[cfg(target_os = "android")]
        {
            self.handle
                .run_mobile_plugin::<()>(
                    "updateForegroundServiceType",
                    UpdateForegroundServiceTypeArgs {
                        foreground_service_type: foreground_service_type.to_string(),
                    },
                )
                .map_err(|e| ServiceError::Platform(e.to_string()))?;
        }
        Ok(())
    }

    /// Fire the native incoming-call notification (spec 08 C6, Step 15).
    pub fn show_incoming_call(
        &self,
        call_id: &str,
        caller_name: &str,
        is_video: bool,
    ) -> Result<(), ServiceError> {
        log::info!(
            "MobileLifecycle::show_incoming_call: call_id={}, video={}",
            call_id,
            is_video
        );
        self.handle
            .run_mobile_plugin::<()>(
                "showIncomingCall",
                ShowIncomingCallArgs {
                    call_id: call_id.to_string(),
                    caller_name: caller_name.to_string(),
                    is_video,
                },
            )
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }

    /// Fire an actionable native message notification.
    ///
    /// **doc-06 NTF-07 iOS actionable message-surface DEFERRED to Step-13
    /// iOS runbook.** The active arm below is `#[cfg(target_os = "android")]`:
    /// Android dispatches via the Kotlin `showMessageNotification` `@Command`
    /// (`BackgroundServicePlugin.kt`:415 → `ActionableMessageNotifier.kt`:35),
    /// which posts a `MessagingStyle` notification with reply / mark-read
    /// actions. The `#[cfg(not(target_os = "android"))]` arm is a let-underscore
    /// no-op — that is CORRECT for desktop (desktop routes actionable
    /// notifications through `tauri/src/event_bridge.rs` `emit_system_notification`
    /// + notify-rust, NOT through this `MobileLifecycle` path) but it also
    /// leaves awake iOS without a native actionable message surface.
    ///
    /// The deferral is INTENTIONAL and documented, not an oversight. There is
    /// NO `@objc showMessageNotification` handler among the 17
    /// `BackgroundServicePlugin.swift` `@objc` command handlers
    /// (`waitForCancel` … `recordFailedPending`, lines 717-1167), and iOS Swift
    /// has NO `UNUserNotificationCenter` post infrastructure anywhere in
    /// `ios/Sources/` (only `requestAuthorization` at `Seams.swift`:72-93). The
    /// incoming-call-wake half is owned by doc-08. Widening the arm to
    /// `cfg(any(target_os = "android", target_os = "ios"))` is FORBIDDEN until a
    /// Swift handler exists — it would wire `run_mobile_plugin` to a command
    /// Tauri rejects at runtime. The future-correct iOS arm mirrors the android
    /// one under `#[cfg(target_os = "ios")]` active + `#[cfg(not(target_os =
    /// "ios"))]` no-op (the pattern already used by `mirror_desired_state`
    /// below), but ONLY after the Swift `showMessageNotification` handler +
    /// `UNNotificationRequest` post code are authored in the Step-13 iOS runbook.
    #[allow(clippy::too_many_arguments)]
    pub fn show_message_notification(
        &self,
        notification_id: i32,
        chat_id: &str,
        message_id: &str,
        title: &str,
        body: &str,
        route_uri: &str,
    ) -> Result<(), ServiceError> {
        log::info!(
            "MobileLifecycle::show_message_notification: notification_id={}, chat_id={}, message_id={}",
            notification_id,
            chat_id,
            message_id
        );
        #[cfg(target_os = "android")]
        {
            self.handle
                .run_mobile_plugin::<()>(
                    "showMessageNotification",
                    ShowMessageNotificationArgs {
                        notification_id,
                        chat_id: chat_id.to_string(),
                        message_id: message_id.to_string(),
                        title: title.to_string(),
                        body: body.to_string(),
                        route_uri: route_uri.to_string(),
                    },
                )
                .map_err(|e| ServiceError::Platform(e.to_string()))?;
        }
        #[cfg(not(target_os = "android"))]
        {
            // doc-06 NTF-07 iOS actionable message-surface DEFERRED to
            // Step-13 iOS runbook: this arm conflates desktop (correctly a
            // no-op — desktop routes actionable notifications via
            // emit_system_notification) with iOS (which has no Swift
            // showMessageNotification handler + no UNUserNotificationCenter
            // post code). Do NOT widen the `#[cfg(target_os = "android")]`
            // active arm above to `cfg(any(.., target_os = "ios"))`.
            let _ = (notification_id, chat_id, message_id, title, body, route_uri);
        }
        Ok(())
    }

    /// Cancel the native incoming-call notification (spec 08 C6, Step 15).
    pub fn cancel_incoming_call(&self, call_id: &str) -> Result<(), ServiceError> {
        log::info!("MobileLifecycle::cancel_incoming_call: call_id={}", call_id);
        self.handle
            .run_mobile_plugin::<()>(
                "cancelIncomingCall",
                CancelIncomingCallArgs {
                    call_id: call_id.to_string(),
                },
            )
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }

    /// Set the active call's device audio route (M-NATIVE-3 / CCF-11, Step 11):
    /// Android applies it to the live self-managed `BackgroundCallConnection` via
    /// `Connection.setAudioRoute`; iOS via `AVAudioSession.overrideOutputAudioPort`.
    pub fn set_call_audio_route(&self, call_id: &str, route: &str) -> Result<(), ServiceError> {
        log::info!(
            "MobileLifecycle::set_call_audio_route: call_id={}, route={}",
            call_id,
            route
        );
        self.handle
            .run_mobile_plugin::<()>(
                "setCallAudioRoute",
                SetCallAudioRouteArgs {
                    call_id: call_id.to_string(),
                    route: route.to_string(),
                },
            )
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }

    /// Open the OS app-settings screen (M-DIAG-2 / CCF-12, Step 17): Android
    /// opens the app-details / permission settings via an
    /// `ACTION_APPLICATION_DETAILS_SETTINGS` intent; iOS opens
    /// `UIApplication.openSettingsURLString`.
    pub fn open_app_settings(&self) -> Result<(), ServiceError> {
        log::info!("MobileLifecycle::open_app_settings");
        self.handle
            .run_mobile_plugin::<()>("openAppSettings", ())
            .map_err(|e| ServiceError::Platform(e.to_string()))?;
        Ok(())
    }
    /// Mirror the Rust-authoritative desired state into iOS native persistence
    /// (H4 / D1).
    ///
    /// Calls the Swift `setDesiredRunning` handler, which writes
    /// `desiredRunning` (+ optional `lastStartConfig` as a JSON string) into
    /// `UserDefaults` and (re)schedules or cancels the BGTasks accordingly. This
    /// is how the intent-only recovery commands (`enableAutoRestart` /
    /// `disableAutoRestart` / `setDesiredRunning` / `configureRecovery`) take
    /// real effect on iOS instead of silently no-op'ing.
    ///
    /// iOS-only: Android keeps its Kotlin `DurableState` authoritative, so this
    /// is a no-op there (the `setDesiredRunning` handler is iOS-specific).
    pub fn mirror_desired_state(
        &self,
        desired_running: bool,
        last_start_config: Option<&serde_json::Value>,
    ) -> Result<(), ServiceError> {
        #[cfg(target_os = "ios")]
        {
            let last_start_config = last_start_config.map(|v| v.to_string());
            self.handle
                .run_mobile_plugin::<()>(
                    "setDesiredRunning",
                    SetDesiredRunningArgs {
                        desired_running,
                        last_start_config,
                    },
                )
                .map_err(|e| ServiceError::Platform(e.to_string()))?;
        }
        #[cfg(not(target_os = "ios"))]
        {
            let _ = (desired_running, last_start_config);
        }
        Ok(())
    }
}

/// Arguments for the native iOS `setDesiredRunning` handler (H4 desired-state
/// mirror). `last_start_config` is the JSON-serialized `StartConfig` string so
/// the iOS auto-start can `from_str::<StartConfig>` it back.
#[cfg(target_os = "ios")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetDesiredRunningArgs {
    desired_running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_start_config: Option<String>,
}

/// Arguments for the native `updateForegroundServiceType` handler (spec 08 C6).
/// Android-only (M5): iOS has no such handler.
#[cfg(target_os = "android")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateForegroundServiceTypeArgs {
    foreground_service_type: String,
}

/// Arguments for the native `showIncomingCall` handler (spec 08 C6).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ShowIncomingCallArgs {
    call_id: String,
    caller_name: String,
    is_video: bool,
}

/// Arguments for the native `showMessageNotification` handler.
#[cfg(target_os = "android")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ShowMessageNotificationArgs {
    notification_id: i32,
    chat_id: String,
    message_id: String,
    title: String,
    body: String,
    route_uri: String,
}

/// Arguments for the native `cancelIncomingCall` handler (spec 08 C6).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CancelIncomingCallArgs {
    call_id: String,
}

/// Arguments for the native `setCallAudioRoute` handler (M-NATIVE-3, Step 11).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetCallAudioRouteArgs {
    call_id: String,
    route: String,
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
        ios_processing_ceiling_multiplier: Option<f64>,
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
            ios_processing_ceiling_multiplier,
        )
        .map(|_| ())
    }

    fn stop_keepalive(&self) -> Result<(), ServiceError> {
        self.stop_keepalive()
    }

    /// iOS BGTask scheduling is advisory (H9): it can be unavailable on the
    /// Simulator / a degraded device while the in-process Core still runs in
    /// the foreground, so a `start_keepalive` failure is a non-fatal degraded
    /// warning, not a rollback. Android foreground-service denials stay fatal.
    fn scheduling_is_advisory(&self) -> bool {
        cfg!(target_os = "ios")
    }

    /// Foreground-service *types* are an Android concept (M5/M6): Android
    /// validates the 14 valid types and swaps the running type via the native
    /// `updateForegroundServiceType` handler. iOS has no such handler, so type
    /// validation and the swap must not run there.
    fn enforces_foreground_service_type(&self) -> bool {
        cfg!(target_os = "android")
    }

    fn get_android_service_state(&self) -> Result<Option<AndroidServiceState>, ServiceError> {
        self.get_android_service_state()
    }

    fn get_ios_native_state(&self) -> Result<Option<IosNativeState>, ServiceError> {
        self.get_ios_native_state()
    }

    /// Tag the native authority by platform (H6 / L4): iOS returns the BGTask
    /// snapshot without ever touching the Android bridge; Android returns the
    /// foreground-service state.
    fn query_native_state(&self) -> Result<Option<NativeAuthority>, ServiceError> {
        #[cfg(target_os = "ios")]
        {
            Ok(self.get_ios_native_state()?.map(NativeAuthority::Ios))
        }
        #[cfg(not(target_os = "ios"))]
        {
            Ok(self
                .get_android_service_state()?
                .map(NativeAuthority::Android))
        }
    }

    // spec 08 C6 (Step 15): native call ringing + FGS-type swap.
    fn update_keepalive_type(&self, foreground_service_type: &str) -> Result<(), ServiceError> {
        self.update_keepalive_type(foreground_service_type)
    }

    fn show_incoming_call(
        &self,
        call_id: &str,
        caller_name: &str,
        is_video: bool,
    ) -> Result<(), ServiceError> {
        self.show_incoming_call(call_id, caller_name, is_video)
    }

    #[allow(clippy::too_many_arguments)]
    fn show_message_notification(
        &self,
        notification_id: i32,
        chat_id: &str,
        message_id: &str,
        title: &str,
        body: &str,
        route_uri: &str,
    ) -> Result<(), ServiceError> {
        self.show_message_notification(notification_id, chat_id, message_id, title, body, route_uri)
    }

    fn cancel_incoming_call(&self, call_id: &str) -> Result<(), ServiceError> {
        self.cancel_incoming_call(call_id)
    }

    fn set_call_audio_route(&self, call_id: &str, route: &str) -> Result<(), ServiceError> {
        self.set_call_audio_route(call_id, route)
    }

    fn open_app_settings(&self) -> Result<(), ServiceError> {
        self.open_app_settings()
    }

    fn mirror_desired_state(
        &self,
        desired_running: bool,
        last_start_config: Option<&serde_json::Value>,
    ) -> Result<(), ServiceError> {
        self.mirror_desired_state(desired_running, last_start_config)
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
