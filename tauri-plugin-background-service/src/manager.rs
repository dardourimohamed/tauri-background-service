//! Actor-based service manager.
//!
//! The [`manager_loop`] function runs as a single-owner Tokio task that receives
//! [`ManagerCommand`] messages through an `mpsc` channel. This serialises all
//! state mutations (start, stop, is_running) and prevents concurrent interleaving.
//!
//! Most of this module is `pub(crate)` — the public API surface is re-exported
//! from the crate root. Items that are `pub` only for the iOS lifecycle bridge
//! are marked `#[doc(hidden)]`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::desired_state::DesiredStateBackend;
use crate::error::ServiceError;
use crate::models::{
    validate_fg_type_against_allowlist, validate_foreground_service_type, LifecycleMode,
    LifecycleState, LifecycleStatus, PluginEvent, ServiceContext, ServiceState as ServiceLifecycle,
    ServiceStatus, StartConfig, StopReason, ValidationIssue,
};
use crate::notifier::{Notifier, NotifierPolicy, NotifySink};
use crate::service_trait::BackgroundService;

/// Callback fired when the service task completes. Receives `true` on success.
#[doc(hidden)]
pub type OnCompleteCallback = Box<dyn Fn(bool) + Send + Sync>;

/// Abstraction over mobile keepalive operations.
///
/// Defined here (not behind `#[cfg(mobile)]`) so the actor can reference it
/// on all platforms. On desktop, `ServiceState.mobile` is `None` and these
/// methods are never called. On mobile, `MobileLifecycle` implements this trait.
pub(crate) trait MobileKeepalive: Send + Sync {
    /// Start the OS-specific keepalive (Android foreground service / iOS BGTask).
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
    ) -> Result<(), ServiceError>;
    /// Stop the OS-specific keepalive.
    fn stop_keepalive(&self) -> Result<(), ServiceError>;
    /// Whether a `start_keepalive` failure should be treated as a non-fatal
    /// degraded warning rather than a fatal start rollback (H9).
    ///
    /// `true` on iOS: BGTask scheduling can be unavailable (Simulator /
    /// degraded device) while the in-process Core still runs in the
    /// foreground — so a scheduling failure leaves the service running and
    /// surfaces a "scheduling degraded / foreground-only" status. `false`
    /// (default) on Android/desktop, where a foreground-service denial is a
    /// genuine start failure that must roll back.
    fn scheduling_is_advisory(&self) -> bool {
        false
    }
    /// Query the Android native service state from the Kotlin bridge.
    ///
    /// Returns `None` on non-Android platforms (iOS, desktop). On Android,
    /// returns `Some(AndroidServiceState)` with the native service status or
    /// an error if the bridge call fails.
    fn get_android_service_state(
        &self,
    ) -> Result<Option<crate::models::AndroidServiceState>, ServiceError> {
        Ok(None)
    }
    /// Whether the OS enforces foreground-service *types* (Android only, M5/M6).
    ///
    /// Android returns `true`: the 14 valid types are validated
    /// ([`validate_foreground_service_type`]) and the running service's type can
    /// be swapped (`remoteMessaging` ↔ `phoneCall`) via the native
    /// `updateForegroundServiceType` handler. iOS and desktop return `false` —
    /// neither has an OS foreground-service-type concept, so the type validation
    /// (M6) and the native type-swap (M5) must NOT run there (calling them on iOS
    /// only produced missing-native-method error noise). The real
    /// [`MobileLifecycle`](crate::mobile::MobileLifecycle) override returns
    /// `cfg!(target_os = "android")`; the host mock lets tests simulate each
    /// platform. Default `false` (desktop / no bridge attached).
    fn enforces_foreground_service_type(&self) -> bool {
        false
    }
    /// Swap the foreground service type of the already-running service
    /// (Android only, spec 08 C6 Step 15): `remoteMessaging` → `phoneCall`
    /// on answer and back on end, without restarting the headless core.
    /// Default no-op on non-Android / desktop. The caller additionally gates
    /// this behind [`enforces_foreground_service_type`](Self::enforces_foreground_service_type)
    /// so iOS never reaches the native handler (M5).
    fn update_keepalive_type(&self, _foreground_service_type: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    /// Fire the native incoming-call notification (Android only, spec 08 C6):
    /// `CallStyle` + full-screen intent when granted, ringtone fallback (F4)
    /// otherwise. Default no-op on non-Android / desktop.
    fn show_incoming_call(
        &self,
        _call_id: &str,
        _caller_name: &str,
        _is_video: bool,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    /// Fire an actionable native message notification. Android implements
    /// content tap, inline reply, and mark-as-read without requiring the
    /// webview. Other platforms no-op unless they add a real native handler.
    #[allow(clippy::too_many_arguments)]
    fn show_message_notification(
        &self,
        _notification_id: i32,
        _chat_id: &str,
        _message_id: &str,
        _title: &str,
        _body: &str,
        _route_uri: &str,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    /// Cancel the native incoming-call notification (Android only, spec 08 C6).
    fn cancel_incoming_call(&self, _call_id: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    /// Set the active call's device audio route (M-NATIVE-3 / CCF-11, Step 11):
    /// `speaker`/`earpiece`/`bluetooth`/`system`. Android applies it to the live
    /// self-managed `SilaCallConnection` via `Connection.setAudioRoute`; iOS via
    /// `AVAudioSession.overrideOutputAudioPort`. Default no-op on non-mobile / desktop.
    fn set_call_audio_route(&self, _call_id: &str, _route: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    /// Open the OS app-settings screen (M-DIAG-2 / CCF-12, Step 17): Android
    /// opens the app-details / permission settings; iOS opens
    /// `UIApplication.openSettingsURLString`. Default no-op on non-mobile / desktop.
    fn open_app_settings(&self) -> Result<(), ServiceError> {
        Ok(())
    }
    /// Mirror the Rust-authoritative desired state into native persistence
    /// (H4 / D1). On iOS this writes `desiredRunning` (+ optional
    /// `last_start_config`) into `UserDefaults` and (re)schedules or cancels
    /// BGTasks so the intent-only recovery commands have a real, observable
    /// effect — never a silent `Ok`. Default no-op on non-iOS / desktop, where
    /// the start/stop keepalive paths (or Kotlin `DurableState`) already own
    /// native desired-state persistence.
    fn mirror_desired_state(
        &self,
        _desired_running: bool,
        _last_start_config: Option<&serde_json::Value>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    /// Query the platform-tagged native authority for reconcile + status (H6).
    ///
    /// Default: Android authority via the Kotlin bridge — preserves the
    /// existing behavior for the Android (and desktop/host-mock) path. iOS
    /// overrides this to return an [`NativeAuthority::Ios`] snapshot assembled
    /// from the typed iOS queries, so the status path never round-trips to
    /// `get_android_service_state` on iOS (L4).
    fn query_native_state(&self) -> Result<Option<NativeAuthority>, ServiceError> {
        Ok(self
            .get_android_service_state()?
            .map(NativeAuthority::Android))
    }
    /// Query the iOS native background-task snapshot (H6).
    ///
    /// Returns `None` on non-iOS / desktop. On iOS, assembled from
    /// `getSchedulingStatus` / `getDesiredStateStatus` / `getPendingBgTask`.
    /// Consumed by the real iOS `MobileLifecycle::query_native_state` override
    /// and the host iOS-mock tests; the desktop/Android path uses the
    /// `query_native_state` default instead.
    #[allow(dead_code)]
    fn get_ios_native_state(&self) -> Result<Option<crate::models::IosNativeState>, ServiceError> {
        Ok(None)
    }
}

/// Platform-tagged native authority returned by
/// [`MobileKeepalive::query_native_state`].
///
/// Lets `build_lifecycle_status` / `reconcile_running_with_native` consume the
/// correct native source per platform without the status path calling
/// `get_android_service_state` on iOS (L4): `Android` carries the long-lived
/// foreground-service state; `Ios` carries the BGTask/scheduling snapshot (H6).
#[allow(dead_code)] // `Ios` is constructed only on iOS / in tests.
pub(crate) enum NativeAuthority {
    Android(crate::models::AndroidServiceState),
    Ios(crate::models::IosNativeState),
}

/// Type-erased factory: produces a fresh `Box<dyn BackgroundService<R>>` on demand.
#[doc(hidden)]
pub type ServiceFactory<R> = Box<dyn Fn() -> Box<dyn BackgroundService<R>> + Send + Sync>;

// ─── Commands ───────────────────────────────────────────────────────────

/// Commands sent to the service manager actor.
///
/// Internal implementation detail — not part of the public API.
///
/// This enum is `#[non_exhaustive]` to prevent external construction.
/// Use [`ServiceManagerHandle`] methods instead.
#[non_exhaustive]
pub enum ManagerCommand<R: Runtime> {
    Start {
        config: StartConfig,
        reply: oneshot::Sender<Result<(), ServiceError>>,
        app: AppHandle<R>,
    },
    Stop {
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    StopWithReason {
        reason: StopReason,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    /// spec 08 C6 (Step 15): swap the running FGS type (remoteMessaging ↔
    /// phoneCall) without restarting the headless core.
    UpdateForegroundServiceType {
        foreground_service_type: String,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    /// spec 08 C6 (Step 15): fire the native incoming-call notification.
    NotifyIncomingCall {
        call_id: String,
        caller_name: String,
        is_video: bool,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    /// Fire an actionable native message notification.
    NotifyMessage {
        notification_id: i32,
        chat_id: String,
        message_id: String,
        title: String,
        body: String,
        route_uri: String,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    /// spec 08 C6 (Step 15): cancel the native incoming-call notification.
    CancelIncomingCall {
        call_id: String,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    /// M-NATIVE-3 (Step 11): set the active call's device audio route.
    SetCallAudioRoute {
        call_id: String,
        route: String,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    /// M-DIAG-2 (Step 17): open the OS app-settings screen so the user can grant
    /// a denied camera/mic permission.
    OpenAppSettings {
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    IsRunning {
        reply: oneshot::Sender<bool>,
    },
    GetState {
        reply: oneshot::Sender<ServiceStatus>,
    },
    SetOnComplete {
        callback: OnCompleteCallback,
    },
    #[allow(dead_code, private_interfaces)]
    SetMobile {
        mobile: Arc<dyn MobileKeepalive>,
    },
    SetDesiredRunning {
        desired: bool,
        config: Option<StartConfig>,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    EnableAutoRestart {
        config: Option<StartConfig>,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    DisableAutoRestart {
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    GetDesiredState {
        reply: oneshot::Sender<Option<crate::desired_state::DesiredState>>,
    },
    NativeLifecycleEvent {
        event: crate::models::NativeLifecycleEvent,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    GetLifecycleStatus {
        desktop_mode: Option<String>,
        reply: oneshot::Sender<LifecycleStatus>,
    },
    /// Gracefully drain service-owned state before the host process exits
    /// (BGS-31, doc-08 Step 9). Sent by the headless SIGTERM/SIGINT handler
    /// AFTER `Stop`, so a service override can perform a bounded Core-level
    /// drain instead of the abrupt process-exit `Drop` abort. Appended at the
    /// END of this `#[non_exhaustive]` enum (contract-compliant, non-breaking).
    ShutdownGracefully {
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
}

// ─── Handle ────────────────────────────────────────────────────────────

/// Handle to the service manager actor. Stored as Tauri managed state.
///
/// Tauri commands send messages through the internal channel; the actor
/// task processes them sequentially, preventing concurrent start/stop
/// interleaving.
pub struct ServiceManagerHandle<R: Runtime> {
    pub(crate) cmd_tx: mpsc::Sender<ManagerCommand<R>>,
}

impl<R: Runtime> ServiceManagerHandle<R> {
    /// Create a new handle backed by the given channel sender.
    pub fn new(cmd_tx: mpsc::Sender<ManagerCommand<R>>) -> Self {
        Self { cmd_tx }
    }

    /// Start a background service.
    ///
    /// Sends a `Start` command to the actor. Returns `AlreadyRunning` if a
    /// service is already active.
    pub async fn start(&self, app: AppHandle<R>, config: StartConfig) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::Start { config, reply, app })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Stop the running background service.
    ///
    /// Sends a `Stop` command to the actor. Returns `NotRunning` if no
    /// service is active.
    pub async fn stop(&self) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::Stop { reply })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Swap the foreground service type of the running service (spec 08 C6,
    /// Step 15) — e.g. `remoteMessaging` → `phoneCall` on call answer.
    ///
    /// Validates the type against the Android valid-types list and the plugin
    /// config allowlist, then forwards to the mobile bridge. Returns
    /// `NotRunning` if no service is active. No-op (Ok) on desktop.
    pub async fn update_foreground_service_type(
        &self,
        foreground_service_type: String,
    ) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::UpdateForegroundServiceType {
                foreground_service_type,
                reply,
            })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Fire the native incoming-call notification (spec 08 C6, Step 15).
    pub async fn notify_incoming_call(
        &self,
        call_id: String,
        caller_name: String,
        is_video: bool,
    ) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::NotifyIncomingCall {
                call_id,
                caller_name,
                is_video,
                reply,
            })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Fire an actionable native message notification.
    #[allow(clippy::too_many_arguments)]
    pub async fn notify_message(
        &self,
        notification_id: i32,
        chat_id: String,
        message_id: String,
        title: String,
        body: String,
        route_uri: String,
    ) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::NotifyMessage {
                notification_id,
                chat_id,
                message_id,
                title,
                body,
                route_uri,
                reply,
            })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Cancel the native incoming-call notification (spec 08 C6, Step 15).
    pub async fn cancel_incoming_call(&self, call_id: String) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::CancelIncomingCall { call_id, reply })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Set the active call's device audio route (M-NATIVE-3 / CCF-11, Step 11).
    pub async fn set_call_audio_route(
        &self,
        call_id: String,
        route: String,
    ) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::SetCallAudioRoute {
                call_id,
                route,
                reply,
            })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Open the OS app-settings screen (M-DIAG-2 / CCF-12, Step 17) so the user
    /// can grant a denied camera/mic permission. Forwards to the native plugin
    /// (Android settings intent / iOS `openSettingsURLString`); a no-op when no
    /// mobile bridge is attached.
    pub async fn open_app_settings(&self) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::OpenAppSettings { reply })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Stop the running background service synchronously.
    ///
    /// Uses `blocking_send` so this can be called from synchronous contexts
    /// (e.g., a Tauri `on_event` closure). Returns `NotRunning` if no
    /// service is active.
    pub fn stop_blocking(&self) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .blocking_send(ManagerCommand::Stop { reply })
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.blocking_recv()
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Stop the running background service with a specific reason.
    ///
    /// Applies a reason-based desired-state policy: intentional stops
    /// (UserStop, AppStop, etc.) clear desired state, while platform
    /// errors and timeouts preserve it for auto-restart recovery.
    pub async fn stop_with_reason(&self, reason: StopReason) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::StopWithReason { reason, reply })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Gracefully drain service-owned state before exit (BGS-31, doc-08 Step 9).
    ///
    /// Sends a `ShutdownGracefully` command that drives the registered
    /// service's [`BackgroundService::shutdown_gracefully`] hook (a bounded
    /// Core-level drain). Intended to be sent AFTER [`stop`](Self::stop) from
    /// the headless SIGTERM/SIGINT handler, so the bookkeeping `Stop` and the
    /// bounded drain both complete before the IPC token is cancelled.
    pub async fn shutdown_gracefully(&self) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::ShutdownGracefully { reply })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Stop the running background service synchronously with a specific reason.
    ///
    /// Blocking variant of [`ServiceManagerHandle::stop_with_reason`].
    pub fn stop_blocking_with_reason(&self, reason: StopReason) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .blocking_send(ManagerCommand::StopWithReason { reason, reply })
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.blocking_recv()
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }

    /// Check whether a background service is currently running.
    pub async fn is_running(&self) -> bool {
        let (reply, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(ManagerCommand::IsRunning { reply })
            .await
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Set the callback fired when the service task completes.
    ///
    /// The callback is captured at spawn time (generation-guarded), so calling
    /// this while a service is running will only affect the *next* start.
    #[doc(hidden)]
    pub async fn set_on_complete(&self, callback: OnCompleteCallback) {
        let _ = self
            .cmd_tx
            .send(ManagerCommand::SetOnComplete { callback })
            .await;
    }

    /// Get the current service lifecycle status.
    pub async fn get_state(&self) -> ServiceStatus {
        let (reply, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(ManagerCommand::GetState { reply })
            .await
            .is_err()
        {
            return ServiceStatus {
                state: ServiceLifecycle::Idle,
                ..Default::default()
            };
        }
        rx.await.unwrap_or(ServiceStatus {
            state: ServiceLifecycle::Idle,
            ..Default::default()
        })
    }

    /// Send a native lifecycle event to the actor.
    ///
    /// Maps the native event to the appropriate [`StopReason`] and delegates
    /// to [`handle_stop_with_reason`].
    #[doc(hidden)]
    pub async fn send_native_lifecycle_event(
        &self,
        event: crate::models::NativeLifecycleEvent,
    ) -> Result<(), ServiceError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::NativeLifecycleEvent { event, reply })
            .await
            .map_err(|_| ServiceError::Runtime("manager actor shut down".into()))?;
        rx.await
            .map_err(|_| ServiceError::Runtime("manager actor dropped reply".into()))?
    }
}

// ─── Actor State ───────────────────────────────────────────────────────

/// Internal state owned exclusively by the actor task.
struct ServiceState<R: Runtime> {
    /// Fast path: `true` when a service task is active.
    /// Set by `handle_start`, cleared by `handle_stop` or task cleanup.
    /// Avoids acquiring the Mutex for status-only queries.
    is_running: Arc<AtomicBool>,
    /// Cancellation token: `Some` means a service is running.
    /// Shared with the spawned service task via `Arc<Mutex<>>` so it can
    /// clear the slot when the task finishes.
    token: Arc<Mutex<Option<CancellationToken>>>,
    /// Generation counter for the race-condition guard.
    /// Incremented on each start; shared via `Arc<AtomicU64>`.
    generation: Arc<AtomicU64>,
    /// Callback fired once when the service task completes.
    /// Captured via `take()` at spawn time so a new callback can be set
    /// for the next start.
    on_complete: Option<OnCompleteCallback>,
    /// Factory that creates fresh service instances.
    factory: ServiceFactory<R>,
    /// Mobile keepalive handle. Set via `SetMobile` command on mobile platforms.
    mobile: Option<Arc<dyn MobileKeepalive>>,
    /// Last `AppHandle` provided by a `Start` command.
    /// Used for event emission during merge (degraded-state detection).
    app: Option<AppHandle<R>>,
    /// iOS safety timeout in seconds (from PluginConfig, default 28.0).
    /// Passed to mobile via `start_keepalive`. Android ignores this field.
    ios_safety_timeout_secs: f64,
    /// iOS BGProcessingTask safety timeout in seconds (from PluginConfig, default 0.0).
    /// When > 0.0, caps processing task duration. Passed as `Some(value)` to mobile.
    /// When 0.0, passed as `None` (no cap).
    ios_processing_safety_timeout_secs: f64,
    /// iOS BGAppRefreshTask earliest begin date in minutes (default 15.0).
    ios_earliest_refresh_begin_minutes: f64,
    /// iOS BGProcessingTask earliest begin date in minutes (default 15.0).
    ios_earliest_processing_begin_minutes: f64,
    /// iOS BGProcessingTask requires external power (default false).
    ios_requires_external_power: bool,
    /// iOS BGProcessingTask requires network connectivity (default false).
    ios_requires_network_connectivity: bool,
    /// iOS adaptive processing ceiling multiplier (default 4.0).
    /// Bounds the Swift adaptive scheduler's back-off for the processing task.
    ios_processing_ceiling_multiplier: f64,
    /// Current lifecycle state of the service.
    /// Shared with spawned task for transitions (Initializing→Running→Stopped).
    lifecycle_state: Arc<Mutex<ServiceLifecycle>>,
    /// Last error message from init/run failure.
    /// Shared with spawned task for error capture.
    last_error: Arc<Mutex<Option<String>>>,
    /// Set by `handle_start` when an *advisory* (iOS BGTask) `start_keepalive`
    /// fails: the Core keeps running in the foreground but background scheduling
    /// is unavailable. Surfaced by `build_lifecycle_status` as a distinct
    /// "scheduling degraded / foreground-only" status (H9). Cleared on the next
    /// start attempt and on stop.
    scheduling_degraded: Arc<Mutex<Option<String>>>,
    /// Desired-state persistence backend.
    /// `None` on platforms that haven't set one up yet.
    desired_state: Option<Arc<dyn DesiredStateBackend>>,
    /// Current platform's lifecycle mode (FGS, BGTask, in-process, OS-service).
    lifecycle_mode: LifecycleMode,
    /// Android foreground service types allowed by plugin config.
    /// Used by `handle_start` to validate before calling mobile start.
    android_fg_service_types: Vec<String>,
    /// Whether to validate the requested foreground service type against
    /// `android_fg_service_types` before starting the native service.
    android_validate_fg_type: bool,
    /// Which lifecycle notifications are enabled, derived from PluginConfig
    /// per DEC-002 (Android suppression). Default: everything off.
    notifier_policy: NotifierPolicy,
    /// Notification dispatch seam. `None` when no notification-capable
    /// app handle is available (headless daemon, tests without a sink).
    notify_sink: Option<Arc<dyn NotifySink>>,
}

// ─── Actor Loop ────────────────────────────────────────────────────────

/// Main actor loop: receives commands and dispatches to handlers.
///
/// Runs as a spawned Tokio task. The loop exits when all `Sender` halves
/// are dropped (i.e., the handle is dropped).
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn manager_loop<R: Runtime>(
    mut rx: mpsc::Receiver<ManagerCommand<R>>,
    factory: ServiceFactory<R>,
    // iOS safety timeout in seconds. From PluginConfig.
    // Default: 28.0 (Apple recommends keeping BG tasks under ~30s).
    // Passed to mobile via actor's `start_keepalive` call.
    ios_safety_timeout_secs: f64,
    // iOS BGProcessingTask safety timeout in seconds. From PluginConfig.
    // Default: 0.0 (no cap). When > 0.0, passed as Some(value) to mobile.
    ios_processing_safety_timeout_secs: f64,
    // iOS BGAppRefreshTask earliest begin date in minutes. From PluginConfig.
    ios_earliest_refresh_begin_minutes: f64,
    // iOS BGProcessingTask earliest begin date in minutes. From PluginConfig.
    ios_earliest_processing_begin_minutes: f64,
    // iOS BGProcessingTask requires external power. From PluginConfig.
    ios_requires_external_power: bool,
    // iOS BGProcessingTask requires network connectivity. From PluginConfig.
    ios_requires_network_connectivity: bool,
    // iOS adaptive processing ceiling multiplier. From PluginConfig.
    // Default: 4.0. Bounds the Swift adaptive scheduler's back-off.
    ios_processing_ceiling_multiplier: f64,
    // Desired-state persistence backend. None if not configured.
    desired_state_backend: Option<Arc<dyn DesiredStateBackend>>,
    // Android foreground service type allowlist from PluginConfig.
    android_fg_service_types: Vec<String>,
    // Whether to validate foreground service type against the allowlist.
    android_validate_fg_type: bool,
    // Lifecycle-notification policy derived from PluginConfig (D1, DEC-002).
    // Default: everything off.
    notifier_policy: NotifierPolicy,
    // Notification dispatch seam. None if no notification-capable app
    // handle exists at spawn (headless daemon, tests without a sink).
    notify_sink: Option<Arc<dyn NotifySink>>,
    // BGS-05 Leg B: an optional AppHandle for boot Start-replay. When `Some`
    // AND desktop lifecycle mode AND the persisted desired-state says
    // `desired_running`, the loop replays a `Start` on entry via `handle_start`
    // (the SAME path a runtime `ManagerCommand::Start` takes — minus the IPC
    // reply). `None` for GUI in-process + IPC-client callers (no boot-replay).
    boot_app: Option<AppHandle<R>>,
    // BGS-05 re-fix (Critic Blocker 2 — Leg A/Leg B coordination): the boot
    // Start-replay fires ONLY when the Sila app's consent policy allows
    // auto-unlock (`consent.enabled && consent.auto_unlock`, computed LIVE in
    // `run_headless` and threaded through `headless_main_with_desired_state`).
    // Before this, the replay guard was `desired_running`-ONLY — consent-blind —
    // so consent OFF + credential on disk + `desired_running=true` replayed a
    // Start that reached the (then consent-UN-aware) `start_headless_core` and
    // unlocked unattended. This is belt-and-suspenders alongside F3 (the
    // load-bearing builder gate in `start_headless_core`): `false` here simply
    // suppresses the spurious replay; F3 LIVE-reads consent regardless. GUI
    // in-process + IPC-client callers pass `false` (they set `boot_app = None`
    // so the replay short-circuits anyway).
    consent_allows_auto_unlock: bool,
) {
    let lifecycle_mode = {
        #[cfg(target_os = "android")]
        {
            LifecycleMode::AndroidForegroundService
        }
        #[cfg(target_os = "ios")]
        {
            LifecycleMode::IosBgTaskScheduler
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            LifecycleMode::DesktopInProcess
        }
    };

    let mut state = ServiceState {
        is_running: Arc::new(AtomicBool::new(false)),
        token: Arc::new(Mutex::new(None)),
        generation: Arc::new(AtomicU64::new(0)),
        on_complete: None,
        factory,
        mobile: None,
        app: None,
        ios_safety_timeout_secs,
        ios_processing_safety_timeout_secs,
        ios_earliest_refresh_begin_minutes,
        ios_earliest_processing_begin_minutes,
        ios_requires_external_power,
        ios_requires_network_connectivity,
        ios_processing_ceiling_multiplier,
        lifecycle_state: Arc::new(Mutex::new(ServiceLifecycle::Idle)),
        last_error: Arc::new(Mutex::new(None)),
        scheduling_degraded: Arc::new(Mutex::new(None)),
        desired_state: desired_state_backend,
        lifecycle_mode,
        android_fg_service_types,
        android_validate_fg_type,
        notifier_policy,
        notify_sink,
    };

    // BGS-05 Leg B: replay a Start on boot if the user last left the service
    // desired-running (desktop only; Android/iOS use their own native resubmit
    // at the platform layer). The dispatch IS `handle_start`, the same path a
    // runtime `ManagerCommand::Start` takes.
    //
    // BGS-05 re-fix (Critic Blocker 2): the replay is ALSO gated on
    // `consent_allows_auto_unlock` (threaded LIVE from `run_headless`'s consent
    // read) — consent OFF ⇒ no replay ⇒ no boot-reachable Start. The original
    // guard was `desired_running`-only (consent-blind), which combined with the
    // then-ungated `start_headless_core` unlocked the daemon unattended with
    // consent OFF. F3 (`start_headless_core`'s LIVE consent gate) is the
    // load-bearing backstop; this coordination suppresses the spurious replay
    // and keeps Leg A (boot_restore_core) + Leg B (this replay) acting on the
    // SAME consent decision.
    if matches!(
        state.lifecycle_mode,
        LifecycleMode::DesktopInProcess | LifecycleMode::DesktopOsService
    ) && boot_app.is_some()
        && consent_allows_auto_unlock
    {
        if let Some(ref backend) = state.desired_state {
            if let Ok(ds) = backend.load() {
                if should_replay_on_boot(&ds) {
                    if let Some(app) = boot_app.as_ref() {
                        let config = ds
                            .last_start_config
                            .as_ref()
                            .and_then(|v| serde_json::from_value::<StartConfig>(v.clone()).ok());
                        if let Some(config) = config {
                            match handle_start(&mut state, app.clone(), config) {
                                Ok(()) => {
                                    log::info!(
                                        "BGS-05: replayed Start on boot (desired_running=true)"
                                    );
                                }
                                Err(e) => {
                                    log::warn!("BGS-05: boot Start-replay failed: {e}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    while let Some(cmd) = rx.recv().await {
        match cmd {
            ManagerCommand::Start { config, reply, app } => {
                let _ = reply.send(handle_start(&mut state, app, config));
            }
            ManagerCommand::Stop { reply } => {
                let _ = reply.send(handle_stop(&mut state));
            }
            ManagerCommand::StopWithReason { reason, reply } => {
                let _ = reply.send(handle_stop_with_reason(&mut state, reason));
            }
            ManagerCommand::ShutdownGracefully { reply } => {
                // BGS-31 (doc-08 Step 9): bounded Core-level drain driven from
                // the SIGTERM/SIGINT handler. The service override reaches
                // process-wide state (the Sila Core) through `ctx.app`; the
                // running service task is owned by a spawned task and is not
                // reachable here, so `handle_shutdown_gracefully` constructs a
                // fresh service via the factory + a context from the last
                // `AppHandle` and invokes the hook on it.
                let result = handle_shutdown_gracefully(&mut state).await;
                let _ = reply.send(result);
            }
            ManagerCommand::UpdateForegroundServiceType {
                foreground_service_type,
                reply,
            } => {
                let _ = reply.send(handle_update_foreground_service_type(
                    &mut state,
                    foreground_service_type,
                ));
            }
            ManagerCommand::NotifyIncomingCall {
                call_id,
                caller_name,
                is_video,
                reply,
            } => {
                let _ = reply.send(handle_notify_incoming_call(
                    &state,
                    call_id,
                    caller_name,
                    is_video,
                ));
            }
            ManagerCommand::NotifyMessage {
                notification_id,
                chat_id,
                message_id,
                title,
                body,
                route_uri,
                reply,
            } => {
                let _ = reply.send(handle_notify_message(
                    &state,
                    notification_id,
                    chat_id,
                    message_id,
                    title,
                    body,
                    route_uri,
                ));
            }
            ManagerCommand::CancelIncomingCall { call_id, reply } => {
                let _ = reply.send(handle_cancel_incoming_call(&state, call_id));
            }
            ManagerCommand::SetCallAudioRoute {
                call_id,
                route,
                reply,
            } => {
                let _ = reply.send(handle_set_call_audio_route(&state, call_id, route));
            }
            ManagerCommand::OpenAppSettings { reply } => {
                let _ = reply.send(handle_open_app_settings(&state));
            }
            ManagerCommand::IsRunning { reply } => {
                // Native `LifecycleService.isRunning` is the single source of
                // truth (R-W1.4): reconcile the actor's belief before reporting
                // so a stop/timeout with the UI closed can never surface a stale
                // "running" (closes the split-brain window — harness Scenario 8).
                let _ = reply.send(reconcile_running_with_native(&state));
            }
            ManagerCommand::SetOnComplete { callback } => {
                state.on_complete = Some(callback);
            }
            ManagerCommand::SetMobile { mobile } => {
                state.mobile = Some(mobile);
            }
            ManagerCommand::GetState { reply } => {
                let mut status = ServiceStatus {
                    state: *state.lifecycle_state.lock().unwrap(),
                    last_error: state.last_error.lock().unwrap().clone(),
                    platform_mode: Some(state.lifecycle_mode),
                    ..Default::default()
                };

                if let Some(ref backend) = state.desired_state {
                    if let Ok(ds) = backend.load() {
                        status.desired_running = Some(ds.desired_running);
                        status.native_state = ds
                            .last_native_state
                            .as_deref()
                            .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok());
                        status.last_start_config = ds
                            .last_start_config
                            .and_then(|v| serde_json::from_value(v).ok());
                        status.last_heartbeat_at = ds.last_heartbeat_epoch_ms;
                        status.restart_attempt = if ds.restart_attempt > 0 {
                            Some(ds.restart_attempt)
                        } else {
                            None
                        };
                        status.recovery_reason = ds.recovery_reason;
                        status.platform_error = ds.last_platform_error;
                    }
                }

                let _ = reply.send(status);
            }
            ManagerCommand::SetDesiredRunning {
                desired,
                config,
                reply,
            } => {
                let _ = reply.send(handle_set_desired_running(&mut state, desired, config));
            }
            ManagerCommand::EnableAutoRestart { config, reply } => {
                let _ = reply.send(handle_enable_auto_restart(&mut state, config));
            }
            ManagerCommand::DisableAutoRestart { reply } => {
                let _ = reply.send(handle_disable_auto_restart(&mut state));
            }
            ManagerCommand::GetDesiredState { reply } => {
                let _ = reply.send(handle_get_desired_state(&state));
            }
            ManagerCommand::NativeLifecycleEvent { event, reply } => {
                let _ = reply.send(handle_native_lifecycle_event(&mut state, event));
            }
            ManagerCommand::GetLifecycleStatus {
                desktop_mode,
                reply,
            } => {
                let _ = reply.send(build_lifecycle_status(&state, desktop_mode.as_deref()));
            }
        }
    }
}

// ─── Command Handlers ──────────────────────────────────────────────────

/// Handle a `Start` command.
///
/// Order of operations (critical for the race-condition fix):
/// 1. Check `AlreadyRunning` — reject early, no side-effects.
/// 2. Create token, increment generation.
/// 3. Start mobile keepalive (AFTER AlreadyRunning check).
///    On failure: rollback token and callback, return error.
/// 4. Spawn service task (init -> run -> cleanup).
fn handle_start<R: Runtime>(
    state: &mut ServiceState<R>,
    app: AppHandle<R>,
    config: StartConfig,
) -> Result<(), ServiceError> {
    log::info!("handle_start: entry (label={})", config.service_label);

    // Store the app handle for event emission during merge.
    state.app = Some(app.clone());

    let mut guard = state.token.lock().unwrap();

    if guard.is_some() {
        return Err(ServiceError::AlreadyRunning);
    }

    // Validate foreground service type against the 14 valid Android types.
    // Only relevant where the OS enforces foreground-service types — i.e.
    // Android (M6). iOS has no FGS-type concept, and on desktop the type is
    // ignored (no OS enforcement mechanism), so both skip this. Gated through
    // the bridge so the decision is platform-accurate and host-testable.
    if state
        .mobile
        .as_ref()
        .is_some_and(|m| m.enforces_foreground_service_type())
    {
        validate_foreground_service_type(&config.foreground_service_type)?;
    }

    // Validate against plugin config allowlist (all platforms).
    // This is the user-configured restriction: even if a type is valid
    // (one of the 14 Android types), it must also be in the plugin's
    // android_foreground_service_types list.
    validate_fg_type_against_allowlist(
        &config.foreground_service_type,
        &state.android_fg_service_types,
        state.android_validate_fg_type,
    )?;

    let token = CancellationToken::new();
    let shutdown = token.clone();
    *guard = Some(token);
    let my_gen = state.generation.fetch_add(1, Ordering::Release) + 1;
    state.is_running.store(true, Ordering::SeqCst);
    *state.lifecycle_state.lock().unwrap() = ServiceLifecycle::Initializing;
    *state.last_error.lock().unwrap() = None;
    // Clear any prior advisory scheduling-degraded marker for this fresh start.
    *state.scheduling_degraded.lock().unwrap() = None;

    drop(guard);

    // Capture on_complete at spawn time (generation-guarded).
    // Takes the callback out of the slot so a new start can set a fresh one.
    let captured_callback = state.on_complete.take();

    // Start mobile keepalive AFTER AlreadyRunning check.
    //
    // On Android/desktop a keepalive failure is fatal: rollback (clear token,
    // restore callback) and propagate the error.
    //
    // On iOS (H9), BGTask scheduling is *advisory* — it can be unavailable on
    // the Simulator / a degraded device while the in-process Core still runs in
    // the foreground. There a failure must NOT roll back the service: record a
    // distinct "scheduling degraded / foreground-only" marker, emit a non-fatal
    // warning, and fall through to spawn the Core anyway.

    if let Some(ref mobile) = state.mobile {
        let processing_timeout = if state.ios_processing_safety_timeout_secs > 0.0 {
            Some(state.ios_processing_safety_timeout_secs)
        } else {
            None
        };
        if let Err(e) = mobile.start_keepalive(
            &config.service_label,
            &config.foreground_service_type,
            Some(state.ios_safety_timeout_secs),
            processing_timeout,
            Some(state.ios_earliest_refresh_begin_minutes),
            Some(state.ios_earliest_processing_begin_minutes),
            Some(state.ios_requires_external_power),
            Some(state.ios_requires_network_connectivity),
            Some(state.ios_processing_ceiling_multiplier),
        ) {
            if mobile.scheduling_is_advisory() {
                // Non-fatal: the Core still starts in the foreground; only
                // background scheduling is degraded.
                log::warn!(
                    "start_keepalive: advisory scheduling unavailable ({e}); \
                     starting Core foreground-only (degraded)"
                );
                *state.scheduling_degraded.lock().unwrap() = Some(e.to_string());
                let _ = app.emit(
                    "background-service:state-degraded",
                    serde_json::json!({
                        "degraded": true,
                        "reason": "scheduling_degraded_foreground_only",
                        "error": e.to_string(),
                    }),
                );
                // Fall through: do NOT roll back.
            } else {
                // Rollback: clear the token we just set.
                state.token.lock().unwrap().take();
                state.is_running.store(false, Ordering::SeqCst);
                *state.lifecycle_state.lock().unwrap() = ServiceLifecycle::Idle;
                // Rollback: restore the callback we took.
                state.on_complete = captured_callback;
                return Err(e);
            }
        }
    }

    // Shared refs for the spawned task's cleanup logic.
    let token_ref = state.token.clone();
    let gen_ref = state.generation.clone();
    let is_running_ref = state.is_running.clone();
    let lifecycle_ref = state.lifecycle_state.clone();
    let last_error_ref = state.last_error.clone();

    let mut service = (state.factory)();

    let ctx = ServiceContext {
        notifier: Notifier { app: app.clone() },
        app: app.clone(),
        shutdown,
        #[cfg(mobile)]
        service_label: config.service_label.clone(),
        #[cfg(mobile)]
        foreground_service_type: config.foreground_service_type.clone(),
    };

    // Use tauri::async_runtime::spawn() instead of tokio::spawn() because
    // the plugin setup closure may run before a Tokio runtime context is
    // entered on the current thread (e.g. Android auto-start in setup).
    tauri::async_runtime::spawn(async move {
        // Phase 1: init
        if let Err(e) = service.init(&ctx).await {
            let _ = app.emit(
                "background-service://event",
                PluginEvent::Error {
                    message: e.to_string(),
                },
            );
            // Clear token only if generation hasn't advanced. Hold the token
            // lock across the generation check so a concurrent handle_start —
            // which bumps the generation while holding this same lock — cannot
            // have its freshly installed token taken by this stale task.
            {
                let mut tok = token_ref.lock().unwrap();
                if gen_ref.load(Ordering::Acquire) == my_gen {
                    tok.take();
                    is_running_ref.store(false, Ordering::SeqCst);
                    // Initializing → Stopped on init failure.
                    {
                        let mut lc = lifecycle_ref.lock().unwrap();
                        if *lc == ServiceLifecycle::Initializing {
                            *lc = ServiceLifecycle::Stopped;
                        }
                    }
                    *last_error_ref.lock().unwrap() = Some(e.to_string());
                }
            }
            // Fire callback with false on init failure.
            if let Some(cb) = captured_callback {
                cb(false);
            }
            return;
        }

        // Initializing → Running after successful init (generation + state guarded).
        if gen_ref.load(Ordering::Acquire) == my_gen {
            let mut lc = lifecycle_ref.lock().unwrap();
            if *lc == ServiceLifecycle::Initializing {
                *lc = ServiceLifecycle::Running;
            }
        }

        // Emit Started
        let _ = app.emit("background-service://event", PluginEvent::Started);

        // Phase 2: run
        let result = service.run(&ctx).await;

        // Emit terminal event.
        match result {
            Ok(()) => {
                let _ = app.emit(
                    "background-service://event",
                    PluginEvent::Stopped {
                        reason: StopReason::TaskCompleted,
                    },
                );
            }
            Err(ref e) => {
                let _ = app.emit(
                    "background-service://event",
                    PluginEvent::Error {
                        message: e.to_string(),
                    },
                );
            }
        }

        // Fire on_complete callback (captured at spawn time).
        // MUST fire before clearing the token so that
        // `wait_until_stopped` only returns after the callback ran.
        if let Some(cb) = captured_callback {
            cb(result.is_ok());
        }

        // Clear token only if generation hasn't advanced. Hold the token lock
        // across the generation check so a concurrent handle_start — which bumps
        // the generation while holding this same lock — cannot have its freshly
        // installed token taken by this stale task. Without this, a restart that
        // races this cleanup loses its token and the next stop sees NotRunning.
        {
            let mut tok = token_ref.lock().unwrap();
            if gen_ref.load(Ordering::Acquire) == my_gen {
                tok.take();
                is_running_ref.store(false, Ordering::SeqCst);
                // → Stopped on run completion (generation guarded).
                {
                    let mut lc = lifecycle_ref.lock().unwrap();
                    if matches!(
                        *lc,
                        ServiceLifecycle::Initializing | ServiceLifecycle::Running
                    ) {
                        *lc = ServiceLifecycle::Stopped;
                    }
                }
                if let Err(ref e) = result {
                    *last_error_ref.lock().unwrap() = Some(e.to_string());
                }
            }
        }
    });

    // Persist desired_running=true after successful start.
    save_desired_running(state, true, Some(&config));

    Ok(())
}

/// Handle a `Stop` command.
///
/// Takes the token from state and cancels it, then stops mobile keepalive.
/// Returns `NotRunning` if no service is active.
fn handle_stop<R: Runtime>(state: &mut ServiceState<R>) -> Result<(), ServiceError> {
    handle_stop_with_reason(state, StopReason::UserStop)
}

/// Handle a `ShutdownGracefully` command (BGS-31, doc-08 Step 9).
///
/// Builds a fresh service instance via the factory + a [`ServiceContext`] from
/// the last `AppHandle` provided to `Start`, and invokes the service's
/// [`BackgroundService::shutdown_gracefully`] hook. The running service task is
/// owned by a spawned task and is not reachable from the actor loop; the hook
/// is intentionally STATELESS w.r.t. the instance — an override reaches
/// process-wide state (the Sila `Core`) through `ctx.app.state::<AppState>()`,
/// so invoking it on a fresh factory instance is equivalent to invoking it on
/// the running one.
///
/// Tolerates Core-absent / no-app: `state.app` is `None` until a `Start`
/// supplies an `AppHandle`, and the headless daemon's `AppState` is locked at
/// boot. If no `AppHandle` is available there is nothing to drain, so this
/// returns `Ok(())` (never panics). This is the bounded-drain path; it does
/// NOT reshape `Core::drop`.
async fn handle_shutdown_gracefully<R: Runtime>(
    state: &mut ServiceState<R>,
) -> Result<(), ServiceError> {
    let Some(app) = state.app.clone() else {
        // No AppHandle ever provided (no Start happened): nothing to drain.
        return Ok(());
    };
    let mut service = (state.factory)();
    let ctx = ServiceContext {
        notifier: Notifier { app: app.clone() },
        app,
        shutdown: CancellationToken::new(),
        #[cfg(mobile)]
        service_label: String::new(),
        #[cfg(mobile)]
        foreground_service_type: String::new(),
    };
    service.shutdown_gracefully(&ctx).await
}

/// Handle a `StopWithReason` command.
///
/// Like `handle_stop` but applies a reason-based desired-state policy:
/// - Clears desired state for intentional stops: `UserStop`, `AppStop`,
///   `NativeNotificationStop`, `TaskCompleted`.
/// - Preserves desired state for platform/error/exit reasons: `PlatformTimeout`,
///   `PlatformExpiration`, `OsRestart`, `BootRecovery`, `Error`, `ProcessExit`.
///   A `PlatformTimeout` additionally re-submits native scheduling (M13).
fn handle_stop_with_reason<R: Runtime>(
    state: &mut ServiceState<R>,
    reason: StopReason,
) -> Result<(), ServiceError> {
    let mut guard = state.token.lock().unwrap();
    match guard.take() {
        Some(token) => {
            token.cancel();
            state.is_running.store(false, Ordering::SeqCst);
            *state.lifecycle_state.lock().unwrap() = ServiceLifecycle::Stopped;
            *state.last_error.lock().unwrap() = None;
            // Clear any advisory scheduling-degraded marker (H9): once stopped,
            // foreground-only degradation no longer applies.
            *state.scheduling_degraded.lock().unwrap() = None;
            drop(guard);
            if should_stop_keepalive(reason) {
                if let Some(ref mobile) = state.mobile {
                    if let Err(e) = mobile.stop_keepalive() {
                        log::warn!("stop_keepalive failed: {e}");
                    }
                }
            }
            if should_clear_desired_state(reason) {
                save_desired_running(state, false, None);
            } else if should_reconcile_resubmit(reason) {
                // M13 reconcile: a cancel-listener PlatformTimeout paused the
                // service but `desired_running` survives. Surface the degraded
                // state and re-submit native scheduling (on iOS this reschedules
                // the BGTask) so background delivery resumes instead of silently
                // dying. No-op on desktop / when desired_running is false.
                if let Some(ds) = state.desired_state.as_ref().and_then(|b| b.load().ok()) {
                    if ds.desired_running {
                        log::warn!(
                            "background service degraded after platform timeout; \
                             desired_running=true — re-submitting native scheduling"
                        );
                        let config: Option<StartConfig> = ds
                            .last_start_config
                            .as_ref()
                            .and_then(|v| serde_json::from_value(v.clone()).ok());
                        mirror_desired_to_native(state, true, config.as_ref());
                    }
                }
            }
            // D1 timeout fire point: policy gate first, then the sink.
            if state.notifier_policy.on_timeout && should_notify_timeout(reason) {
                if let Some(ref sink) = state.notify_sink {
                    sink.notify(
                        "bg-timeout",
                        "Sila background service paused",
                        "The OS paused background delivery; it will resume automatically.",
                    );
                }
            }
            Ok(())
        }
        None => Err(ServiceError::NotRunning),
    }
}

/// Handle `UpdateForegroundServiceType` (spec 08 C6, Step 15).
///
/// Validates the type against the Android valid-types list (mobile) and the
/// plugin config allowlist (all platforms), then forwards to the mobile bridge
/// to swap the running FGS type (e.g. `remoteMessaging` → `phoneCall`) without
/// restarting the headless core. Returns `NotRunning` if no service is active.
fn handle_update_foreground_service_type<R: Runtime>(
    state: &mut ServiceState<R>,
    foreground_service_type: String,
) -> Result<(), ServiceError> {
    let running = state.token.lock().unwrap().is_some();
    if !running {
        return Err(ServiceError::NotRunning);
    }
    // Mirror handle_start's validation ordering. The 14-type validation and the
    // native type-swap are Android-only (M5/M6): iOS has no FGS-type concept, so
    // neither runs there. The plugin-config allowlist check stays all-platform.
    let enforces = state
        .mobile
        .as_ref()
        .is_some_and(|m| m.enforces_foreground_service_type());
    if enforces {
        validate_foreground_service_type(&foreground_service_type)?;
    }
    validate_fg_type_against_allowlist(
        &foreground_service_type,
        &state.android_fg_service_types,
        state.android_validate_fg_type,
    )?;
    if enforces {
        if let Some(ref mobile) = state.mobile {
            // The token stays; only the FGS *type* swaps.
            mobile.update_keepalive_type(&foreground_service_type)?;
        }
    }
    Ok(())
}

/// Handle `NotifyIncomingCall` (spec 08 C6, Step 15): fire the native
/// incoming-call notification. No-op (Ok) when no mobile bridge is attached.
fn handle_notify_incoming_call<R: Runtime>(
    state: &ServiceState<R>,
    call_id: String,
    caller_name: String,
    is_video: bool,
) -> Result<(), ServiceError> {
    if let Some(ref mobile) = state.mobile {
        mobile.show_incoming_call(&call_id, &caller_name, is_video)?;
    }
    Ok(())
}

/// Handle `NotifyMessage`: fire an actionable native message notification. No-op
/// when no mobile bridge is attached.
#[allow(clippy::too_many_arguments)]
fn handle_notify_message<R: Runtime>(
    state: &ServiceState<R>,
    notification_id: i32,
    chat_id: String,
    message_id: String,
    title: String,
    body: String,
    route_uri: String,
) -> Result<(), ServiceError> {
    if let Some(ref mobile) = state.mobile {
        mobile.show_message_notification(
            notification_id,
            &chat_id,
            &message_id,
            &title,
            &body,
            &route_uri,
        )?;
    }
    Ok(())
}

/// Handle `CancelIncomingCall` (spec 08 C6, Step 15).
fn handle_cancel_incoming_call<R: Runtime>(
    state: &ServiceState<R>,
    call_id: String,
) -> Result<(), ServiceError> {
    if let Some(ref mobile) = state.mobile {
        mobile.cancel_incoming_call(&call_id)?;
    }
    Ok(())
}

/// Handle `SetCallAudioRoute` (M-NATIVE-3 / CCF-11, Step 11): set the active
/// call's device audio route. No-op (Ok) when no mobile bridge is attached.
fn handle_set_call_audio_route<R: Runtime>(
    state: &ServiceState<R>,
    call_id: String,
    route: String,
) -> Result<(), ServiceError> {
    if let Some(ref mobile) = state.mobile {
        mobile.set_call_audio_route(&call_id, &route)?;
    }
    Ok(())
}

/// Handle `OpenAppSettings` (M-DIAG-2 / CCF-12, Step 17): open the OS
/// app-settings screen. No-op (Ok) when no mobile bridge is attached.
fn handle_open_app_settings<R: Runtime>(state: &ServiceState<R>) -> Result<(), ServiceError> {
    if let Some(ref mobile) = state.mobile {
        mobile.open_app_settings()?;
    }
    Ok(())
}

/// Handle a `NativeLifecycleEvent` command.
///
/// Recovery-acceptance events (an OsRestart/BootRecovery start the native
/// layer ACCEPTED) are not stops: they fire the policy-gated `bg-recovery`
/// notification and leave the actor's run state alone — the native layer
/// owns that restart. Every other event maps to a stop reason and delegates
/// to [`handle_stop_with_reason`].
fn handle_native_lifecycle_event<R: Runtime>(
    state: &mut ServiceState<R>,
    event: crate::models::NativeLifecycleEvent,
) -> Result<(), ServiceError> {
    if event.is_recovery_acceptance() {
        // D1 recovery fire point: policy gate first, then the sink.
        if state.notifier_policy.on_recovery {
            if let Some(ref sink) = state.notify_sink {
                sink.notify(
                    "bg-recovery",
                    "Sila background service restored",
                    "Background delivery restored.",
                );
            }
        }
        return Ok(());
    }
    handle_stop_with_reason(state, event.to_stop_reason())
}

/// BGS-05 Leg B: whether the manager should replay a `Start` on boot from the
/// persisted desired-state.
///
/// Pure (host-testable). The desktop lifecycle gate + the `handle_start`
/// dispatch live in `manager_loop`'s pre-loop block; mobile uses its own native
/// resubmit at the platform layer. Consent is the Sila app's concern (Leg A) —
/// this fn is `desired_running`-only. NV-MUT (force `false`) REDs only the
/// replay test; NV-MUT (force `true`) REDs the no-replay guards.
fn should_replay_on_boot(ds: &crate::desired_state::DesiredState) -> bool {
    ds.desired_running
}

/// Returns `true` if the given stop reason should clear the desired-state
/// (i.e. set `desired_running = false`). Intentional user/app stops clear
/// desired state so auto-restart won't fight the user's intent. Platform
/// timeouts and errors preserve desired state so recovery can restart.
fn should_clear_desired_state(reason: StopReason) -> bool {
    matches!(
        reason,
        StopReason::UserStop
            | StopReason::AppStop
            | StopReason::NativeNotificationStop
            | StopReason::TaskCompleted
    )
}

/// Returns `true` if the "paused, will resume" timeout notification should
/// fire for the given stop reason: a platform pause (timeout/expiration)
/// whose desired state survives so recovery will restart the service.
/// Reuses [`should_clear_desired_state`] for the recoverable classification
/// rather than re-mapping reasons.
fn should_notify_timeout(reason: StopReason) -> bool {
    !should_clear_desired_state(reason)
        && matches!(
            reason,
            StopReason::PlatformTimeout | StopReason::PlatformExpiration
        )
}

/// Returns `true` if `stop_keepalive` should be called for the given reason.
///
/// Platform pauses and OS-driven exits are skipped because the OS has already
/// paused/killed the background window — tearing down the keepalive would be
/// redundant and, on iOS, would cancel the BGTask schedule recovery still needs:
/// - `PlatformExpiration`: the BGTask expiration handler already fired.
/// - `PlatformTimeout` (M13): the cancel-listener safety timeout / Android FGS
///   timeout — the OS owns the pause; keep the schedule so delivery resumes.
/// - `ProcessExit` (H2): the host process is exiting; the BGTask schedule must
///   survive so a future launch resumes background delivery.
fn should_stop_keepalive(reason: StopReason) -> bool {
    !matches!(
        reason,
        StopReason::PlatformExpiration | StopReason::PlatformTimeout | StopReason::ProcessExit
    )
}

/// Returns `true` if a stop should trigger a reconcile re-submit of native
/// scheduling (M13). A cancel-listener `PlatformTimeout` pauses the service while
/// `desired_running` survives; without re-submitting the BGTask schedule the
/// background delivery would silently end. Other preserve-desired reasons keep
/// their existing schedule (nothing was torn down), so they need no re-submit.
fn should_reconcile_resubmit(reason: StopReason) -> bool {
    matches!(reason, StopReason::PlatformTimeout)
}

// ─── Desired-State Helpers ──────────────────────────────────────────────

/// Save desired-state to the backend (if configured).
///
/// On `desired=true`: saves `desired_running=true` with config and timestamp.
/// On `desired=false`: saves `desired_running=false` and clears recovery fields.
fn save_desired_running<R: Runtime>(
    state: &ServiceState<R>,
    desired: bool,
    config: Option<&StartConfig>,
) {
    let Some(ref backend) = state.desired_state else {
        return;
    };

    let mut ds = backend.load().unwrap_or_default();
    ds.desired_running = desired;
    if desired {
        ds.last_start_config = config.map(|c| serde_json::to_value(c).unwrap_or_default());
        ds.last_start_epoch_ms = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        );
    } else {
        ds.last_start_config = None;
        ds.last_start_epoch_ms = None;
        ds.recovery_pending = false;
        ds.recovery_reason = None;
        ds.restart_attempt = 0;
    }
    if let Err(e) = backend.save(&ds) {
        log::warn!("failed to save desired state: {e}");
    }
}

/// Mirror the Rust-authoritative desired state into native persistence (H4/D1).
///
/// Called by the intent-only recovery commands (`SetDesiredRunning`,
/// `EnableAutoRestart`, `DisableAutoRestart`) so that on iOS they update
/// `UserDefaults` + BGTask scheduling instead of silently no-op'ing. The actual
/// start/stop paths are NOT mirrored here — they already reach native via
/// `start_keepalive`/`stop_keepalive`, so mirroring there would re-schedule
/// redundantly. No-op when no mobile bridge is present (desktop).
fn mirror_desired_to_native<R: Runtime>(
    state: &ServiceState<R>,
    desired: bool,
    config: Option<&StartConfig>,
) {
    let Some(ref mobile) = state.mobile else {
        return;
    };
    let config_value = config.map(|c| serde_json::to_value(c).unwrap_or_default());
    if let Err(e) = mobile.mirror_desired_state(desired, config_value.as_ref()) {
        log::warn!("failed to mirror desired state to native: {e}");
    }
}

/// Handle a `SetDesiredRunning` command.
///
/// Persists the desired running state WITHOUT affecting the actual running state.
/// This is used by `enableAutoRestart()` / `disableAutoRestart()` to set intent
/// for recovery without starting/stopping the service.
fn handle_set_desired_running<R: Runtime>(
    state: &mut ServiceState<R>,
    desired: bool,
    config: Option<StartConfig>,
) -> Result<(), ServiceError> {
    save_desired_running(state, desired, config.as_ref());
    mirror_desired_to_native(state, desired, config.as_ref());
    Ok(())
}

/// Handle an `EnableAutoRestart` command.
///
/// Persists `desired_running=true` with the optional config WITHOUT starting
/// the service. Used to set recovery intent for future restart/reboot.
fn handle_enable_auto_restart<R: Runtime>(
    state: &mut ServiceState<R>,
    config: Option<StartConfig>,
) -> Result<(), ServiceError> {
    save_desired_running(state, true, config.as_ref());
    mirror_desired_to_native(state, true, config.as_ref());
    Ok(())
}

/// Handle a `DisableAutoRestart` command.
///
/// Persists `desired_running=false` with cleared recovery fields WITHOUT
/// stopping the service.
fn handle_disable_auto_restart<R: Runtime>(
    state: &mut ServiceState<R>,
) -> Result<(), ServiceError> {
    save_desired_running(state, false, None);
    mirror_desired_to_native(state, false, None);
    Ok(())
}

/// Handle a `GetDesiredState` command.
///
/// Returns the persisted desired state, or `None` if no backend is configured.
fn handle_get_desired_state<R: Runtime>(
    state: &ServiceState<R>,
) -> Option<crate::desired_state::DesiredState> {
    state
        .desired_state
        .as_ref()
        .and_then(|backend| backend.load().ok())
}

/// Reconcile the actor's running-state belief against the native authority.
///
/// Native `LifecycleService.isRunning` is the single source of truth for
/// service running-state (R-W1.4). When the actor's in-memory belief diverges
/// from the native report, the actor **converges to native** and **logs** the
/// divergence — never silently (NFR-1 / NFR-5). This closes the split-brain
/// window where a stop/timeout with the UI closed could otherwise leave the
/// actor stuck believing the service still runs (harness Scenario 8).
///
/// Returns the reconciled running-state. No-op — returns the actor's current
/// belief unchanged — when no native authority is reachable (desktop, iOS, or a
/// bridge query failure): there is nothing more authoritative to defer to.
fn reconcile_running_with_native<R: Runtime>(state: &ServiceState<R>) -> bool {
    let rust_running = state.is_running.load(Ordering::Acquire);

    // Query the native authority. On desktop / bridge failure there is no
    // authority to reconcile against, so keep the actor's belief unchanged.
    let Some(authority) = state
        .mobile
        .as_ref()
        .and_then(|m| m.query_native_state().ok().flatten())
    else {
        return rust_running;
    };

    // iOS has no long-lived native service whose running bit can override the
    // actor: during a BGTask the Rust actor *is* the runtime authority and the
    // cancel-listener owns the stop. Keep the actor's belief unchanged (L4: no
    // Android round-trip on iOS).
    let native = match authority {
        NativeAuthority::Android(ns) => ns,
        NativeAuthority::Ios(_) => return rust_running,
    };

    if native.native_running == rust_running {
        return rust_running;
    }

    // Divergence: native wins. Converge + log, never swallow (NFR-1 / NFR-5).
    log::warn!(
        "running-state diverged from native authority \
         (rust_running={rust_running}, native_running={}, durable_state={}); \
         converging to native",
        native.native_running,
        native.durable_state,
    );
    state
        .is_running
        .store(native.native_running, Ordering::Release);
    *state.lifecycle_state.lock().unwrap() = if native.native_running {
        crate::models::ServiceState::Running
    } else {
        crate::models::ServiceState::Stopped
    };
    native.native_running
}

/// Compose a [`LifecycleStatus`] snapshot from the actor's current state.
///
/// Gathers: service lifecycle state → `LifecycleState`, desired-state fields
/// from the persistence backend, native Android state via the mobile bridge,
/// merge logic (adopt / auto-heal / normal), platform capabilities, and
/// validation issues.
fn build_lifecycle_status<R: Runtime>(
    state: &ServiceState<R>,
    desktop_mode: Option<&str>,
) -> LifecycleStatus {
    let last_error = state.last_error.lock().unwrap().clone();

    // Load desired-state fields.
    let desired = state.desired_state.as_ref().and_then(|b| b.load().ok());

    let desired_running = desired.as_ref().is_some_and(|d| d.desired_running);
    let recovery_enabled = desired_running;
    let recovery_reason = desired.as_ref().and_then(|d| d.recovery_reason.clone());
    let last_start_config = desired
        .as_ref()
        .and_then(|d| d.last_start_config.clone())
        .and_then(|v| serde_json::from_value(v).ok());
    let mut last_platform_state = desired.as_ref().and_then(|d| d.last_native_state.clone());
    let mut last_platform_error = desired.as_ref().and_then(|d| d.last_platform_error.clone());

    // Query the platform-tagged native authority when the mobile bridge is
    // available. On failure, fall back to Rust-only state (no native fields
    // populated). The Android merge logic below operates on `native_state`
    // (Android only); the iOS snapshot is handled by a dedicated branch so the
    // status path never round-trips to `get_android_service_state` on iOS (L4).
    let authority = state
        .mobile
        .as_ref()
        .and_then(|m| m.query_native_state().ok().flatten());
    let native_state = match &authority {
        Some(NativeAuthority::Android(ns)) => Some(ns.clone()),
        _ => None,
    };
    let ios_native = match &authority {
        Some(NativeAuthority::Ios(s)) => Some(s.clone()),
        _ => None,
    };

    let (mut native_running, mut native_foreground) = match &native_state {
        Some(ns) => (Some(ns.native_running), Some(ns.native_foreground)),
        None => (None, None),
    };

    // ── Merge rules ─────────────────────────────────────────────────────
    // Compare native Android state with Rust actor state.
    // Authority: native for service state, Rust for Core state.
    let rust_running = state.is_running.load(Ordering::Acquire);

    let (adopted, degraded, degraded_reason, should_emit) = match &native_state {
        Some(ns) if ns.native_running && !rust_running => {
            // Rule 1: Native running, Rust idle → adopt (bookkeeping only).
            state.is_running.store(true, Ordering::Release);
            *state.lifecycle_state.lock().unwrap() = crate::models::ServiceState::Running;
            (Some(true), Some(false), None, false)
        }
        Some(ns) if !ns.native_running && rust_running => {
            // Rule 2: Native stopped, Rust running → auto-heal to idle.
            // Native is the authority (R-W1.4): converge + log, never silently
            // swallow the divergence (NFR-1 / NFR-5).
            log::warn!(
                "running-state diverged from native authority \
                 (rust_running=true, native_running=false, durable_state={}); \
                 converging to native (auto-heal)",
                ns.durable_state,
            );
            state.is_running.store(false, Ordering::Release);
            *state.lifecycle_state.lock().unwrap() = crate::models::ServiceState::Stopped;
            (
                None,
                Some(true),
                Some("native_stopped_rust_running".into()),
                true,
            )
        }
        Some(ns) if ns.native_running && rust_running => {
            // Rule 3: Both running → normal.
            (Some(false), Some(false), None, false)
        }
        Some(_ns) => {
            // Rule 4: Both idle → normal.
            (Some(false), Some(false), None, false)
        }
        None => {
            // No native state (non-Android or bridge failure).
            (None, None, None, false)
        }
    };

    // Rule 5: Timeout detection — native DurableState says timeout.
    // Detects timeout regardless of Rust actor state so that stale DurableState
    // from a previous session (app relaunch after JS-less timeout) surfaces.
    let (mut degraded, mut degraded_reason) = if degraded == Some(true) {
        (degraded, degraded_reason)
    } else if let Some(ns) = &native_state {
        if ns.durable_state == "timeout" {
            let reason = if rust_running {
                "native_timeout"
            } else {
                "stale_timeout"
            };
            (Some(true), Some(reason.into()))
        } else {
            (degraded, degraded_reason)
        }
    } else {
        (degraded, degraded_reason)
    };

    // Rule 6: Surface recovery_pending from native state.
    let recovery_pending = desired.as_ref().is_some_and(|d| d.recovery_pending)
        || native_state.as_ref().is_some_and(|ns| ns.recovery_pending);

    // ── iOS native authority (H6) ─────────────────────────────────────────
    // iOS has no persistent foreground service: surface the *real* BGTask /
    // scheduling situation from the native snapshot so the status reflects
    // native facts (a task executing, a pending task waiting, a scheduling
    // error, an exhausted budget) rather than the actor's possibly-stale
    // in-memory lifecycle state. A scheduling error / out-of-budget is degraded
    // and is never swallowed silently (NFR-1 / NFR-5).
    if let Some(ios) = &ios_native {
        // A BGTask is not a foreground service; report whether one is currently
        // executing as the "native running" bit (honest `false` when waiting).
        native_running = Some(ios.active_task_kind.is_some());
        native_foreground = Some(false);

        let phase = if ios.active_task_kind.is_some() {
            "running"
        } else if ios.pending_task.is_some() {
            "pendingBgTask"
        } else if ios.desired_running {
            "waitingForBgTask"
        } else {
            "stopped"
        };
        last_platform_state = Some(phase.to_string());

        // "last-failed?" is split per task type (M7); surface whichever failed
        // (refresh preferred) as the degraded scheduling error.
        let schedule_error = ios
            .last_refresh_error
            .as_ref()
            .or(ios.last_processing_error.as_ref());
        if let Some(err) = schedule_error {
            last_platform_error = Some(err.clone());
            degraded = Some(true);
            degraded_reason = Some("ios_scheduling_error".to_string());
        } else if ios.desired_running && !ios.in_budget {
            degraded = Some(true);
            degraded_reason = Some("ios_out_of_budget".to_string());
        } else {
            degraded = Some(false);
            degraded_reason = None;
        }
    }

    // H9: an advisory scheduling failure recorded at start time surfaces as a
    // distinct "scheduling degraded / foreground-only" status — the Core runs in
    // the foreground but background scheduling is unavailable. A more specific
    // native iOS health problem (already `degraded == Some(true)` above) wins.
    if degraded != Some(true) {
        if let Some(reason) = state.scheduling_degraded.lock().unwrap().as_ref() {
            last_platform_error = Some(reason.clone());
            degraded = Some(true);
            degraded_reason = Some("scheduling_degraded_foreground_only".to_string());
        }
    }

    // Emit degraded event on mismatch, stale timeout, or an iOS health problem.
    if should_emit
        || (degraded == Some(true)
            && matches!(
                degraded_reason.as_deref(),
                Some("native_timeout")
                    | Some("stale_timeout")
                    | Some("ios_scheduling_error")
                    | Some("ios_out_of_budget")
                    | Some("scheduling_degraded_foreground_only")
            ))
    {
        if let Some(ref app) = state.app {
            let _ = app.emit(
                "background-service:state-degraded",
                serde_json::json!({
                    "degraded": true,
                    "reason": degraded_reason,
                    "native_running": native_running,
                    "rust_running": rust_running,
                }),
            );
        }
    }

    // Read lifecycle state AFTER merge (merge may have updated it).
    let mut lifecycle_state: LifecycleState = (*state.lifecycle_state.lock().unwrap()).into();

    // Override lifecycle state for native setup_idle/locked_idle durable states.
    // These are healthy states where the service is running but waiting for
    // user action (account setup or unlock). They are NOT errors.
    if let Some(ns) = &native_state {
        match ns.durable_state.as_str() {
            "setup_idle" => lifecycle_state = LifecycleState::SetupIdle,
            "locked_idle" => lifecycle_state = LifecycleState::LockedIdle,
            _ => {}
        }
    }

    // iOS (H6): don't report a stale "Running" lifecycle when the native
    // snapshot shows no BGTask executing and the service is not desired — the
    // actor's in-memory state may be stale after a JS-less expiration.
    if let Some(ios) = &ios_native {
        if ios.active_task_kind.is_none() && !ios.desired_running {
            lifecycle_state = LifecycleState::Stopped;
        }
    }

    // Surface data_dir from native state for path validation.
    let data_dir = native_state.as_ref().map(|ns| ns.data_dir.clone());

    let (platform, _) = crate::capabilities::CapabilityProvider::detect_platform(desktop_mode);
    let capabilities = crate::capabilities::CapabilityProvider::capabilities(
        platform,
        state.lifecycle_mode,
        false,
    );
    let report = crate::validator::SetupValidator::validate(platform);
    let mut issues: Vec<ValidationIssue> = report
        .errors
        .into_iter()
        .map(|i| ValidationIssue {
            severity: crate::models::Severity::Error,
            code: i.code,
            message: i.message,
            fix: i.fix,
            platform,
        })
        .collect();
    issues.extend(report.warnings.into_iter().map(|i| ValidationIssue {
        severity: crate::models::Severity::Warning,
        code: i.code,
        message: i.message,
        fix: i.fix,
        platform,
    }));

    LifecycleStatus {
        state: lifecycle_state,
        desired_running,
        recovery_enabled,
        recovery_pending,
        recovery_reason,
        last_start_config,
        last_platform_state,
        last_platform_error,
        last_error,
        platform,
        capabilities,
        issues,
        native_running,
        native_foreground,
        adopted,
        degraded,
        degraded_reason,
        data_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desired_state::DesiredState;
    use crate::models::{NativeLifecycleEvent, NativeState};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicI8, AtomicU8, AtomicUsize};
    use tauri::Listener;

    // ── Mock mobile for keepalive testing ─────────────────────────────

    /// Mock mobile that records start/stop_keepalive calls.
    struct MockMobile {
        start_called: AtomicUsize,
        stop_called: AtomicUsize,
        start_fail: bool,
        last_label: std::sync::Mutex<Option<String>>,
        last_fst: std::sync::Mutex<Option<String>>,
        last_timeout_secs: std::sync::Mutex<Option<f64>>,
        last_processing_timeout_secs: std::sync::Mutex<Option<f64>>,
        last_earliest_refresh_begin_minutes: std::sync::Mutex<Option<f64>>,
        last_earliest_processing_begin_minutes: std::sync::Mutex<Option<f64>>,
        last_requires_external_power: std::sync::Mutex<Option<bool>>,
        last_requires_network_connectivity: std::sync::Mutex<Option<bool>>,
        last_processing_ceiling_multiplier: std::sync::Mutex<Option<f64>>,
        /// Records every `mirror_desired_state` call as `(desired, config)`
        /// — the H4 desired-state mirror seam.
        mirror_calls: std::sync::Mutex<Vec<(bool, Option<serde_json::Value>)>>,
        /// When true, `scheduling_is_advisory()` returns true — models the iOS
        /// BGTask scheduler whose failure is a non-fatal degraded warning (H9).
        advisory_scheduling: bool,
        /// When true, `enforces_foreground_service_type()` returns true — models
        /// Android (FGS types are OS-enforced). When false, models iOS/desktop
        /// (no FGS-type concept) — the M5/M6 gate seam.
        enforces_fst: bool,
        /// Records every `update_keepalive_type` (native `updateForegroundServiceType`)
        /// call — proves zero native swaps fire on the iOS-like path (M5).
        update_type_calls: std::sync::Mutex<Vec<String>>,
    }

    impl MockMobile {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                start_called: AtomicUsize::new(0),
                stop_called: AtomicUsize::new(0),
                start_fail: false,
                last_label: std::sync::Mutex::new(None),
                last_fst: std::sync::Mutex::new(None),
                last_timeout_secs: std::sync::Mutex::new(None),
                last_processing_timeout_secs: std::sync::Mutex::new(None),
                last_earliest_refresh_begin_minutes: std::sync::Mutex::new(None),
                last_earliest_processing_begin_minutes: std::sync::Mutex::new(None),
                last_requires_external_power: std::sync::Mutex::new(None),
                last_requires_network_connectivity: std::sync::Mutex::new(None),
                last_processing_ceiling_multiplier: std::sync::Mutex::new(None),
                mirror_calls: std::sync::Mutex::new(Vec::new()),
                advisory_scheduling: false,
                enforces_fst: false,
                update_type_calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn new_failing() -> Arc<Self> {
            Arc::new(Self {
                start_called: AtomicUsize::new(0),
                stop_called: AtomicUsize::new(0),
                start_fail: true,
                last_label: std::sync::Mutex::new(None),
                last_fst: std::sync::Mutex::new(None),
                last_timeout_secs: std::sync::Mutex::new(None),
                last_processing_timeout_secs: std::sync::Mutex::new(None),
                last_earliest_refresh_begin_minutes: std::sync::Mutex::new(None),
                last_earliest_processing_begin_minutes: std::sync::Mutex::new(None),
                last_requires_external_power: std::sync::Mutex::new(None),
                last_requires_network_connectivity: std::sync::Mutex::new(None),
                last_processing_ceiling_multiplier: std::sync::Mutex::new(None),
                mirror_calls: std::sync::Mutex::new(Vec::new()),
                advisory_scheduling: false,
                enforces_fst: false,
                update_type_calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        /// Failing mock whose scheduling is *advisory* (iOS BGTask): the failure
        /// must be treated as a non-fatal degraded warning, not a rollback (H9).
        fn new_failing_advisory() -> Arc<Self> {
            Arc::new(Self {
                start_called: AtomicUsize::new(0),
                stop_called: AtomicUsize::new(0),
                start_fail: true,
                last_label: std::sync::Mutex::new(None),
                last_fst: std::sync::Mutex::new(None),
                last_timeout_secs: std::sync::Mutex::new(None),
                last_processing_timeout_secs: std::sync::Mutex::new(None),
                last_earliest_refresh_begin_minutes: std::sync::Mutex::new(None),
                last_earliest_processing_begin_minutes: std::sync::Mutex::new(None),
                last_requires_external_power: std::sync::Mutex::new(None),
                last_requires_network_connectivity: std::sync::Mutex::new(None),
                last_processing_ceiling_multiplier: std::sync::Mutex::new(None),
                mirror_calls: std::sync::Mutex::new(Vec::new()),
                advisory_scheduling: true,
                enforces_fst: false,
                update_type_calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        /// Mock whose platform enforces foreground-service types — models
        /// Android (`enforces_foreground_service_type()` → true). Used by the
        /// M5/M6 gating tests to prove validation + the native type-swap run on
        /// the Android path. The default [`new`](Self::new) mock models
        /// iOS/desktop (enforces → false).
        fn new_enforcing() -> Arc<Self> {
            Arc::new(Self {
                start_called: AtomicUsize::new(0),
                stop_called: AtomicUsize::new(0),
                start_fail: false,
                last_label: std::sync::Mutex::new(None),
                last_fst: std::sync::Mutex::new(None),
                last_timeout_secs: std::sync::Mutex::new(None),
                last_processing_timeout_secs: std::sync::Mutex::new(None),
                last_earliest_refresh_begin_minutes: std::sync::Mutex::new(None),
                last_earliest_processing_begin_minutes: std::sync::Mutex::new(None),
                last_requires_external_power: std::sync::Mutex::new(None),
                last_requires_network_connectivity: std::sync::Mutex::new(None),
                last_processing_ceiling_multiplier: std::sync::Mutex::new(None),
                mirror_calls: std::sync::Mutex::new(Vec::new()),
                advisory_scheduling: false,
                enforces_fst: true,
                update_type_calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        /// Snapshot of every `update_keepalive_type` argument recorded so far.
        fn update_type_calls(&self) -> Vec<String> {
            self.update_type_calls.lock().unwrap().clone()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn mock_start_keepalive(
        mock: &MockMobile,
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
        mock.start_called.fetch_add(1, Ordering::Release);
        *mock.last_label.lock().unwrap() = Some(label.to_string());
        *mock.last_fst.lock().unwrap() = Some(foreground_service_type.to_string());
        *mock.last_timeout_secs.lock().unwrap() = ios_safety_timeout_secs;
        *mock.last_processing_timeout_secs.lock().unwrap() = ios_processing_safety_timeout_secs;
        *mock.last_earliest_refresh_begin_minutes.lock().unwrap() =
            ios_earliest_refresh_begin_minutes;
        *mock.last_earliest_processing_begin_minutes.lock().unwrap() =
            ios_earliest_processing_begin_minutes;
        *mock.last_requires_external_power.lock().unwrap() = ios_requires_external_power;
        *mock.last_requires_network_connectivity.lock().unwrap() =
            ios_requires_network_connectivity;
        *mock.last_processing_ceiling_multiplier.lock().unwrap() =
            ios_processing_ceiling_multiplier;
        if mock.start_fail {
            return Err(ServiceError::Platform("mock keepalive failure".into()));
        }
        Ok(())
    }

    impl MobileKeepalive for MockMobile {
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
            mock_start_keepalive(
                self,
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
        }

        fn stop_keepalive(&self) -> Result<(), ServiceError> {
            self.stop_called.fetch_add(1, Ordering::Release);
            Ok(())
        }

        fn scheduling_is_advisory(&self) -> bool {
            self.advisory_scheduling
        }

        fn enforces_foreground_service_type(&self) -> bool {
            self.enforces_fst
        }

        fn update_keepalive_type(&self, foreground_service_type: &str) -> Result<(), ServiceError> {
            self.update_type_calls
                .lock()
                .unwrap()
                .push(foreground_service_type.to_string());
            Ok(())
        }

        fn mirror_desired_state(
            &self,
            desired_running: bool,
            last_start_config: Option<&serde_json::Value>,
        ) -> Result<(), ServiceError> {
            self.mirror_calls
                .lock()
                .unwrap()
                .push((desired_running, last_start_config.cloned()));
            Ok(())
        }
    }

    // ── Recording sink for D1 notification testing ────────────────────

    /// Test double for [`NotifySink`] that records every `notify()` call.
    /// Step 3's fire-point tests assert on the recorded calls; in this step
    /// it proves wiring a sink into the actor changes no behavior.
    struct RecordingSink {
        calls: std::sync::Mutex<Vec<(String, String, String)>>,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<(String, String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl NotifySink for RecordingSink {
        fn notify(&self, id: &str, title: &str, body: &str) {
            self.calls
                .lock()
                .unwrap()
                .push((id.into(), title.into(), body.into()));
        }
    }

    /// Service that blocks in run() until cancelled.
    /// Used for lifecycle tests where is_running must remain true.
    struct BlockingService;

    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for BlockingService {
        async fn init(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        async fn run(
            &mut self,
            ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            ctx.shutdown.cancelled().await;
            Ok(())
        }
    }

    /// Create a manager actor with a BlockingService factory.
    fn setup_manager() -> ServiceManagerHandle<tauri::test::MockRuntime> {
        setup_manager_with_backend(None)
    }

    /// Create a manager actor with a desired-state backend.
    fn setup_manager_with_backend(
        backend: Option<Arc<dyn DesiredStateBackend>>,
    ) -> ServiceManagerHandle<tauri::test::MockRuntime> {
        setup_manager_with_backend_and_allowlist(backend, vec!["remoteMessaging".into()], true)
    }

    /// Create a manager actor with a desired-state backend and custom allowlist.
    fn setup_manager_with_backend_and_allowlist(
        backend: Option<Arc<dyn DesiredStateBackend>>,
        android_fg_service_types: Vec<String>,
        android_validate_fg_type: bool,
    ) -> ServiceManagerHandle<tauri::test::MockRuntime> {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        tokio::spawn(manager_loop(
            cmd_rx,
            factory,
            28.0,
            0.0,
            15.0,
            15.0,
            false,
            false,
            4.0,
            backend,
            android_fg_service_types,
            android_validate_fg_type,
            NotifierPolicy::default(),
            None,
            None,
            false,
        ));
        handle
    }

    /// Create a manager actor with a notifier policy and notify sink (D1).
    fn setup_manager_with_sink(
        policy: NotifierPolicy,
        sink: Arc<dyn NotifySink>,
    ) -> ServiceManagerHandle<tauri::test::MockRuntime> {
        setup_manager_with_policy_and_sink(policy, Some(sink))
    }

    /// Create a manager actor with a notifier policy and an optional sink
    /// (D1; `None` models the headless daemon with no notification handle).
    fn setup_manager_with_policy_and_sink(
        policy: NotifierPolicy,
        sink: Option<Arc<dyn NotifySink>>,
    ) -> ServiceManagerHandle<tauri::test::MockRuntime> {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        tokio::spawn(manager_loop(
            cmd_rx,
            factory,
            28.0,
            0.0,
            15.0,
            15.0,
            false,
            false,
            4.0,
            None,
            vec!["remoteMessaging".into()],
            true,
            policy,
            sink,
            None,
            false,
        ));
        handle
    }

    async fn send_start(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        app: AppHandle<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        send_start_with_config(handle, StartConfig::default(), app).await
    }

    async fn send_start_with_config(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        config: StartConfig,
        app: AppHandle<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::Start {
                config,
                reply: tx,
                app,
            })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    async fn send_stop(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::Stop { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    async fn send_is_running(handle: &ServiceManagerHandle<tauri::test::MockRuntime>) -> bool {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::IsRunning { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    // ── AC1: Start from idle succeeds ────────────────────────────────

    #[tokio::test]
    async fn start_from_idle() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        let result = send_start(&handle, app.handle().clone()).await;
        assert!(result.is_ok(), "start should succeed from idle");
        assert!(
            send_is_running(&handle).await,
            "should be running after start"
        );
    }

    // ── AC2: Stop from running succeeds ──────────────────────────────

    #[tokio::test]
    async fn stop_from_running() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();

        let result = send_stop(&handle).await;
        assert!(result.is_ok(), "stop should succeed from running");
        assert!(
            !send_is_running(&handle).await,
            "should not be running after stop"
        );
    }

    // ── AC3: Double start returns AlreadyRunning ────────────────────

    #[tokio::test]
    async fn double_start_returns_already_running() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();

        let result = send_start(&handle, app.handle().clone()).await;
        assert!(
            matches!(result, Err(ServiceError::AlreadyRunning)),
            "second start should return AlreadyRunning"
        );
    }

    // ── AC4: Stop when not running returns NotRunning ────────────────

    #[tokio::test]
    async fn stop_when_not_running_returns_not_running() {
        let handle = setup_manager();

        let result = send_stop(&handle).await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "stop should return NotRunning when idle"
        );
    }

    // ── AC5: Start-stop-restart cycle works ──────────────────────────

    #[tokio::test]
    async fn start_stop_restart_cycle() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        // Start
        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        // Stop
        send_stop(&handle).await.unwrap();
        assert!(!send_is_running(&handle).await);

        // Restart
        let result = send_start(&handle, app.handle().clone()).await;
        assert!(result.is_ok(), "restart should succeed after stop");
        assert!(
            send_is_running(&handle).await,
            "should be running after restart"
        );
    }

    // ── Test services for callback testing ────────────────────────────

    /// Service that completes run() immediately with success.
    struct ImmediateSuccessService;

    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for ImmediateSuccessService {
        async fn init(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        async fn run(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// Service whose run() returns an error immediately.
    struct ImmediateErrorService;

    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for ImmediateErrorService {
        async fn init(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        async fn run(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Err(ServiceError::Runtime("run error".into()))
        }
    }

    /// Service whose init() fails.
    struct FailingInitService;

    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for FailingInitService {
        async fn init(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Err(ServiceError::Init("init error".into()))
        }

        async fn run(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// Create a manager actor with a custom factory.
    fn setup_manager_with_factory(
        factory: ServiceFactory<tauri::test::MockRuntime>,
    ) -> ServiceManagerHandle<tauri::test::MockRuntime> {
        setup_manager_with_factory_and_backend(factory, None)
    }

    /// Create a manager actor with a custom factory and desired-state backend.
    fn setup_manager_with_factory_and_backend(
        factory: ServiceFactory<tauri::test::MockRuntime>,
        backend: Option<Arc<dyn DesiredStateBackend>>,
    ) -> ServiceManagerHandle<tauri::test::MockRuntime> {
        setup_manager_with_factory_backend_and_boot_app(factory, backend, None, false)
    }

    /// Create a manager actor with a custom factory, desired-state backend, and
    /// an optional boot-replay AppHandle (BGS-05 Leg B), plus the consent flag
    /// that gates the boot Start-replay (BGS-05 re-fix Leg A/Leg B coordination).
    fn setup_manager_with_factory_backend_and_boot_app(
        factory: ServiceFactory<tauri::test::MockRuntime>,
        backend: Option<Arc<dyn DesiredStateBackend>>,
        boot_app: Option<AppHandle<tauri::test::MockRuntime>>,
        consent_allows_auto_unlock: bool,
    ) -> ServiceManagerHandle<tauri::test::MockRuntime> {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        tokio::spawn(manager_loop(
            cmd_rx,
            factory,
            28.0,
            0.0,
            15.0,
            15.0,
            false,
            false,
            4.0,
            backend,
            vec!["remoteMessaging".into()],
            true,
            NotifierPolicy::default(),
            None,
            boot_app,
            consent_allows_auto_unlock,
        ));
        handle
    }

    async fn send_set_on_complete(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        callback: OnCompleteCallback,
    ) {
        handle
            .cmd_tx
            .send(ManagerCommand::SetOnComplete { callback })
            .await
            .unwrap();
    }

    /// Wait for the service to finish (is_running becomes false).
    /// Polls with a short sleep between attempts.
    async fn wait_until_stopped(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        timeout_ms: u64,
    ) {
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < timeout_ms as u128 {
            if !send_is_running(handle).await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("Service did not stop within {timeout_ms}ms");
    }

    // ── AC6 (Step 3): Callback fires on success ──────────────────────

    #[tokio::test]
    async fn callback_fires_on_success() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
        let app = tauri::test::mock_app();

        let called = Arc::new(AtomicI8::new(-1));
        let called_clone = called.clone();
        send_set_on_complete(
            &handle,
            Box::new(move |success| {
                called_clone.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;

        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_stopped(&handle, 1000).await;

        assert_eq!(
            called.load(Ordering::Acquire),
            1,
            "callback should be called with true"
        );
    }

    // ── AC7 (Step 3): Callback fires on error ────────────────────────

    #[tokio::test]
    async fn callback_fires_on_error() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(ImmediateErrorService)));
        let app = tauri::test::mock_app();

        let called = Arc::new(AtomicI8::new(-1));
        let called_clone = called.clone();
        send_set_on_complete(
            &handle,
            Box::new(move |success| {
                called_clone.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;

        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_stopped(&handle, 1000).await;

        assert_eq!(
            called.load(Ordering::Acquire),
            0,
            "callback should be called with false on error"
        );
    }

    // ── AC8 (Step 3): Callback fires on init failure ─────────────────

    #[tokio::test]
    async fn callback_fires_on_init_failure() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(FailingInitService)));
        let app = tauri::test::mock_app();

        let called = Arc::new(AtomicI8::new(-1));
        let called_clone = called.clone();
        send_set_on_complete(
            &handle,
            Box::new(move |success| {
                called_clone.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;

        send_start(&handle, app.handle().clone()).await.unwrap();

        // Init failure: service was never truly running, so token gets cleared quickly.
        // Wait a short time for the spawned task to complete.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            called.load(Ordering::Acquire),
            0,
            "callback should be called with false on init failure"
        );
        assert!(
            !send_is_running(&handle).await,
            "should not be running after init failure"
        );
    }

    // ── AC9 (Step 3): No callback no panic ───────────────────────────

    #[tokio::test]
    async fn no_callback_no_panic() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
        let app = tauri::test::mock_app();

        // Deliberately do NOT call SetOnComplete.
        let result = send_start(&handle, app.handle().clone()).await;
        assert!(result.is_ok(), "start without callback should succeed");

        wait_until_stopped(&handle, 1000).await;
        // If we get here without panicking, the test passes.
    }

    // ── N2: is_running returns false after natural completion ────────

    #[tokio::test]
    async fn is_running_false_after_natural_completion() {
        // Use a service that yields during run() so the is_running check
        // doesn't race with immediate completion.
        struct YieldingService;

        #[async_trait]
        impl BackgroundService<tauri::test::MockRuntime> for YieldingService {
            async fn init(
                &mut self,
                _ctx: &ServiceContext<tauri::test::MockRuntime>,
            ) -> Result<(), ServiceError> {
                Ok(())
            }

            async fn run(
                &mut self,
                _ctx: &ServiceContext<tauri::test::MockRuntime>,
            ) -> Result<(), ServiceError> {
                // Sleep long enough for the caller to observe is_running=true,
                // then complete naturally (no cancellation).
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(())
            }
        }

        let handle = setup_manager_with_factory(Box::new(|| Box::new(YieldingService)));
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(
            send_is_running(&handle).await,
            "should be running immediately after start"
        );

        // Wait for the service to complete naturally (no stop).
        wait_until_stopped(&handle, 2000).await;

        assert!(
            !send_is_running(&handle).await,
            "is_running should be false after natural completion"
        );
    }

    // ── AC10 (Step 3): Generation guard prevents stale cleanup ───────

    #[tokio::test]
    async fn generation_guard_prevents_stale_cleanup() {
        // First start with FailingInit (generation 1) — clears its own token.
        // Second start with a long-running BlockingService (generation 2) —
        // should reach AND stay Running because generation 1's stale cleanup
        // must not steal generation 2's freshly installed token.
        //
        // A long-running service (not ImmediateSuccess) is what makes this
        // assertion both meaningful and deterministic: it distinguishes
        // "gen-1 stole gen-2's token" (is_running would be false) from a
        // service that simply self-completed, and it prevents is_running from
        // flickering true→false out from under the assertion under load.
        let call_count = Arc::new(AtomicU8::new(0));
        let call_count_clone = call_count.clone();

        let handle = setup_manager_with_factory(Box::new(move || {
            let cc = call_count_clone.clone();
            // First call: FailingInit. Second call: BlockingService.
            // Use AtomicU8 to track which invocation this is.
            if cc.fetch_add(1, Ordering::AcqRel) == 0 {
                Box::new(FailingInitService) as Box<dyn BackgroundService<tauri::test::MockRuntime>>
            } else {
                Box::new(BlockingService)
            }
        }));
        let app = tauri::test::mock_app();

        // First start: init fails, token cleared by spawned task. Poll until
        // the generation-1 init failure has been recorded (last_error set)
        // instead of sleeping a fixed interval — this deterministically
        // establishes the stale-cleanup scenario before the second start.
        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_error_recorded(&handle).await;

        // Second start: should succeed — generation guard prevented stale cleanup.
        let result = send_start(&handle, app.handle().clone()).await;
        assert!(
            result.is_ok(),
            "second start should succeed after init failure: {result:?}"
        );
        // BlockingService blocks in run(), so is_running is deterministically
        // true once Running is reached — no flicker race with self-completion.
        wait_until_running(&handle).await;
        assert!(
            send_is_running(&handle).await,
            "should be running after second start"
        );
    }

    // ── AC11 (Step 3): Callback captured at spawn time ───────────────

    #[tokio::test]
    async fn callback_captured_at_spawn_time() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(BlockingService)));
        let app = tauri::test::mock_app();

        // Set callback A, start, then set callback B.
        // When the service completes, A should fire (not B).
        let which = Arc::new(AtomicU8::new(0)); // 0=none, 1=A, 2=B
        let which_clone_a = which.clone();
        let which_clone_b = which.clone();

        send_set_on_complete(
            &handle,
            Box::new(move |_| {
                which_clone_a.store(1, Ordering::Release);
            }),
        )
        .await;

        send_start(&handle, app.handle().clone()).await.unwrap();

        // Service is blocking — set a NEW callback while it runs.
        send_set_on_complete(
            &handle,
            Box::new(move |_| {
                which_clone_b.store(2, Ordering::Release);
            }),
        )
        .await;

        // Stop the service — this triggers cleanup and callback.
        send_stop(&handle).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            which.load(Ordering::Acquire),
            1,
            "callback A should fire, not B"
        );
    }

    // ── Mobile keepalive helpers ──────────────────────────────────────

    async fn send_set_mobile(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        mobile: Arc<dyn MobileKeepalive>,
    ) {
        handle
            .cmd_tx
            .send(ManagerCommand::SetMobile { mobile })
            .await
            .unwrap();
    }

    // ── AC1 (Step 5): start_keepalive called on start ────────────────

    #[tokio::test]
    async fn start_keepalive_called_on_start() {
        let mock = MockMobile::new();
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        assert_eq!(
            mock.start_called.load(Ordering::Acquire),
            1,
            "start_keepalive should be called once"
        );
        assert_eq!(
            mock.last_label.lock().unwrap().as_deref(),
            Some("Service running"),
            "label should be forwarded"
        );
    }

    // ── AC2 (Step 5): start_keepalive failure rollback ───────────────

    #[tokio::test]
    async fn start_keepalive_failure_rollback() {
        let mock = MockMobile::new_failing();
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        let callback_called = Arc::new(AtomicI8::new(-1));
        let cb_clone = callback_called.clone();
        send_set_on_complete(
            &handle,
            Box::new(move |success| {
                cb_clone.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;

        send_set_mobile(&handle, mock.clone()).await;

        let result = send_start(&handle, app.handle().clone()).await;
        assert!(
            matches!(result, Err(ServiceError::Platform(_))),
            "start should return Platform error on keepalive failure: {result:?}"
        );

        // Token should be cleared (not running).
        assert!(
            !send_is_running(&handle).await,
            "token should be rolled back after keepalive failure"
        );

        // Callback should be restored — can be set again.
        let callback_called2 = Arc::new(AtomicI8::new(-1));
        let cb_clone2 = callback_called2.clone();
        send_set_on_complete(
            &handle,
            Box::new(move |success| {
                cb_clone2.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;

        // Without the failing mobile, a start should succeed and callback should work.
        // Use a fresh manager without mobile to test callback restoration.
        let handle2 = setup_manager_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
        let callback_restored = Arc::new(AtomicI8::new(-1));
        let cb_r = callback_restored.clone();
        send_set_on_complete(
            &handle2,
            Box::new(move |success| {
                cb_r.store(if success { 1 } else { 0 }, Ordering::Release);
            }),
        )
        .await;
        send_start(&handle2, app.handle().clone()).await.unwrap();
        wait_until_stopped(&handle2, 1000).await;
        assert_eq!(
            callback_restored.load(Ordering::Acquire),
            1,
            "callback should fire after successful start (proves rollback restored it)"
        );
    }

    // ── AC1/AC2 (Step 11, H9): advisory scheduling failure is non-fatal ──
    //
    // On iOS the BGTask scheduler can be unavailable (Simulator / degraded
    // device). A `start_keepalive` failure there must NOT roll back the
    // in-process Core: the service still starts (`is_running` true) and the
    // status reports a distinct "scheduling degraded / foreground-only"
    // condition — distinguishable from a total start failure (which leaves the
    // service stopped, like the Android path in `start_keepalive_failure_rollback`).
    #[tokio::test]
    async fn advisory_scheduling_failure_starts_core_degraded() {
        let mock = MockMobile::new_failing_advisory();
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        // Listen for the non-fatal degraded warning event.
        let event_received = Arc::new(AtomicBool::new(false));
        let event_received_clone = event_received.clone();
        let _listener = app
            .handle()
            .listen("background-service:state-degraded", move |_event| {
                event_received_clone.store(true, Ordering::Release);
            });

        send_set_mobile(&handle, mock.clone()).await;

        // Start must SUCCEED despite the scheduling failure (no rollback).
        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_running(&handle).await;

        assert_eq!(
            mock.start_called.load(Ordering::Acquire),
            1,
            "start_keepalive should still be attempted once",
        );
        assert!(
            send_is_running(&handle).await,
            "Core must start despite advisory scheduling failure (no rollback)",
        );

        // Status reports the distinct degraded / foreground-only condition.
        let status = send_get_lifecycle_status(&handle).await;
        assert_eq!(
            status.degraded,
            Some(true),
            "advisory scheduling failure must report degraded",
        );
        assert_eq!(
            status.degraded_reason,
            Some("scheduling_degraded_foreground_only".into()),
            "degraded reason must name the foreground-only fallback",
        );

        // A non-fatal scheduling-degraded warning was emitted (not a fatal error).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            event_received.load(Ordering::Acquire),
            "a non-fatal scheduling-degraded warning must be emitted",
        );
    }

    // ── AC3 (Step 5): stop_keepalive called on stop ──────────────────

    #[tokio::test]
    async fn stop_keepalive_called_on_stop() {
        let mock = MockMobile::new();
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            0,
            "stop_keepalive should not be called yet"
        );

        send_stop(&handle).await.unwrap();

        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            1,
            "stop_keepalive should be called once after stop"
        );
    }

    // ── stop_keepalive failure does not propagate ──────────────────────────

    /// Mock mobile where `stop_keepalive` always fails.
    struct MockMobileFailingStop;

    #[allow(clippy::too_many_arguments)]
    impl MobileKeepalive for MockMobileFailingStop {
        fn start_keepalive(
            &self,
            _label: &str,
            _foreground_service_type: &str,
            _ios_safety_timeout_secs: Option<f64>,
            _ios_processing_safety_timeout_secs: Option<f64>,
            _ios_earliest_refresh_begin_minutes: Option<f64>,
            _ios_earliest_processing_begin_minutes: Option<f64>,
            _ios_requires_external_power: Option<bool>,
            _ios_requires_network_connectivity: Option<bool>,
            _ios_processing_ceiling_multiplier: Option<f64>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        fn stop_keepalive(&self) -> Result<(), ServiceError> {
            Err(ServiceError::Platform("mock stop failure".into()))
        }
    }

    #[tokio::test]
    async fn stop_keepalive_failure_does_not_propagate() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, Arc::new(MockMobileFailingStop)).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        let result = send_stop(&handle).await;
        assert!(
            result.is_ok(),
            "stop should succeed even when stop_keepalive fails"
        );

        assert!(
            !send_is_running(&handle).await,
            "service should not be running after stop"
        );
    }

    // ── iOS safety timeout passed to mobile ──────────────────────────────

    #[tokio::test]
    async fn ios_safety_timeout_passed_to_mobile() {
        let mock = MockMobile::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        // Use a custom timeout value (not default 28.0)
        tokio::spawn(manager_loop(
            cmd_rx,
            factory,
            15.0,
            0.0,
            15.0,
            15.0,
            false,
            false,
            4.0,
            None,
            vec!["remoteMessaging".into()],
            true,
            NotifierPolicy::default(),
            None,
            None,
            false,
        ));

        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        // Verify the timeout was passed through to the mock
        let timeout = *mock.last_timeout_secs.lock().unwrap();
        assert_eq!(
            timeout,
            Some(15.0),
            "ios_safety_timeout_secs should be passed to mobile"
        );
    }

    // ── iOS processing timeout passed to mobile ──────────────────────────────

    #[tokio::test]
    async fn ios_processing_timeout_passed_to_mobile() {
        let mock = MockMobile::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        // Use a custom processing timeout value
        tokio::spawn(manager_loop(
            cmd_rx,
            factory,
            28.0,
            60.0,
            15.0,
            15.0,
            false,
            false,
            4.0,
            None,
            vec!["remoteMessaging".into()],
            true,
            NotifierPolicy::default(),
            None,
            None,
            false,
        ));

        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        // Verify the processing timeout was passed through to the mock
        let timeout = *mock.last_processing_timeout_secs.lock().unwrap();
        assert_eq!(
            timeout,
            Some(60.0),
            "ios_processing_safety_timeout_secs should be passed to mobile"
        );
    }

    // ── iOS processing ceiling multiplier passed to mobile (D2) ─────────

    #[tokio::test]
    async fn ios_processing_ceiling_multiplier_default_passed_to_mobile() {
        let mock = MockMobile::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        // Default multiplier (4.0, as the named default fn returns)
        tokio::spawn(manager_loop(
            cmd_rx,
            factory,
            28.0,
            0.0,
            15.0,
            15.0,
            false,
            false,
            4.0,
            None,
            vec!["remoteMessaging".into()],
            true,
            NotifierPolicy::default(),
            None,
            None,
            false,
        ));

        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        let multiplier = *mock.last_processing_ceiling_multiplier.lock().unwrap();
        assert_eq!(
            multiplier,
            Some(4.0),
            "default ios_processing_ceiling_multiplier should be passed to mobile"
        );
    }

    #[tokio::test]
    async fn ios_processing_ceiling_multiplier_override_passed_to_mobile() {
        let mock = MockMobile::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        // Override multiplier (not default 4.0)
        tokio::spawn(manager_loop(
            cmd_rx,
            factory,
            28.0,
            0.0,
            15.0,
            15.0,
            false,
            false,
            6.0,
            None,
            vec!["remoteMessaging".into()],
            true,
            NotifierPolicy::default(),
            None,
            None,
            false,
        ));

        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        let multiplier = *mock.last_processing_ceiling_multiplier.lock().unwrap();
        assert_eq!(
            multiplier,
            Some(6.0),
            "overridden ios_processing_ceiling_multiplier should be passed to mobile"
        );
    }

    // ── Service that captures ServiceContext fields for inspection ──────

    /// Service that captures `service_label` and `foreground_service_type`
    /// from the `ServiceContext` it receives in `init()`.
    /// Only compiled on mobile where those fields exist.
    #[cfg(mobile)]
    struct ContextCapturingService {
        captured_label: Arc<std::sync::Mutex<Option<String>>>,
        captured_fst: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[cfg(mobile)]
    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for ContextCapturingService {
        async fn init(
            &mut self,
            ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            *self.captured_label.lock().unwrap() = Some(ctx.service_label.clone());
            *self.captured_fst.lock().unwrap() = Some(ctx.foreground_service_type.clone());
            Ok(())
        }

        async fn run(
            &mut self,
            ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            ctx.shutdown.cancelled().await;
            Ok(())
        }
    }

    // ── AC (Step 11): ServiceContext fields are populated on mobile ────

    #[cfg(mobile)]
    #[tokio::test]
    async fn service_context_fields_populated_on_mobile() {
        let captured_label: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let captured_fst: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let cl = captured_label.clone();
        let cf = captured_fst.clone();

        let handle = setup_manager_with_factory(Box::new(move || {
            let cl = cl.clone();
            let cf = cf.clone();
            Box::new(ContextCapturingService {
                captured_label: cl,
                captured_fst: cf,
            })
        }));
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "Syncing".into(),
            foreground_service_type: "dataSync".into(),
        };

        send_start_with_config(&handle, config, app.handle().clone())
            .await
            .unwrap();

        // Give the spawned task time to run init() (which captures the values).
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // On mobile, both fields should be populated as Strings
        assert_eq!(
            captured_label.lock().unwrap().as_deref(),
            Some("Syncing"),
            "service_label should be 'Syncing' on mobile"
        );
        assert_eq!(
            captured_fst.lock().unwrap().as_deref(),
            Some("dataSync"),
            "foreground_service_type should be 'dataSync' on mobile"
        );

        send_stop(&handle).await.unwrap();
    }

    // ── S1: handle_start accepts invalid foreground_service_type on desktop ──

    #[tokio::test]
    async fn handle_start_accepts_invalid_foreground_service_type_on_desktop() {
        // On desktop (cfg!(mobile) == false), the foreground_service_type
        // validation is skipped. Use validation disabled to bypass allowlist too.
        let handle = setup_manager_with_backend_and_allowlist(None, vec![], false);
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "test".into(),
            foreground_service_type: "bogusType".into(),
        };

        let result = send_start_with_config(&handle, config, app.handle().clone()).await;
        assert!(
            result.is_ok(),
            "start with invalid fg type should succeed on desktop: {result:?}"
        );
        assert!(
            send_is_running(&handle).await,
            "service should be running after start with invalid type on desktop"
        );

        send_stop(&handle).await.unwrap();
    }

    // ── handle_start accepts all valid foreground_service_types ────────

    #[tokio::test]
    async fn handle_start_accepts_all_valid_foreground_service_types() {
        let all_types: Vec<String> = crate::models::VALID_FOREGROUND_SERVICE_TYPES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        for &valid_type in crate::models::VALID_FOREGROUND_SERVICE_TYPES {
            let handle = setup_manager_with_backend_and_allowlist(None, all_types.clone(), true);
            let app = tauri::test::mock_app();

            let config = StartConfig {
                service_label: "test".into(),
                foreground_service_type: valid_type.into(),
            };

            let result = send_start_with_config(&handle, config, app.handle().clone()).await;
            assert!(
                result.is_ok(),
                "start with valid type '{valid_type}' should succeed: {result:?}"
            );
            assert!(send_is_running(&handle).await);
            // Stop for cleanup
            send_stop(&handle).await.unwrap();
        }
    }

    // ── Allowlist enforcement tests ────────────────────────────────────

    #[tokio::test]
    async fn allowlist_rejected_type_returns_platform_error() {
        let handle =
            setup_manager_with_backend_and_allowlist(None, vec!["remoteMessaging".into()], true);
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "test".into(),
            foreground_service_type: "specialUse".into(),
        };
        let result = send_start_with_config(&handle, config, app.handle().clone()).await;
        assert!(
            matches!(result, Err(ServiceError::Platform(ref msg)) if msg.contains("not allowed")),
            "disallowed type should return Platform error: {result:?}"
        );
        assert!(
            !send_is_running(&handle).await,
            "should not be running after allowlist rejection"
        );
    }

    #[tokio::test]
    async fn allowlist_allowed_type_succeeds() {
        let handle =
            setup_manager_with_backend_and_allowlist(None, vec!["remoteMessaging".into()], true);
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "test".into(),
            foreground_service_type: "remoteMessaging".into(),
        };
        let result = send_start_with_config(&handle, config, app.handle().clone()).await;
        assert!(result.is_ok(), "allowed type should succeed: {result:?}");
        assert!(send_is_running(&handle).await);
        send_stop(&handle).await.unwrap();
    }

    // ── UpdateForegroundServiceType (spec 08 C6, Step 15) ────────────────
    //
    // The phoneCall FGS-type swap is gated by the same plugin-config allowlist
    // as start(). These pin the gate so a call answer cannot promote to a type
    // the app did not declare (tauri.conf.json.androidForegroundServiceTypes).

    #[tokio::test]
    async fn update_foreground_service_type_not_running_returns_not_running() {
        let handle = setup_manager_with_backend_and_allowlist(
            None,
            vec!["remoteMessaging".into(), "phoneCall".into()],
            true,
        );
        // No start → nothing to swap.
        let result = handle
            .update_foreground_service_type("phoneCall".into())
            .await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "update with no running service should be NotRunning: {result:?}"
        );
    }

    #[tokio::test]
    async fn update_foreground_service_type_allowlisted_phonecall_succeeds() {
        let handle = setup_manager_with_backend_and_allowlist(
            None,
            vec!["remoteMessaging".into(), "phoneCall".into()],
            true,
        );
        let app = tauri::test::mock_app();
        send_start_with_config(
            &handle,
            StartConfig {
                service_label: "call".into(),
                foreground_service_type: "remoteMessaging".into(),
            },
            app.handle().clone(),
        )
        .await
        .unwrap();

        let result = handle
            .update_foreground_service_type("phoneCall".into())
            .await;
        assert!(
            result.is_ok(),
            "allowlisted phoneCall update should succeed: {result:?}"
        );
        send_stop(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn update_foreground_service_type_rejected_by_allowlist() {
        // App allowlist does NOT include phoneCall → update rejected even while
        // running. This is exactly why tauri.conf.json must add phoneCall.
        let handle =
            setup_manager_with_backend_and_allowlist(None, vec!["remoteMessaging".into()], true);
        let app = tauri::test::mock_app();
        send_start_with_config(
            &handle,
            StartConfig {
                service_label: "call".into(),
                foreground_service_type: "remoteMessaging".into(),
            },
            app.handle().clone(),
        )
        .await
        .unwrap();

        let result = handle
            .update_foreground_service_type("phoneCall".into())
            .await;
        assert!(
            matches!(result, Err(ServiceError::Platform(ref msg)) if msg.contains("not allowed")),
            "non-allowlisted phoneCall update should be rejected: {result:?}"
        );
        // A rejected update is a no-op on the token: the service keeps running.
        assert!(send_is_running(&handle).await);
        send_stop(&handle).await.unwrap();
    }

    // ── M5/M6: Android-only FGS-type gating (Step 14) ────────────────────
    //
    // The 14-type validation (M6) and the native `updateForegroundServiceType`
    // swap (M5) run only where the OS enforces foreground-service types — i.e.
    // Android (`enforces_foreground_service_type()` → true). iOS has no FGS-type
    // concept, so both are skipped there: calling them only produced
    // missing-native-method error noise. The mock simulates each platform.

    #[tokio::test]
    async fn ios_like_bridge_validates_no_fgs_type_on_start() {
        // iOS-like bridge (enforces → false) + allowlist disabled: a type that
        // is NOT one of the 14 valid Android types still starts cleanly — iOS
        // never runs the Android 14-type validation (M6).
        let handle = setup_manager_with_backend_and_allowlist(None, vec![], false);
        let mock = MockMobile::new(); // enforces_foreground_service_type() → false
        send_set_mobile(&handle, mock).await;
        let app = tauri::test::mock_app();

        let result = send_start_with_config(
            &handle,
            StartConfig {
                service_label: "call".into(),
                foreground_service_type: "iosBackgroundDelivery".into(),
            },
            app.handle().clone(),
        )
        .await;
        assert!(
            result.is_ok(),
            "iOS start with a non-Android FGS type should succeed (M6): {result:?}"
        );
        send_stop(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn android_like_bridge_validates_fgs_type_on_start() {
        // Android-like bridge (enforces → true): an invalid 14-type is rejected
        // on start, exactly as before — the Android path is unchanged (M6).
        let handle = setup_manager_with_backend_and_allowlist(None, vec![], false);
        let mock = MockMobile::new_enforcing(); // enforces → true
        send_set_mobile(&handle, mock).await;
        let app = tauri::test::mock_app();

        let result = send_start_with_config(
            &handle,
            StartConfig {
                service_label: "call".into(),
                foreground_service_type: "notAValidType".into(),
            },
            app.handle().clone(),
        )
        .await;
        assert!(
            matches!(result, Err(ServiceError::Platform(ref m)) if m.contains("invalid foreground_service_type")),
            "Android start with an invalid FGS type should be rejected (M6): {result:?}"
        );
    }

    #[tokio::test]
    async fn ios_like_bridge_emits_zero_update_foreground_service_type() {
        // iOS-like bridge (enforces → false): swapping the call FGS type is a
        // success no-op that fires ZERO native `updateForegroundServiceType`
        // calls — the missing-native-method noise (M5) is gone.
        let handle = setup_manager_with_backend_and_allowlist(None, vec![], false);
        let mock = MockMobile::new(); // enforces → false
        send_set_mobile(&handle, mock.clone()).await;
        let app = tauri::test::mock_app();
        send_start_with_config(
            &handle,
            StartConfig {
                service_label: "call".into(),
                foreground_service_type: "remoteMessaging".into(),
            },
            app.handle().clone(),
        )
        .await
        .unwrap();

        // Call start (→ phoneCall) and call end (→ remoteMessaging).
        let answer = handle
            .update_foreground_service_type("phoneCall".into())
            .await;
        let end = handle
            .update_foreground_service_type("remoteMessaging".into())
            .await;
        assert!(
            answer.is_ok(),
            "iOS FGS swap should be a no-op Ok: {answer:?}"
        );
        assert!(end.is_ok(), "iOS FGS revert should be a no-op Ok: {end:?}");
        assert!(
            mock.update_type_calls().is_empty(),
            "iOS must fire zero updateForegroundServiceType calls (M5), got: {:?}",
            mock.update_type_calls()
        );
        send_stop(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn android_like_bridge_swaps_fgs_type() {
        // Android-like bridge (enforces → true): the running service's type
        // swaps remoteMessaging → phoneCall → remoteMessaging via the native
        // handler — the Android swap behaviour is unchanged (M5).
        let handle = setup_manager_with_backend_and_allowlist(
            None,
            vec!["remoteMessaging".into(), "phoneCall".into()],
            true,
        );
        let mock = MockMobile::new_enforcing(); // enforces → true
        send_set_mobile(&handle, mock.clone()).await;
        let app = tauri::test::mock_app();
        send_start_with_config(
            &handle,
            StartConfig {
                service_label: "call".into(),
                foreground_service_type: "remoteMessaging".into(),
            },
            app.handle().clone(),
        )
        .await
        .unwrap();

        handle
            .update_foreground_service_type("phoneCall".into())
            .await
            .unwrap();
        handle
            .update_foreground_service_type("remoteMessaging".into())
            .await
            .unwrap();
        assert_eq!(
            mock.update_type_calls(),
            vec!["phoneCall".to_string(), "remoteMessaging".to_string()],
            "Android must swap the FGS type via the native handler (M5)"
        );
        send_stop(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn allowlist_empty_type_rejected() {
        let handle = setup_manager_with_backend_and_allowlist(None, vec!["dataSync".into()], true);
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "test".into(),
            foreground_service_type: "".into(),
        };
        let result = send_start_with_config(&handle, config, app.handle().clone()).await;
        assert!(
            matches!(result, Err(ServiceError::Platform(ref msg)) if msg.contains("must not be empty")),
            "empty type should be rejected: {result:?}"
        );
    }

    #[tokio::test]
    async fn allowlist_case_insensitive_match() {
        let handle =
            setup_manager_with_backend_and_allowlist(None, vec!["remoteMessaging".into()], true);
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "test".into(),
            foreground_service_type: "RemoteMessaging".into(),
        };
        let result = send_start_with_config(&handle, config, app.handle().clone()).await;
        assert!(
            result.is_ok(),
            "case-insensitive match should succeed: {result:?}"
        );
        assert!(send_is_running(&handle).await);
        send_stop(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn allowlist_validation_disabled_accepts_any_type() {
        let handle = setup_manager_with_backend_and_allowlist(None, vec!["dataSync".into()], false);
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "test".into(),
            foreground_service_type: "specialUse".into(),
        };
        let result = send_start_with_config(&handle, config, app.handle().clone()).await;
        assert!(
            result.is_ok(),
            "validation disabled should accept any type: {result:?}"
        );
        assert!(send_is_running(&handle).await);
        send_stop(&handle).await.unwrap();
    }

    // ── State transition helpers ────────────────────────────────────────

    async fn send_get_state(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    ) -> ServiceStatus {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::GetState { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    /// Poll the actor until it reports `Running` instead of sleeping a fixed
    /// duration. The service task transitions Initializing→Running on a
    /// separate runtime (`tauri::async_runtime::spawn`), so a fixed sleep
    /// races that transition under load; polling observes the real state and
    /// removes the flaky timing seam.
    async fn wait_until_running(handle: &ServiceManagerHandle<tauri::test::MockRuntime>) {
        for _ in 0..200 {
            if send_get_state(handle).await.state == ServiceLifecycle::Running {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("service did not reach Running within ~1s");
    }

    /// Poll until a service's init/run failure has been recorded (last_error
    /// set). Used to deterministically observe a generation's failure cleanup
    /// without relying on a fixed sleep.
    async fn wait_until_error_recorded(handle: &ServiceManagerHandle<tauri::test::MockRuntime>) {
        for _ in 0..200 {
            if send_get_state(handle).await.last_error.is_some() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("service error was not recorded within ~1s");
    }

    // ── State transition: initial state is Idle ───────────────────────

    #[tokio::test]
    async fn get_state_returns_idle_initially() {
        let handle = setup_manager();
        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Idle);
        assert_eq!(status.last_error, None);
    }

    // ── State transition: Idle → Initializing → Running → Stopped ─────

    #[tokio::test]
    async fn lifecycle_idle_to_running_to_stopped() {
        // Use BlockingService so we can reliably observe the Running state.
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        // Idle initially
        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Idle);

        // Start — transitions to Initializing, then Running after init()
        send_start(&handle, app.handle().clone()).await.unwrap();

        // Poll for Running (BlockingService stays Running) instead of a fixed
        // sleep, which races the actor's Initializing→Running transition under
        // parallel-suite load (mem-1781352466-ae2a).
        wait_until_running(&handle).await;
        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Running);

        // Stop → Stopped
        send_stop(&handle).await.unwrap();
        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Stopped);
        assert_eq!(status.last_error, None);
    }

    // ── State transition: Idle → Initializing → Stopped on init failure ─

    #[tokio::test]
    async fn lifecycle_init_failure_sets_stopped_with_error() {
        let handle = setup_manager_with_factory(Box::new(|| Box::new(FailingInitService)));
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();

        // Poll until the init failure is recorded instead of a fixed sleep,
        // which races the FailingInit cleanup under parallel-suite load
        // (mem-1781352466-ae2a). last_error set ⇒ the actor has reached Stopped.
        wait_until_error_recorded(&handle).await;

        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Stopped);
        assert!(
            status.last_error.is_some(),
            "last_error should be set on init failure"
        );
        assert!(
            status.last_error.unwrap().contains("init error"),
            "error should mention init error"
        );
    }

    // ── State transition: explicit stop sets Stopped, clears last_error ─

    #[tokio::test]
    async fn lifecycle_explicit_stop_sets_stopped_clears_error() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        // Poll for Running instead of a fixed sleep, which races the
        // Initializing→Running transition under parallel-suite load
        // (mem-1781352466-ae2a).
        wait_until_running(&handle).await;

        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Running);

        send_stop(&handle).await.unwrap();

        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Stopped);
        assert_eq!(
            status.last_error, None,
            "explicit stop should clear last_error"
        );
    }

    // ── State transition: restart clears stale last_error ─────────────

    #[tokio::test]
    async fn restart_clears_stale_last_error() {
        // Step 1: start with a service whose init() fails → Stopped + last_error set
        let handle = setup_manager_with_factory(Box::new(|| Box::new(FailingInitService)));
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_error_recorded(&handle).await;

        let status = send_get_state(&handle).await;
        assert_eq!(status.state, ServiceLifecycle::Stopped);
        assert!(
            status.last_error.is_some(),
            "should have error after init failure"
        );

        // Step 2: restart with a succeeding service — last_error must be cleared
        // We can't swap the factory, but we CAN verify the field is cleared
        // by starting again with the same failing service and checking that
        // handle_start resets last_error before the spawn.
        // Instead, use a two-phase factory: first fails, then succeeds.
        let call_count = Arc::new(AtomicUsize::new(0));
        let count_clone = call_count.clone();
        let handle2 = setup_manager_with_factory(Box::new(move || {
            let n = count_clone.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Box::new(FailingInitService)
            } else {
                Box::new(ImmediateSuccessService)
            }
        }));
        let app2 = tauri::test::mock_app();

        // First start: init fails
        send_start(&handle2, app2.handle().clone()).await.unwrap();
        wait_until_error_recorded(&handle2).await;

        let status = send_get_state(&handle2).await;
        assert_eq!(status.state, ServiceLifecycle::Stopped);
        assert!(
            status.last_error.is_some(),
            "first run should set last_error"
        );

        // Second start: succeeds — last_error must be None.
        // handle_start clears last_error synchronously before send_start
        // returns; wait for the ImmediateSuccessService to reach its natural
        // Stopped completion so we observe the final state deterministically.
        send_start(&handle2, app2.handle().clone()).await.unwrap();
        wait_until_stopped(&handle2, 1000).await;

        let status = send_get_state(&handle2).await;
        // After successful init + run completion, state is Stopped (natural completion)
        // but last_error should be cleared by handle_start
        assert_eq!(
            status.last_error, None,
            "last_error must be cleared on restart, not stale from previous failure"
        );
    }

    // ── get_state via ServiceManagerHandle method ─────────────────────

    #[tokio::test]
    async fn get_state_handle_method_returns_idle() {
        let handle = setup_manager();
        let status = handle.get_state().await;
        assert_eq!(status.state, ServiceLifecycle::Idle);
        assert_eq!(status.last_error, None);
    }

    // ── stop_blocking sends Stop command and returns success from running ─

    #[tokio::test]
    async fn stop_blocking_returns_success_from_running() {
        let handle = Arc::new(setup_manager());
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        // Must call stop_blocking from outside the async runtime.
        let h = handle.clone();
        let result = tokio::task::spawn_blocking(move || h.stop_blocking())
            .await
            .expect("spawn_blocking panicked");
        assert!(
            result.is_ok(),
            "stop_blocking should succeed from running: {result:?}"
        );
        assert!(
            !send_is_running(&handle).await,
            "should not be running after stop_blocking"
        );
    }

    // ── stop_blocking returns NotRunning when idle ───────────────────────

    #[tokio::test]
    async fn stop_blocking_returns_not_running_when_idle() {
        let handle = Arc::new(setup_manager());

        let h = handle.clone();
        let result = tokio::task::spawn_blocking(move || h.stop_blocking())
            .await
            .expect("spawn_blocking panicked");
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "stop_blocking should return NotRunning when idle: {result:?}"
        );
    }

    #[tokio::test]
    async fn ios_processing_timeout_zero_passes_as_none() {
        let mock = MockMobile::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = ServiceManagerHandle::new(cmd_tx);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        // Processing timeout = 0.0 (default, no cap)
        tokio::spawn(manager_loop(
            cmd_rx,
            factory,
            28.0,
            0.0,
            15.0,
            15.0,
            false,
            false,
            4.0,
            None,
            vec!["remoteMessaging".into()],
            true,
            NotifierPolicy::default(),
            None,
            None,
            false,
        ));

        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        // Zero timeout should be passed as None
        let timeout = *mock.last_processing_timeout_secs.lock().unwrap();
        assert_eq!(
            timeout, None,
            "ios_processing_safety_timeout_secs of 0.0 should pass None to mobile"
        );
    }

    // ── Desired-state MockBackend ─────────────────────────────────────────

    /// Mock desired-state backend that records all saves in a Mutex<Vec>.
    struct MockDesiredStateBackend {
        saves: std::sync::Mutex<Vec<DesiredState>>,
    }

    impl MockDesiredStateBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                saves: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn last_save(&self) -> Option<DesiredState> {
            self.saves.lock().unwrap().last().cloned()
        }

        #[allow(dead_code)]
        fn save_count(&self) -> usize {
            self.saves.lock().unwrap().len()
        }

        #[allow(dead_code)]
        fn saves(&self) -> std::sync::MutexGuard<'_, Vec<DesiredState>> {
            self.saves.lock().unwrap()
        }
    }

    impl DesiredStateBackend for MockDesiredStateBackend {
        fn load(&self) -> Result<DesiredState, String> {
            Ok(self
                .saves
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default())
        }

        fn save(&self, state: &DesiredState) -> Result<(), String> {
            self.saves.lock().unwrap().push(state.clone());
            Ok(())
        }

        fn clear(&self) -> Result<(), String> {
            self.saves.lock().unwrap().clear();
            Ok(())
        }
    }

    // ── BGS-05 Leg B: boot Start-replay from persisted desired-state ──────

    /// Poll `IsRunning` until true or `timeout_ms` elapses (boot replay fires
    /// when the manager_loop task is first scheduled).
    async fn poll_is_running(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        timeout_ms: u64,
    ) -> bool {
        let start = std::time::Instant::now();
        loop {
            if send_is_running(handle).await {
                return true;
            }
            if start.elapsed().as_millis() >= timeout_ms as u128 {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn bgs05_should_replay_on_boot_decision() {
        // Pure decision fn (host-testable, no actor). `desired_running` is the
        // sole gate; the desktop lifecycle check + `handle_start` dispatch live
        // in `manager_loop`'s pre-loop block. NV-MUT (force false) REDs the
        // replay test; (force true) REDs the no-replay guards.
        let on = DesiredState {
            desired_running: true,
            ..Default::default()
        };
        let off = DesiredState {
            desired_running: false,
            ..Default::default()
        };
        assert!(should_replay_on_boot(&on), "desired_running=true ⇒ replay");
        assert!(
            !should_replay_on_boot(&off),
            "desired_running=false ⇒ no replay"
        );
    }

    #[tokio::test]
    async fn bgs05_replay_starts_on_boot_when_desired() {
        // Seed desired_running=true + a valid last_start_config. With boot_app
        // threaded, manager_loop replays a Start on entry (desktop lifecycle)
        // via handle_start — the SAME path a runtime Start takes. BlockingService
        // keeps is_running true. NV-MUT (should_replay_on_boot ⇒ false) REDs
        // ONLY this leg.
        let backend = MockDesiredStateBackend::new();
        backend
            .save(&DesiredState {
                desired_running: true,
                last_start_config: Some(
                    serde_json::to_value(StartConfig {
                        service_label: "Sila".into(),
                        foreground_service_type: "remoteMessaging".into(),
                    })
                    .unwrap(),
                ),
                ..Default::default()
            })
            .unwrap();
        let app = tauri::test::mock_app();
        let handle = setup_manager_with_factory_backend_and_boot_app(
            Box::new(|| Box::new(BlockingService)),
            Some(backend),
            Some(app.handle().clone()),
            true,
        );
        assert!(
            poll_is_running(&handle, 1000).await,
            "boot replay should start the service when desired_running=true"
        );
        let _ = send_stop(&handle).await;
    }

    #[tokio::test]
    async fn bgs05_no_replay_when_desired_false() {
        // desired_running=false ⇒ should_replay_on_boot is false ⇒ no Start on
        // boot even with boot_app + backend configured.
        let backend = MockDesiredStateBackend::new();
        backend
            .save(&DesiredState {
                desired_running: false,
                ..Default::default()
            })
            .unwrap();
        let app = tauri::test::mock_app();
        let handle = setup_manager_with_factory_backend_and_boot_app(
            Box::new(|| Box::new(BlockingService)),
            Some(backend),
            Some(app.handle().clone()),
            true,
        );
        // Give the loop a moment to (not) replay.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            !send_is_running(&handle).await,
            "no boot replay when desired_running=false"
        );
    }

    #[tokio::test]
    async fn bgs05_no_replay_without_backend() {
        // Regression guard: callers that pass backend=None see NO boot replay
        // (preserves the pre-Step-6 behavior; mirrors Step-5 unchanged-default
        // discipline). boot_app is Some here to prove the backend gate binds.
        let app = tauri::test::mock_app();
        let handle = setup_manager_with_factory_backend_and_boot_app(
            Box::new(|| Box::new(BlockingService)),
            None,
            Some(app.handle().clone()),
            true,
        );
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            !send_is_running(&handle).await,
            "no boot replay without a desired-state backend"
        );
    }

    #[tokio::test]
    async fn bgs05_no_replay_when_consent_allows_auto_unlock_false() {
        // BGS-05 re-fix HEADLINE LEG-B PIN (Critic Blocker 2 — F2 load-bearing
        // wiring pin). The persisted desired-state says `desired_running=true`
        // with a valid `last_start_config`, AND `boot_app` is `Some` — every
        // OTHER replay gate is satisfied — but `consent_allows_auto_unlock=false`
        // ⇒ the replay guard must SHORT-CIRCUIT ⇒ no `handle_start` ⇒ the
        // service never reaches `is_running`. This is the consent half of the
        // Leg A/Leg B coordination: consent OFF suppresses the boot Start-replay
        // regardless of desired-state. NV-MUT (drop the `&&
        // consent_allows_auto_unlock` conjunct from the replay guard, OR pass
        // `true` here) ⇒ the replay fires ⇒ `BlockingService` sets is_running ⇒
        // the `!send_is_running` assert flips ⇒ RED. Pairs with the F3 builder
        // pin (`bgs05_start_headless_core_consent_off_stays_locked_despite_credential`
        // in the Sila crate): F2 gates the replay dispatch, F3 gates the builder
        // itself — together they close every boot-reachable auto-unlock path.
        let backend = MockDesiredStateBackend::new();
        backend
            .save(&DesiredState {
                desired_running: true,
                last_start_config: Some(
                    serde_json::to_value(StartConfig {
                        service_label: "Sila".into(),
                        foreground_service_type: "remoteMessaging".into(),
                    })
                    .unwrap(),
                ),
                ..Default::default()
            })
            .unwrap();
        let app = tauri::test::mock_app();
        let handle = setup_manager_with_factory_backend_and_boot_app(
            Box::new(|| Box::new(BlockingService)),
            Some(backend),
            Some(app.handle().clone()),
            false,
        );
        // Give the loop a moment to (not) replay.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            !send_is_running(&handle).await,
            "consent_allows_auto_unlock=false ⇒ no boot replay even with desired_running=true"
        );
    }

    // ── Desired-state actor integration tests ─────────────────────────────

    async fn send_set_desired_running(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        desired: bool,
        config: Option<StartConfig>,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::SetDesiredRunning {
                desired,
                config,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    #[tokio::test]
    async fn start_saves_desired_running_true() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "Syncing".into(),
            ..Default::default()
        };
        send_start_with_config(&handle, config, app.handle().clone())
            .await
            .unwrap();

        // Give the actor a moment to process the save (it happens after spawn).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let last = backend
            .last_save()
            .expect("should have saved desired state");
        assert!(
            last.desired_running,
            "desired_running should be true after start"
        );
        assert!(
            last.last_start_config.is_some(),
            "last_start_config should be set"
        );
        assert!(
            last.last_start_epoch_ms.is_some(),
            "last_start_epoch_ms should be set"
        );
    }

    #[tokio::test]
    async fn stop_saves_desired_running_false_with_cleared_recovery() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();

        // Simulate some recovery state that should be cleared on stop.
        {
            let mut saves = backend.saves.lock().unwrap();
            let last = saves.last_mut().unwrap();
            last.recovery_pending = true;
            last.recovery_reason = Some("boot".into());
            last.restart_attempt = 3;
        }

        send_stop(&handle).await.unwrap();

        let last = backend.last_save().expect("should have saved on stop");
        assert!(
            !last.desired_running,
            "desired_running should be false after stop"
        );
        assert!(
            last.last_start_config.is_none(),
            "last_start_config should be cleared"
        );
        assert!(
            last.last_start_epoch_ms.is_none(),
            "last_start_epoch_ms should be cleared"
        );
        assert!(!last.recovery_pending, "recovery_pending should be cleared");
        assert_eq!(
            last.recovery_reason, None,
            "recovery_reason should be cleared"
        );
        assert_eq!(last.restart_attempt, 0, "restart_attempt should be cleared");
    }

    #[tokio::test]
    async fn set_desired_running_saves_without_affecting_is_running() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));

        // Not running initially
        assert!(!send_is_running(&handle).await);

        // Set desired_running=true WITHOUT starting
        let config = StartConfig {
            service_label: "AutoRestart".into(),
            ..Default::default()
        };
        send_set_desired_running(&handle, true, Some(config.clone()))
            .await
            .unwrap();

        // Should NOT be running
        assert!(
            !send_is_running(&handle).await,
            "SetDesiredRunning should not affect is_running"
        );

        // But desired state should be saved
        let last = backend.last_save().expect("should have saved");
        assert!(last.desired_running);
        assert!(last.last_start_config.is_some());

        // Now set desired_running=false
        send_set_desired_running(&handle, false, None)
            .await
            .unwrap();

        assert!(!send_is_running(&handle).await);

        let last = backend.last_save().expect("should have saved");
        assert!(!last.desired_running);
    }

    #[tokio::test]
    async fn no_backend_means_no_panic() {
        // No backend — should work fine without panicking.
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        send_stop(&handle).await.unwrap();

        send_set_desired_running(&handle, true, None).await.unwrap();
        // If we got here, no panic occurred.
    }

    #[tokio::test]
    async fn start_config_serialized_in_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend_and_allowlist(
            Some(backend.clone()),
            vec!["specialUse".into()],
            true,
        );
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "CustomLabel".into(),
            foreground_service_type: "specialUse".into(),
        };
        send_start_with_config(&handle, config, app.handle().clone())
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let last = backend.last_save().expect("should have saved");
        let saved_config = last.last_start_config.expect("config should be set");
        assert_eq!(saved_config["serviceLabel"], "CustomLabel");
        assert_eq!(saved_config["foregroundServiceType"], "specialUse");
    }

    // ── GetState population from desired-state backend (Step 4, task 1c5e) ──

    #[tokio::test]
    async fn get_state_returns_desired_running_true_after_start() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let status = send_get_state(&handle).await;
        assert_eq!(
            status.desired_running,
            Some(true),
            "desired_running should be Some(true) after start with backend"
        );
    }

    #[tokio::test]
    async fn get_state_returns_desired_running_false_after_stop() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        send_stop(&handle).await.unwrap();

        let status = send_get_state(&handle).await;
        assert_eq!(
            status.desired_running,
            Some(false),
            "desired_running should be Some(false) after stop with backend"
        );
    }

    #[tokio::test]
    async fn get_state_returns_none_fields_when_no_backend() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let status = send_get_state(&handle).await;
        assert_eq!(status.desired_running, None);
        assert_eq!(status.native_state, None);
        assert_eq!(status.last_start_config, None);
        assert_eq!(status.last_heartbeat_at, None);
        assert_eq!(status.restart_attempt, None);
        assert_eq!(status.recovery_reason, None);
        assert_eq!(status.platform_error, None);
    }

    #[tokio::test]
    async fn get_state_returns_last_start_config_from_backend() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend_and_allowlist(
            Some(backend.clone()),
            vec!["specialUse".into()],
            true,
        );
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "TestService".into(),
            foreground_service_type: "specialUse".into(),
        };
        send_start_with_config(&handle, config, app.handle().clone())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let status = send_get_state(&handle).await;
        let cfg = status
            .last_start_config
            .expect("last_start_config should be populated from backend");
        assert_eq!(cfg.service_label, "TestService");
        assert_eq!(cfg.foreground_service_type, "specialUse");
    }

    #[tokio::test]
    async fn get_state_populates_all_desired_state_fields() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Mutate the backend state to simulate recovery fields being set.
        {
            let mut saves = backend.saves.lock().unwrap();
            let last = saves.last_mut().unwrap();
            last.last_native_state = Some("timeout".into());
            last.last_platform_error = Some("FGS timed out".into());
            last.restart_attempt = 3;
            last.recovery_reason = Some("boot recovery".into());
            last.last_heartbeat_epoch_ms = Some(1700000005000);
        }

        let status = send_get_state(&handle).await;
        assert_eq!(status.desired_running, Some(true));
        assert_eq!(status.native_state, Some(NativeState::Timeout));
        assert_eq!(status.platform_error, Some("FGS timed out".into()));
        assert_eq!(status.restart_attempt, Some(3));
        assert_eq!(status.recovery_reason, Some("boot recovery".into()));
        assert_eq!(status.last_heartbeat_at, Some(1700000005000));
    }

    #[tokio::test]
    async fn get_state_returns_platform_mode() {
        let handle = setup_manager();

        let status = send_get_state(&handle).await;
        // On desktop (Linux test runner), should be DesktopInProcess.
        assert_eq!(
            status.platform_mode,
            Some(LifecycleMode::DesktopInProcess),
            "platform_mode should be populated even without backend"
        );
    }

    // ── Step 13: EnableAutoRestart / DisableAutoRestart / GetDesiredState tests ──

    async fn send_enable_auto_restart(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        config: Option<StartConfig>,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::EnableAutoRestart { config, reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    async fn send_disable_auto_restart(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::DisableAutoRestart { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    async fn send_get_desired_state(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    ) -> Option<DesiredState> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::GetDesiredState { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    #[tokio::test]
    async fn enable_auto_restart_saves_true_without_starting() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));

        assert!(!send_is_running(&handle).await);

        send_enable_auto_restart(&handle, None).await.unwrap();

        // Should NOT start the service
        assert!(
            !send_is_running(&handle).await,
            "enableAutoRestart should not start the service"
        );

        // But desired state should be saved as true
        let ds = backend.last_save().expect("should have saved");
        assert!(ds.desired_running, "desired_running should be true");
    }

    #[tokio::test]
    async fn disable_auto_restart_saves_false_without_stopping() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        // Start the service first
        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        // Disable auto restart
        send_disable_auto_restart(&handle).await.unwrap();

        // Should NOT stop the service
        assert!(
            send_is_running(&handle).await,
            "disableAutoRestart should not stop the service"
        );

        // But desired state should be saved as false
        let ds = backend.last_save().expect("should have saved");
        assert!(!ds.desired_running, "desired_running should be false");
    }

    #[tokio::test]
    async fn enable_auto_restart_with_config_stores_config() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));

        let config = StartConfig {
            service_label: "MyService".into(),
            foreground_service_type: "specialUse".into(),
        };
        send_enable_auto_restart(&handle, Some(config.clone()))
            .await
            .unwrap();

        let ds = backend.last_save().expect("should have saved");
        assert!(ds.desired_running);
        let saved_config = ds.last_start_config.expect("config should be stored");
        assert_eq!(saved_config["serviceLabel"], "MyService");
        assert_eq!(saved_config["foregroundServiceType"], "specialUse");
        assert!(
            ds.last_start_epoch_ms.is_some(),
            "should set last_start_epoch_ms"
        );
    }

    #[tokio::test]
    async fn disable_auto_restart_clears_recovery_fields() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));

        // Enable with some recovery state
        send_enable_auto_restart(&handle, None).await.unwrap();
        {
            let mut saves = backend.saves.lock().unwrap();
            let last = saves.last_mut().unwrap();
            last.recovery_pending = true;
            last.recovery_reason = Some("boot".into());
            last.restart_attempt = 5;
        }

        // Disable should clear recovery
        send_disable_auto_restart(&handle).await.unwrap();

        let ds = backend.last_save().expect("should have saved");
        assert!(!ds.desired_running);
        assert!(!ds.recovery_pending, "recovery_pending should be cleared");
        assert_eq!(
            ds.recovery_reason, None,
            "recovery_reason should be cleared"
        );
        assert_eq!(ds.restart_attempt, 0, "restart_attempt should be cleared");
    }

    // ── Step 5 (H4): recovery commands mirror desired state to native ──────

    #[tokio::test]
    async fn enable_auto_restart_mirrors_desired_state_to_native() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));
        let mock = MockMobile::new();
        send_set_mobile(&handle, mock.clone()).await;

        let config = StartConfig {
            service_label: "Mirror".into(),
            foreground_service_type: "specialUse".into(),
        };
        send_enable_auto_restart(&handle, Some(config.clone()))
            .await
            .unwrap();

        let mirrors = mock.mirror_calls.lock().unwrap();
        assert_eq!(
            mirrors.len(),
            1,
            "enableAutoRestart must mirror exactly once to native (H4), got {mirrors:?}"
        );
        let (desired, cfg) = &mirrors[0];
        assert!(*desired, "mirror should request desired_running=true");
        let cfg = cfg.as_ref().expect("config should be mirrored to native");
        assert_eq!(cfg["serviceLabel"], "Mirror");
        assert_eq!(cfg["foregroundServiceType"], "specialUse");
    }

    #[tokio::test]
    async fn disable_auto_restart_mirrors_false_to_native() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));
        let mock = MockMobile::new();
        send_set_mobile(&handle, mock.clone()).await;

        send_disable_auto_restart(&handle).await.unwrap();

        let mirrors = mock.mirror_calls.lock().unwrap();
        assert_eq!(
            mirrors.len(),
            1,
            "disableAutoRestart must mirror exactly once to native (H4)"
        );
        let (desired, cfg) = &mirrors[0];
        assert!(!*desired, "mirror should request desired_running=false");
        assert!(cfg.is_none(), "disable must not mirror a start config");
    }

    #[tokio::test]
    async fn set_desired_running_mirrors_to_native() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));
        let mock = MockMobile::new();
        send_set_mobile(&handle, mock.clone()).await;

        send_set_desired_running(&handle, true, None).await.unwrap();

        let mirrors = mock.mirror_calls.lock().unwrap();
        assert_eq!(
            mirrors.len(),
            1,
            "setDesiredRunning must mirror exactly once to native (H4)"
        );
        assert!(mirrors[0].0, "mirror should request desired_running=true");
    }

    #[tokio::test]
    async fn enable_auto_restart_without_mobile_does_not_panic() {
        // Desktop path: no mobile bridge — mirror is a no-op, command still
        // saves desired state (regression guard for the H4 mirror seam).
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));

        send_enable_auto_restart(&handle, None).await.unwrap();

        let ds = backend
            .last_save()
            .expect("should still save without mobile");
        assert!(ds.desired_running);
    }

    #[tokio::test]
    async fn get_desired_state_returns_current_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_backend(Some(backend.clone()));

        // Initially returns default
        let ds = send_get_desired_state(&handle).await;
        assert!(ds.is_some());
        assert!(!ds.unwrap().desired_running);

        // After enable, returns updated state
        let config = StartConfig {
            service_label: "Test".into(),
            ..Default::default()
        };
        send_enable_auto_restart(&handle, Some(config))
            .await
            .unwrap();

        let ds = send_get_desired_state(&handle)
            .await
            .expect("should return state");
        assert!(ds.desired_running);
        assert!(ds.last_start_config.is_some());
    }

    #[tokio::test]
    async fn get_desired_state_returns_none_without_backend() {
        let handle = setup_manager();
        let ds = send_get_desired_state(&handle).await;
        assert!(
            ds.is_none(),
            "GetDesiredState should return None without a backend"
        );
    }

    #[tokio::test]
    async fn enable_disable_no_backend_no_panic() {
        let handle = setup_manager();

        // These should succeed (no-op) without a backend
        send_enable_auto_restart(&handle, None).await.unwrap();
        send_disable_auto_restart(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn get_state_stop_clears_start_config_and_recovery() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        let config = StartConfig {
            service_label: "Syncing".into(),
            ..Default::default()
        };
        send_start_with_config(&handle, config, app.handle().clone())
            .await
            .unwrap();
        send_stop(&handle).await.unwrap();

        let status = send_get_state(&handle).await;
        assert_eq!(status.desired_running, Some(false));
        assert_eq!(
            status.last_start_config, None,
            "last_start_config should be None after stop"
        );
        assert_eq!(
            status.restart_attempt, None,
            "restart_attempt should be None after stop"
        );
        assert_eq!(
            status.recovery_reason, None,
            "recovery_reason should be None after stop"
        );
    }

    // ── Step 5 (task 8763): Desktop persistence integration tests ──────────

    use crate::desired_state::FileDesiredStateBackend;
    use std::path::PathBuf;

    fn temp_state_dir() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    fn file_backend(dir: PathBuf) -> Arc<dyn DesiredStateBackend> {
        Arc::new(FileDesiredStateBackend::new(dir))
    }

    #[tokio::test]
    async fn enable_auto_restart_persists_desired_running_true_to_file() {
        let dir = temp_state_dir();
        let backend = file_backend(dir.clone());
        let handle = setup_manager_with_backend(Some(backend));

        send_enable_auto_restart(&handle, None).await.unwrap();

        // Verify the file was written with desired_running=true
        let file_backend = FileDesiredStateBackend::new(dir);
        let state = file_backend.load().unwrap();
        assert!(
            state.desired_running,
            "file should contain desired_running=true after enable_auto_restart"
        );
    }

    #[tokio::test]
    async fn simulated_process_restart_loads_persisted_state() {
        let dir = temp_state_dir();
        let backend = file_backend(dir.clone());
        let config = StartConfig {
            service_label: "PersistentSvc".into(),
            foreground_service_type: "dataSync".into(),
        };

        // Simulate first process: enable auto-restart with config
        let handle1 = setup_manager_with_backend(Some(backend));
        send_enable_auto_restart(&handle1, Some(config.clone()))
            .await
            .unwrap();

        // Drop the first manager (simulates process death)
        drop(handle1);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Simulate second process: fresh manager with same backend dir
        let backend2 = file_backend(dir.clone());
        let handle2 = setup_manager_with_backend(Some(backend2));

        // The fresh manager should be able to load the persisted state
        let ds = send_get_desired_state(&handle2)
            .await
            .expect("should return persisted state");
        assert!(
            ds.desired_running,
            "persisted desired_running should be true after simulated restart"
        );
        let saved_config = ds
            .last_start_config
            .expect("config should be persisted across restart");
        assert_eq!(saved_config["serviceLabel"], "PersistentSvc");
    }

    #[tokio::test]
    async fn disable_auto_restart_clears_file_backed_state() {
        let dir = temp_state_dir();
        let backend = file_backend(dir.clone());
        let handle = setup_manager_with_backend(Some(backend));

        // First enable
        send_enable_auto_restart(&handle, None).await.unwrap();
        let ds = send_get_desired_state(&handle)
            .await
            .expect("should return state");
        assert!(ds.desired_running, "should be true after enable");

        // Now disable
        send_disable_auto_restart(&handle).await.unwrap();

        // Verify file-backed state is now false with cleared fields
        let file_backend = FileDesiredStateBackend::new(dir);
        let state = file_backend.load().unwrap();
        assert!(
            !state.desired_running,
            "file should contain desired_running=false after disable"
        );
        assert!(
            state.last_start_config.is_none(),
            "config should be cleared"
        );
        assert!(
            state.last_start_epoch_ms.is_none(),
            "epoch should be cleared"
        );
        assert!(!state.recovery_pending, "recovery should be cleared");
        assert_eq!(state.restart_attempt, 0, "restart_attempt should be 0");
    }

    #[tokio::test]
    async fn file_backend_get_desired_state_returns_none_without_backend() {
        let handle = setup_manager();

        let ds = send_get_desired_state(&handle).await;
        assert!(
            ds.is_none(),
            "get_desired_state should return None without backend (existing behavior preserved)"
        );
    }

    // ── Step 6 (task d820): StopWithReason command and handler tests ──────────

    async fn send_stop_with_reason(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        reason: StopReason,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::StopWithReason { reason, reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    #[tokio::test]
    async fn stop_with_reason_user_stop_clears_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::UserStop)
            .await
            .unwrap();

        // UserStop should save desired_running=false
        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before + 1,
            "UserStop should save a new desired state"
        );
        let last = saves.last().unwrap();
        assert!(
            !last.desired_running,
            "UserStop should clear desired_running"
        );
        assert!(last.last_start_config.is_none(), "config should be cleared");
    }

    #[tokio::test]
    async fn stop_with_reason_app_stop_clears_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::AppStop)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(saves.len(), saves_before + 1);
        assert!(
            !saves.last().unwrap().desired_running,
            "AppStop should clear desired_running"
        );
    }

    // ── D1 NotifySink seam tests (Step 2: wiring only, no fire points) ──

    #[tokio::test]
    async fn recording_sink_records_notify_calls() {
        let sink = RecordingSink::new();
        sink.notify("bg-timeout", "title", "body");
        assert_eq!(
            sink.calls(),
            vec![("bg-timeout".into(), "title".into(), "body".into())]
        );
    }

    // ── D1 fire-point tests (Step 3) ──────────────────────────────────

    #[tokio::test]
    async fn timeout_stop_with_default_policy_fires_nothing() {
        // Default policy (everything off) must keep stops silent even with
        // a live sink installed.
        let sink = RecordingSink::new();
        let handle = setup_manager_with_sink(NotifierPolicy::default(), sink.clone());
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_running(&handle).await;
        send_stop_with_reason(&handle, StopReason::PlatformTimeout)
            .await
            .unwrap();

        assert!(
            sink.calls().is_empty(),
            "default policy must not notify on PlatformTimeout"
        );
    }

    #[tokio::test]
    async fn timeout_stop_fires_bg_timeout_when_policy_on() {
        let sink = RecordingSink::new();
        let policy = NotifierPolicy {
            on_timeout: true,
            on_recovery: false,
        };
        let handle = setup_manager_with_sink(policy, sink.clone());
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_running(&handle).await;
        send_stop_with_reason(&handle, StopReason::PlatformTimeout)
            .await
            .unwrap();

        let calls = sink.calls();
        assert_eq!(
            calls.len(),
            1,
            "exactly one notification on PlatformTimeout"
        );
        assert_eq!(calls[0].0, "bg-timeout", "stable id so repeats replace");

        // PlatformExpiration is the other platform-pause reason and must use
        // the SAME stable id so notices replace rather than stack.
        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_running(&handle).await;
        send_stop_with_reason(&handle, StopReason::PlatformExpiration)
            .await
            .unwrap();

        let calls = sink.calls();
        assert_eq!(calls.len(), 2, "PlatformExpiration also notifies");
        assert_eq!(calls[1].0, "bg-timeout");
    }

    #[tokio::test]
    async fn user_stop_fires_nothing_even_with_timeout_policy_on() {
        let sink = RecordingSink::new();
        let policy = NotifierPolicy {
            on_timeout: true,
            on_recovery: true,
        };
        let handle = setup_manager_with_sink(policy, sink.clone());
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_running(&handle).await;
        send_stop_with_reason(&handle, StopReason::UserStop)
            .await
            .unwrap();

        assert!(
            sink.calls().is_empty(),
            "intentional user stop must never notify"
        );
    }

    #[tokio::test]
    async fn recovery_acceptance_fires_bg_recovery_not_the_stop_path() {
        let sink = RecordingSink::new();
        let policy = NotifierPolicy {
            on_timeout: false,
            on_recovery: true,
        };
        let handle = setup_manager_with_sink(policy, sink.clone());
        let app = tauri::test::mock_app();

        // A STOP with reason OsRestart is not recovery acceptance and must
        // not notify (Design Critic concern #1).
        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_running(&handle).await;
        send_stop_with_reason(&handle, StopReason::OsRestart)
            .await
            .unwrap();
        assert!(
            sink.calls().is_empty(),
            "stop with reason OsRestart is not recovery acceptance"
        );

        // The ACCEPTANCE event fires exactly one bg-recovery, without any
        // service-running precondition (the native layer owns the restart).
        send_native_event(&handle, NativeLifecycleEvent::AndroidOsRestartAccepted)
            .await
            .unwrap();
        let calls = sink.calls();
        assert_eq!(calls.len(), 1, "exactly one notification on acceptance");
        assert_eq!(calls[0].0, "bg-recovery", "stable id so repeats replace");

        // Boot-recovery acceptance uses the SAME stable id.
        send_native_event(&handle, NativeLifecycleEvent::AndroidBootRecoveryAccepted)
            .await
            .unwrap();
        let calls = sink.calls();
        assert_eq!(calls.len(), 2, "boot-recovery acceptance also notifies");
        assert_eq!(calls[1].0, "bg-recovery");
    }

    #[tokio::test]
    async fn fire_points_with_no_sink_installed_do_not_panic() {
        // All-on policy but no sink (headless daemon): both fire points must
        // be safe no-ops.
        let policy = NotifierPolicy {
            on_timeout: true,
            on_recovery: true,
        };
        let handle = setup_manager_with_policy_and_sink(policy, None);
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_running(&handle).await;
        send_stop_with_reason(&handle, StopReason::PlatformTimeout)
            .await
            .unwrap();
        send_native_event(&handle, NativeLifecycleEvent::AndroidOsRestartAccepted)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn android_derived_policy_suppresses_both_fire_points() {
        // DEC-002: on Android with androidOnTimeout=notifyUser the Kotlin
        // layer already posts native notifications — the derived policy must
        // suppress both plugin-side fire points even with both keys true.
        let config = crate::models::PluginConfig {
            notify_on_timeout: true,
            notify_on_recovery: true,
            android_on_timeout: "notifyUser".into(),
            ..Default::default()
        };
        let policy = NotifierPolicy::derive(&config, true);

        let sink = RecordingSink::new();
        let handle = setup_manager_with_sink(policy, sink.clone());
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_running(&handle).await;
        send_stop_with_reason(&handle, StopReason::PlatformTimeout)
            .await
            .unwrap();
        send_native_event(&handle, NativeLifecycleEvent::AndroidOsRestartAccepted)
            .await
            .unwrap();

        assert!(
            sink.calls().is_empty(),
            "android-derived policy must suppress both fire points (DEC-002)"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_native_notification_stop_clears_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::NativeNotificationStop)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(saves.len(), saves_before + 1);
        assert!(
            !saves.last().unwrap().desired_running,
            "NativeNotificationStop should clear desired_running"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_task_completed_clears_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::TaskCompleted)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(saves.len(), saves_before + 1);
        assert!(
            !saves.last().unwrap().desired_running,
            "TaskCompleted should clear desired_running"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_platform_expiration_preserves_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::PlatformExpiration)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "PlatformExpiration should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_platform_timeout_preserves_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::PlatformTimeout)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "PlatformTimeout should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_error_preserves_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::Error)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "Error should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_not_running_returns_not_running() {
        let handle = setup_manager();

        let result = send_stop_with_reason(&handle, StopReason::UserStop).await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "StopWithReason should return NotRunning when idle"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_cancels_service() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        send_stop_with_reason(&handle, StopReason::UserStop)
            .await
            .unwrap();

        assert!(
            !send_is_running(&handle).await,
            "service should be stopped after StopWithReason"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_stops_mobile_keepalive() {
        let mock = MockMobile::new();
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();

        assert_eq!(mock.stop_called.load(Ordering::Acquire), 0);

        send_stop_with_reason(&handle, StopReason::UserStop)
            .await
            .unwrap();

        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            1,
            "stop_keepalive should be called once after StopWithReason"
        );
    }

    // ── Step 6 (task fee4): handle_stop delegates to handle_stop_with_reason ──

    #[tokio::test]
    async fn stop_delegates_to_stop_with_reason_user_stop_clears_desired() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        // Plain Stop should behave like StopWithReason(UserStop) — clear desired state
        send_stop(&handle).await.unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before + 1,
            "Stop should save desired state (delegates to StopWithReason(UserStop))"
        );
        assert!(
            !saves.last().unwrap().desired_running,
            "Stop should clear desired_running"
        );
    }

    // ── Step 6 (task fee4): ServiceManagerHandle::stop_with_reason ──────────

    #[tokio::test]
    async fn stop_with_reason_handle_method_stops_service() {
        let handle = setup_manager();
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        handle.stop_with_reason(StopReason::UserStop).await.unwrap();

        assert!(
            !send_is_running(&handle).await,
            "service should be stopped after stop_with_reason"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_handle_method_preserves_desired_for_platform_timeout() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        handle
            .stop_with_reason(StopReason::PlatformTimeout)
            .await
            .unwrap();

        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "PlatformTimeout should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_handle_method_returns_not_running_when_idle() {
        let handle = setup_manager();

        let result = handle.stop_with_reason(StopReason::UserStop).await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "stop_with_reason should return NotRunning when idle"
        );
    }

    // ── Step 6 (task fee4): ServiceManagerHandle::stop_blocking_with_reason ──

    #[tokio::test]
    async fn stop_blocking_with_reason_stops_service() {
        let handle = Arc::new(setup_manager());
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        assert!(send_is_running(&handle).await);

        let h = handle.clone();
        let result =
            tokio::task::spawn_blocking(move || h.stop_blocking_with_reason(StopReason::AppStop))
                .await
                .expect("spawn_blocking panicked");

        assert!(
            result.is_ok(),
            "stop_blocking_with_reason should succeed: {result:?}"
        );
        assert!(
            !send_is_running(&handle).await,
            "service should be stopped after stop_blocking_with_reason"
        );
    }

    #[tokio::test]
    async fn stop_blocking_with_reason_returns_not_running_when_idle() {
        let handle = Arc::new(setup_manager());

        let h = handle.clone();
        let result =
            tokio::task::spawn_blocking(move || h.stop_blocking_with_reason(StopReason::UserStop))
                .await
                .expect("spawn_blocking panicked");

        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "stop_blocking_with_reason should return NotRunning when idle: {result:?}"
        );
    }

    // ── Step 6 (task d336): Idempotent stop and PlatformExpiration keepalive ──

    #[tokio::test]
    async fn stop_with_reason_idempotent_second_returns_not_running() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // First stop succeeds
        send_stop_with_reason(&handle, StopReason::UserStop)
            .await
            .unwrap();

        let saves_after_first = backend.saves.lock().unwrap().len();

        // Second stop returns NotRunning with no additional side effects
        let result = send_stop_with_reason(&handle, StopReason::UserStop).await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "second StopWithReason should return NotRunning: {result:?}"
        );

        let saves_after_second = backend.saves.lock().unwrap().len();
        assert_eq!(
            saves_after_first, saves_after_second,
            "second StopWithReason should not produce additional desired-state saves"
        );
    }

    #[tokio::test]
    async fn stop_with_reason_platform_expiration_skips_stop_keepalive() {
        let mock = MockMobile::new();
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            0,
            "stop_keepalive should not be called yet"
        );

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::PlatformExpiration)
            .await
            .unwrap();

        assert!(!send_is_running(&handle).await, "service should be stopped");
        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            0,
            "PlatformExpiration should NOT call stop_keepalive"
        );

        // Desired state should be preserved (not cleared)
        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "PlatformExpiration should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );
    }

    // ── Step 8 (H2/M13): stop-reason matrix per design §5.4 ─────────────────

    /// Every `StopReason` maps to the expected `should_stop_keepalive` /
    /// `should_clear_desired_state` policy per design §5.4. `clear_desired`
    /// gates the Swift `UserDefaults` `desired=false` write, so it doubles as
    /// the "write UD desired=false?" column. `PlatformTimeout` and `ProcessExit`
    /// preserve desired state and skip keepalive teardown (M13/H2).
    #[test]
    fn stop_reason_matrix_matches_design_5_4() {
        // (reason, should_stop_keepalive, should_clear_desired_state)
        let matrix = [
            (StopReason::UserStop, true, true),
            (StopReason::AppStop, true, true),
            (StopReason::NativeNotificationStop, true, true),
            (StopReason::TaskCompleted, true, true),
            (StopReason::OsRestart, true, false),
            (StopReason::BootRecovery, true, false),
            (StopReason::Error, true, false),
            (StopReason::PlatformExpiration, false, false),
            // M13: cancel-listener timeout preserves desired + skips keepalive.
            (StopReason::PlatformTimeout, false, false),
            // H2: OS-driven exit preserves desired + skips keepalive.
            (StopReason::ProcessExit, false, false),
        ];
        for (reason, expect_stop_keepalive, expect_clear_desired) in matrix {
            assert_eq!(
                should_stop_keepalive(reason),
                expect_stop_keepalive,
                "should_stop_keepalive mismatch for {reason:?}"
            );
            assert_eq!(
                should_clear_desired_state(reason),
                expect_clear_desired,
                "should_clear_desired_state mismatch for {reason:?}"
            );
        }
    }

    /// H2: an OS-driven `ProcessExit` (the iOS `RunEvent::Exit` path) stops the
    /// service but preserves desired state and leaves the keepalive / BGTask
    /// schedule intact — it must not masquerade as a `UserStop` that erases
    /// recovery intent. A genuine user stop is covered by the UserStop tests.
    #[tokio::test]
    async fn process_exit_preserves_desired_and_keepalive() {
        let mock = MockMobile::new();
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_stop_with_reason(&handle, StopReason::ProcessExit)
            .await
            .unwrap();

        assert!(!send_is_running(&handle).await, "service should be stopped");

        // H2: keepalive (BGTask schedule) must survive an OS-driven exit.
        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            0,
            "ProcessExit should NOT call stop_keepalive (H2)"
        );

        // Desired state must be preserved so recovery can resume delivery.
        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "ProcessExit should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "ProcessExit must preserve desired_running=true"
        );
    }

    // ── Cancel-listener actor-level integration tests ────────────────────────
    //
    // These tests exercise the full cmd_tx → manager_loop path that
    // run_cancel_listener (in lib.rs) uses to send StopWithReason commands.
    // They verify desired-state and keepalive behaviour with both
    // MockDesiredStateBackend and MockMobile wired into the actor.

    #[tokio::test]
    async fn cancel_listener_platform_timeout_preserves_desired_and_resubmits() {
        let mock = MockMobile::new();
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();
        let mirror_before = mock.mirror_calls.lock().unwrap().len();

        // Simulate what run_cancel_listener sends on timeout
        send_stop_with_reason(&handle, StopReason::PlatformTimeout)
            .await
            .unwrap();

        assert!(!send_is_running(&handle).await, "service should be stopped");

        // M13: PlatformTimeout must NOT tear down keepalive — treat it like
        // PlatformExpiration (the OS already paused the background window).
        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            0,
            "PlatformTimeout should NOT call stop_keepalive (M13)"
        );

        // Desired state should be preserved (no desired=false write).
        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "PlatformTimeout should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );

        // M13 reconcile: desired stays true, so re-submit native scheduling
        // (mirror) so a future BGTask resumes delivery instead of silently dying.
        let mirror = mock.mirror_calls.lock().unwrap();
        assert_eq!(
            mirror.len(),
            mirror_before + 1,
            "PlatformTimeout should re-submit native scheduling exactly once"
        );
        assert!(
            mirror.last().unwrap().0,
            "reconcile must mirror desired_running=true (never false)"
        );
    }

    #[tokio::test]
    async fn cancel_listener_user_stop_clears_desired_and_stops_keepalive() {
        let mock = MockMobile::new();
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // UserStop via plain Stop command (delegates to StopWithReason(UserStop))
        send_stop(&handle).await.unwrap();

        assert!(!send_is_running(&handle).await, "service should be stopped");

        // UserStop should call stop_keepalive
        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            1,
            "UserStop should call stop_keepalive"
        );

        // Desired state should be cleared
        let last = backend
            .last_save()
            .expect("should have saved desired state");
        assert!(
            !last.desired_running,
            "UserStop should clear desired_running to false"
        );
    }

    // ── Step 10 (task 3f1f): NativeLifecycleEvent command and handler tests ──

    async fn send_native_event(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
        event: NativeLifecycleEvent,
    ) -> Result<(), ServiceError> {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::NativeLifecycleEvent { event, reply: tx })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    #[tokio::test]
    async fn native_lifecycle_notification_stop_clears_desired_state() {
        let mock = MockMobile::new();
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_native_event(&handle, NativeLifecycleEvent::AndroidNotificationStop)
            .await
            .unwrap();

        assert!(!send_is_running(&handle).await, "service should be stopped");

        // NativeNotificationStop clears desired state
        let saves = backend.saves.lock().unwrap();
        assert_eq!(saves.len(), saves_before + 1);
        assert!(
            !saves.last().unwrap().desired_running,
            "AndroidNotificationStop should clear desired_running"
        );

        // stop_keepalive should have been called
        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            1,
            "AndroidNotificationStop should call stop_keepalive"
        );
    }

    #[tokio::test]
    async fn native_lifecycle_timeout_preserves_desired_state() {
        let mock = MockMobile::new();
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let saves_before = backend.saves.lock().unwrap().len();

        send_native_event(
            &handle,
            NativeLifecycleEvent::AndroidTimeout {
                fgs_type: Some("dataSync".into()),
            },
        )
        .await
        .unwrap();

        assert!(!send_is_running(&handle).await, "service should be stopped");

        // PlatformTimeout preserves desired state
        let saves = backend.saves.lock().unwrap();
        assert_eq!(
            saves.len(),
            saves_before,
            "AndroidTimeout should not save new desired state"
        );
        assert!(
            saves.last().unwrap().desired_running,
            "desired_running should remain true"
        );

        // M13: AndroidTimeout maps to PlatformTimeout, which now preserves the
        // keepalive (the OS already timed out the FGS window).
        assert_eq!(
            mock.stop_called.load(Ordering::Acquire),
            0,
            "AndroidTimeout (PlatformTimeout) should NOT call stop_keepalive (M13)"
        );
    }

    #[tokio::test]
    async fn native_lifecycle_event_idempotent_when_already_stopped() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );
        let app = tauri::test::mock_app();

        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Stop first
        send_stop(&handle).await.unwrap();
        assert!(!send_is_running(&handle).await);

        let saves_before = backend.saves.lock().unwrap().len();

        // Send native event while already stopped — should be a no-op (NotRunning)
        let result =
            send_native_event(&handle, NativeLifecycleEvent::AndroidNotificationStop).await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "native event while stopped should return NotRunning: {result:?}"
        );

        // No additional desired-state saves
        {
            let saves = backend.saves.lock().unwrap();
            assert_eq!(
                saves.len(),
                saves_before,
                "no additional saves when already stopped"
            );
        }

        // Same for timeout variant
        let result = send_native_event(
            &handle,
            NativeLifecycleEvent::AndroidTimeout { fgs_type: None },
        )
        .await;
        assert!(
            matches!(result, Err(ServiceError::NotRunning)),
            "timeout while stopped should return NotRunning: {result:?}"
        );
    }

    // ── Step 13: GetLifecycleStatus command tests ────────────────────────────

    /// Helper: send GetLifecycleStatus and return the result.
    async fn send_get_lifecycle_status(
        handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    ) -> LifecycleStatus {
        let (reply, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(ManagerCommand::GetLifecycleStatus {
                desktop_mode: None,
                reply,
            })
            .await
            .expect("send GetLifecycleStatus");
        rx.await.expect("receive LifecycleStatus")
    }

    #[tokio::test]
    async fn get_lifecycle_status_returns_idle_initially() {
        let handle = setup_manager();
        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            matches!(status.state, LifecycleState::Idle),
            "expected Idle, got {:?}",
            status.state
        );
        assert!(!status.desired_running);
        assert!(!status.recovery_enabled);
        assert!(!status.recovery_pending);
        assert!(status.last_error.is_none());
        assert!(status.last_start_config.is_none());
    }

    #[tokio::test]
    async fn get_lifecycle_status_returns_running_after_start() {
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();
        send_start(&handle, app.handle().clone()).await.unwrap();
        // Poll for Running instead of a fixed sleep, which races the
        // Initializing→Running transition under parallel-suite load
        // (mem-1781352466-ae2a).
        wait_until_running(&handle).await;

        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            matches!(status.state, LifecycleState::Running),
            "expected Running, got {:?}",
            status.state
        );
    }

    #[tokio::test]
    async fn get_lifecycle_status_reflects_desired_state() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );

        // Enable auto-restart (sets desired_running=true)
        send_enable_auto_restart(&handle, None).await.unwrap();

        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            status.desired_running,
            "expected desired_running=true after enable_auto_restart"
        );
        assert!(
            status.recovery_enabled,
            "expected recovery_enabled=true when desired_running=true"
        );
    }

    #[tokio::test]
    async fn get_lifecycle_status_clears_after_disable_recovery() {
        let backend = MockDesiredStateBackend::new();
        let handle = setup_manager_with_factory_and_backend(
            Box::new(|| Box::new(BlockingService)),
            Some(backend.clone()),
        );

        // Enable then disable
        send_enable_auto_restart(&handle, None).await.unwrap();
        send_disable_auto_restart(&handle).await.unwrap();

        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            !status.desired_running,
            "expected desired_running=false after disable"
        );
        assert!(
            !status.recovery_enabled,
            "expected recovery_enabled=false after disable"
        );
    }

    #[tokio::test]
    async fn get_lifecycle_status_includes_platform_and_capabilities() {
        let handle = setup_manager();
        let status = send_get_lifecycle_status(&handle).await;

        // On the test machine (Linux desktop), platform should be Linux
        #[cfg(target_os = "linux")]
        assert!(
            matches!(status.platform, crate::models::Platform::Linux),
            "expected Linux platform, got {:?}",
            status.platform
        );
        // Capabilities should be populated
        assert!(
            !status.capabilities.limitations.is_empty()
                || !status.capabilities.required_setup.is_empty(),
            "capabilities should have some content"
        );
    }

    #[tokio::test]
    async fn get_lifecycle_status_returns_stopped_after_stop() {
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        send_stop(&handle).await.unwrap();

        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            matches!(status.state, LifecycleState::Stopped),
            "expected Stopped, got {:?}",
            status.state
        );
    }

    // ── Step 3: State merge / degraded detection tests ────────────────────

    /// Mock mobile that returns a configurable [`AndroidServiceState`].
    ///
    /// `start_keepalive` / `stop_keepalive` are no-ops (always succeed).
    /// `get_android_service_state` returns the value set via
    /// [`Self::set_native_state`], or `Ok(None)` if unset.
    struct MockNativeState {
        native_state: std::sync::Mutex<Option<crate::models::AndroidServiceState>>,
        android_calls: AtomicUsize,
    }

    impl MockNativeState {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                native_state: std::sync::Mutex::new(None),
                android_calls: AtomicUsize::new(0),
            })
        }

        fn set_native_state(&self, state: crate::models::AndroidServiceState) {
            *self.native_state.lock().unwrap() = Some(state);
        }

        fn android_call_count(&self) -> usize {
            self.android_calls.load(Ordering::Acquire)
        }
    }

    impl MobileKeepalive for MockNativeState {
        #[allow(clippy::too_many_arguments)]
        fn start_keepalive(
            &self,
            _label: &str,
            _foreground_service_type: &str,
            _ios_safety_timeout_secs: Option<f64>,
            _ios_processing_safety_timeout_secs: Option<f64>,
            _ios_earliest_refresh_begin_minutes: Option<f64>,
            _ios_earliest_processing_begin_minutes: Option<f64>,
            _ios_requires_external_power: Option<bool>,
            _ios_requires_network_connectivity: Option<bool>,
            _ios_processing_ceiling_multiplier: Option<f64>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        fn stop_keepalive(&self) -> Result<(), ServiceError> {
            Ok(())
        }

        fn get_android_service_state(
            &self,
        ) -> Result<Option<crate::models::AndroidServiceState>, ServiceError> {
            self.android_calls.fetch_add(1, Ordering::AcqRel);
            Ok(self.native_state.lock().unwrap().clone())
        }
    }

    /// Helper: create a default `AndroidServiceState` with the given
    /// `native_running` flag. All other fields get sensible defaults.
    fn native_state(running: bool) -> crate::models::AndroidServiceState {
        crate::models::AndroidServiceState {
            native_running: running,
            native_foreground: running,
            desired_running: running,
            durable_state: if running { "running" } else { "stopped" }.into(),
            service_label: None,
            foreground_service_type: None,
            notification_id: None,
            notification_channel_id: None,
            recovery_pending: false,
            recovery_reason: None,
            last_platform_error: None,
            data_dir: "/data".into(),
        }
    }

    /// AC1: Adopt native when running, Rust idle.
    #[tokio::test]
    async fn merge_adopt_native_when_running() {
        let mock = MockNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        // Start → Rust running, app handle stored.
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Stop → Rust becomes stopped.
        send_stop(&handle).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Native reports running (OS restarted service).
        mock.set_native_state(native_state(true));

        let status = send_get_lifecycle_status(&handle).await;
        assert_eq!(status.adopted, Some(true), "should adopt native");
        assert_eq!(status.degraded, Some(false), "adopt is not degraded");
        assert_eq!(status.native_running, Some(true));
        assert!(
            matches!(status.state, LifecycleState::Running),
            "expected Running after adopt, got {:?}",
            status.state,
        );
    }

    /// AC2: Auto-heal when native stopped but Rust running.
    #[tokio::test]
    async fn merge_autoheal_when_native_stopped() {
        let mock = MockNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Native reports stopped while Rust is running.
        mock.set_native_state(native_state(false));

        let status = send_get_lifecycle_status(&handle).await;
        assert_eq!(
            status.degraded,
            Some(true),
            "transient degraded on mismatch"
        );
        assert_eq!(
            status.degraded_reason,
            Some("native_stopped_rust_running".into()),
            "should explain the mismatch"
        );
        // Auto-healed: Rust is now idle/stopped.
        assert!(
            matches!(status.state, LifecycleState::Stopped | LifecycleState::Idle),
            "expected Stopped or Idle after auto-heal, got {:?}",
            status.state,
        );
    }

    /// R-W1.4 / D-SPLITBRAIN: `is_running()` is native-authoritative.
    ///
    /// When the actor believes the service is running but the native authority
    /// (`LifecycleService.isRunning`) reports stopped — e.g. the OS killed or
    /// timed-out the FGS while the UI was closed — a direct `is_running()`
    /// query MUST reconcile to native truth: converge to `false` (closing the
    /// split-brain "stuck running" window the harness Scenario 8 tests) and
    /// converge the lifecycle state to `Stopped`. The divergence is logged on
    /// the reconcile path (NFR-1/NFR-5, no silent swallow).
    #[tokio::test]
    async fn actor_converges_to_native_stopped() {
        let mock = MockNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        // Poll for Running rather than a fixed sleep (mem-1781352466-ae2a).
        wait_until_running(&handle).await;

        // Precondition: with no native report yet, the actor believes it runs.
        assert!(
            send_is_running(&handle).await,
            "precondition: actor should believe it is running after start",
        );

        // Native authority now reports STOPPED (OS killed/timed-out the FGS
        // while the UI was closed) — the split-brain trigger.
        mock.set_native_state(native_state(false));

        // is_running() reconciles to native truth: converges to false, so a
        // caller can never observe a stale "running" after a native stop.
        assert!(
            !send_is_running(&handle).await,
            "is_running() must converge to native-stopped (no stuck 'running')",
        );

        // Full convergence: the lifecycle state also reflects native authority.
        assert_eq!(
            send_get_state(&handle).await.state,
            ServiceLifecycle::Stopped,
            "lifecycle state must converge to Stopped on native-authority reconcile",
        );
    }

    /// AC3: Normal when both agree running.
    #[tokio::test]
    async fn merge_normal_both_running() {
        let mock = MockNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        // Poll for Running instead of a fixed sleep, which races the
        // Initializing→Running transition under parallel-suite load
        // (mem-1781352466-ae2a).
        wait_until_running(&handle).await;

        // Native agrees: running.
        mock.set_native_state(native_state(true));

        let status = send_get_lifecycle_status(&handle).await;
        assert_eq!(status.degraded, Some(false));
        assert_eq!(status.adopted, Some(false));
        assert!(
            matches!(status.state, LifecycleState::Running),
            "expected Running, got {:?}",
            status.state,
        );
    }

    /// AC4: Normal when both agree idle.
    #[tokio::test]
    async fn merge_normal_both_idle() {
        let mock = MockNativeState::new();
        let handle = setup_manager();

        send_set_mobile(&handle, mock.clone()).await;

        // Neither started — Rust is idle, native reports stopped.
        mock.set_native_state(native_state(false));

        let status = send_get_lifecycle_status(&handle).await;
        assert_eq!(status.degraded, Some(false));
        assert!(
            matches!(status.state, LifecycleState::Idle),
            "expected Idle, got {:?}",
            status.state,
        );
    }

    /// AC5: Timeout detection — native timeout DurableState + Rust running.
    #[tokio::test]
    async fn merge_timeout_detection() {
        let mock = MockNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Native reports running but DurableState says "timeout".
        let mut ns = native_state(true);
        ns.durable_state = "timeout".into();
        mock.set_native_state(ns);

        let status = send_get_lifecycle_status(&handle).await;
        assert_eq!(status.degraded, Some(true));
        assert_eq!(
            status.degraded_reason,
            Some("native_timeout".into()),
            "should report timeout degradation"
        );
    }

    /// AC6: Recovery pending from native surfaces in status.
    #[tokio::test]
    async fn merge_recovery_pending_surfaces() {
        let mock = MockNativeState::new();
        let handle = setup_manager();

        send_set_mobile(&handle, mock.clone()).await;

        // Native reports recovery pending.
        let mut ns = native_state(false);
        ns.recovery_pending = true;
        ns.recovery_reason = Some("core_start_failed".into());
        mock.set_native_state(ns);

        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            status.recovery_pending,
            "recovery_pending should be true from native state"
        );
    }

    /// AC7: Degraded event emitted on state mismatch.
    #[tokio::test]
    async fn merge_degraded_event_emitted() {
        let mock = MockNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();

        let event_received = Arc::new(AtomicBool::new(false));
        let event_received_clone = event_received.clone();
        let _listener = app
            .handle()
            .listen("background-service:state-degraded", move |_event| {
                event_received_clone.store(true, Ordering::Release);
            });

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Trigger mismatch: native stopped, Rust running.
        mock.set_native_state(native_state(false));

        let _status = send_get_lifecycle_status(&handle).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            event_received.load(Ordering::Acquire),
            "state-degraded event should be emitted on mismatch"
        );
    }

    /// AC8: Degraded clears when states converge on next query.
    #[tokio::test]
    async fn merge_degraded_clears_on_convergence() {
        let mock = MockNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();

        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // First query: mismatch → degraded.
        mock.set_native_state(native_state(false));
        let status1 = send_get_lifecycle_status(&handle).await;
        assert_eq!(
            status1.degraded,
            Some(true),
            "first query should be degraded"
        );

        // Auto-heal set Rust to stopped. Now both agree stopped.
        // Second query: convergence → degraded clears.
        let status2 = send_get_lifecycle_status(&handle).await;
        assert_eq!(
            status2.degraded,
            Some(false),
            "degraded should clear on convergence"
        );
        assert_eq!(status2.degraded_reason, None);
    }

    // ── Step 10: iOS native reconciliation + Android-gating (H6, L4) ───────

    /// Mock mobile that returns a configurable iOS [`IosNativeState`] via
    /// `query_native_state` and COUNTS `get_android_service_state` calls, so a
    /// test can assert the iOS status / reconcile paths never round-trip to the
    /// Android bridge (L4).
    struct MockIosNativeState {
        ios_state: std::sync::Mutex<Option<crate::models::IosNativeState>>,
        android_calls: AtomicUsize,
    }

    impl MockIosNativeState {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                ios_state: std::sync::Mutex::new(None),
                android_calls: AtomicUsize::new(0),
            })
        }

        fn set_ios_state(&self, state: crate::models::IosNativeState) {
            *self.ios_state.lock().unwrap() = Some(state);
        }

        fn android_call_count(&self) -> usize {
            self.android_calls.load(Ordering::Acquire)
        }
    }

    impl MobileKeepalive for MockIosNativeState {
        #[allow(clippy::too_many_arguments)]
        fn start_keepalive(
            &self,
            _label: &str,
            _foreground_service_type: &str,
            _ios_safety_timeout_secs: Option<f64>,
            _ios_processing_safety_timeout_secs: Option<f64>,
            _ios_earliest_refresh_begin_minutes: Option<f64>,
            _ios_earliest_processing_begin_minutes: Option<f64>,
            _ios_requires_external_power: Option<bool>,
            _ios_requires_network_connectivity: Option<bool>,
            _ios_processing_ceiling_multiplier: Option<f64>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        fn stop_keepalive(&self) -> Result<(), ServiceError> {
            Ok(())
        }

        fn get_android_service_state(
            &self,
        ) -> Result<Option<crate::models::AndroidServiceState>, ServiceError> {
            // L4 tripwire: the iOS status/reconcile path must NEVER reach this.
            self.android_calls.fetch_add(1, Ordering::AcqRel);
            Ok(None)
        }

        fn get_ios_native_state(
            &self,
        ) -> Result<Option<crate::models::IosNativeState>, ServiceError> {
            Ok(self.ios_state.lock().unwrap().clone())
        }

        fn query_native_state(&self) -> Result<Option<NativeAuthority>, ServiceError> {
            // Mirror the real iOS `MobileLifecycle`: tag the snapshot `Ios` and
            // never touch the Android bridge.
            Ok(self.get_ios_native_state()?.map(NativeAuthority::Ios))
        }
    }

    /// Helper: a default iOS native snapshot with the given desired-running
    /// flag, in-budget, no active task / pending / error.
    fn ios_state(desired_running: bool) -> crate::models::IosNativeState {
        crate::models::IosNativeState {
            desired_running,
            refresh_scheduled: false,
            processing_scheduled: false,
            active_task_kind: None,
            pending_task: None,
            last_completed_at: None,
            last_completion_reason: None,
            last_refresh_error: None,
            last_processing_error: None,
            in_budget: true,
        }
    }

    /// H6 / AC1: a native scheduling failure surfaces as degraded + the real
    /// error string — not silently swallowed and not stale actor memory.
    #[tokio::test]
    async fn ios_status_reflects_scheduling_failure() {
        let mock = MockIosNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        send_set_mobile(&handle, mock.clone()).await;

        let mut s = ios_state(true);
        s.last_refresh_error = Some("BGTaskSchedulerErrorDomain code 1".into());
        mock.set_ios_state(s);

        let status = send_get_lifecycle_status(&handle).await;
        assert_eq!(
            status.degraded,
            Some(true),
            "iOS scheduling failure must report degraded",
        );
        assert_eq!(
            status.degraded_reason,
            Some("ios_scheduling_error".into()),
            "degraded reason must name the iOS scheduling error",
        );
        assert_eq!(
            status.last_platform_error,
            Some("BGTaskSchedulerErrorDomain code 1".into()),
            "the real native error string must surface",
        );
        assert_eq!(mock.android_call_count(), 0, "L4: no Android round-trip");
    }

    /// H6 / AC1: with the service desired but no BGTask currently executing,
    /// the status reflects the native "waiting for next BGTask" phase and an
    /// honest `native_running=false`, instead of the actor's stale "running".
    #[tokio::test]
    async fn ios_status_reflects_waiting_for_next_bgtask() {
        let mock = MockIosNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();
        send_set_mobile(&handle, mock.clone()).await;
        // Actor believes it is running (BlockingService keeps run() alive)…
        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_running(&handle).await;

        // …but the native snapshot shows no active BGTask (window expired).
        mock.set_ios_state(ios_state(true));

        let status = send_get_lifecycle_status(&handle).await;
        assert_eq!(
            status.last_platform_state,
            Some("waitingForBgTask".into()),
            "status must reflect the native waiting phase, not stale actor memory",
        );
        assert_eq!(
            status.native_running,
            Some(false),
            "no BGTask is executing natively",
        );
        assert_eq!(status.degraded, Some(false));
        assert_eq!(mock.android_call_count(), 0, "L4: no Android round-trip");
    }

    /// H6 / AC1: a desired-but-out-of-budget snapshot reports degraded.
    #[tokio::test]
    async fn ios_status_out_of_budget_is_degraded() {
        let mock = MockIosNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        send_set_mobile(&handle, mock.clone()).await;

        let mut s = ios_state(true);
        s.in_budget = false;
        mock.set_ios_state(s);

        let status = send_get_lifecycle_status(&handle).await;
        assert_eq!(status.degraded, Some(true));
        assert_eq!(status.degraded_reason, Some("ios_out_of_budget".into()));
    }

    /// L4 / AC2: an iOS status poll must NOT invoke `getAndroidServiceState`.
    #[tokio::test]
    async fn ios_status_poll_does_not_call_get_android_service_state() {
        let mock = MockIosNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        send_set_mobile(&handle, mock.clone()).await;
        mock.set_ios_state(ios_state(true));

        let status = send_get_lifecycle_status(&handle).await;
        // Proof the iOS native path actually ran:
        assert_eq!(status.last_platform_state, Some("waitingForBgTask".into()));
        // …without ever touching the Android bridge.
        assert_eq!(
            mock.android_call_count(),
            0,
            "iOS status poll must not call getAndroidServiceState (L4)",
        );
    }

    /// L4 / AC2: the iOS reconcile path (is_running) must not flip the actor's
    /// belief (iOS has no authoritative native running bit) and must not call
    /// `getAndroidServiceState`.
    #[tokio::test]
    async fn ios_reconcile_keeps_actor_belief_without_android_call() {
        let mock = MockIosNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();
        send_set_mobile(&handle, mock.clone()).await;
        send_start(&handle, app.handle().clone()).await.unwrap();
        wait_until_running(&handle).await;

        mock.set_ios_state(ios_state(true));

        assert!(
            send_is_running(&handle).await,
            "iOS reconcile must keep the actor's running belief",
        );
        assert_eq!(
            mock.android_call_count(),
            0,
            "iOS reconcile must not call getAndroidServiceState (L4)",
        );
    }

    /// AC2 (Android path unchanged): the default `query_native_state` route
    /// still queries `getAndroidServiceState` for the Android/desktop bridge.
    #[tokio::test]
    async fn android_status_poll_still_queries_android_state() {
        let mock = MockNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        send_set_mobile(&handle, mock.clone()).await;
        mock.set_native_state(native_state(true));

        let _ = send_get_lifecycle_status(&handle).await;
        assert!(
            mock.android_call_count() >= 1,
            "Android path must still query getAndroidServiceState",
        );
    }

    // ── Step 11: Core Adoption Tests ──────────────────────────────────────

    /// AC1: Native headless Core start → UI attach → event reception.
    ///
    /// Simulates Android OS sticky-restarting the service. The native layer
    /// starts Core via JNI without Rust knowing. When the UI (re)attaches and
    /// calls `get_lifecycle_status`, the merge logic adopts the native state.
    /// The late subscriber can then receive events from the adopted Core.
    #[tokio::test]
    async fn adoption_native_start_ui_attach_events() {
        let mock = MockNativeState::new();
        let handle =
            setup_manager_with_factory_and_backend(Box::new(|| Box::new(BlockingService)), None);
        let app = tauri::test::mock_app();

        // Register event listener BEFORE adoption (late subscriber).
        let event_received = Arc::new(AtomicBool::new(false));
        let event_received_clone = event_received.clone();
        let _listener = app
            .handle()
            .listen("background-service:state-degraded", move |_event| {
                event_received_clone.store(true, Ordering::Release);
            });

        send_set_mobile(&handle, mock.clone()).await;

        // Start service normally → sets app handle in state.
        send_start(&handle, app.handle().clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Stop service → Rust becomes stopped (simulating app killed by OS).
        send_stop(&handle).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Native reports running (OS sticky restart).
        mock.set_native_state(native_state(true));

        // UI attaches → calls get_lifecycle_status.
        let status = send_get_lifecycle_status(&handle).await;

        // Adoption: native running, Rust idle → adopt.
        assert_eq!(status.adopted, Some(true), "should adopt native state");
        assert_eq!(status.degraded, Some(false), "adoption is not degraded");
        assert!(
            matches!(status.state, LifecycleState::Running),
            "expected Running after adoption, got {:?}",
            status.state,
        );

        // Verify the event system still works after adoption.
        // Trigger a state mismatch to emit the degraded event.
        mock.set_native_state(native_state(false));
        let _ = send_get_lifecycle_status(&handle).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            event_received.load(Ordering::Acquire),
            "late subscriber should receive events after adoption"
        );
    }

    /// AC2: Data directory from native state surfaces in LifecycleStatus.
    ///
    /// When native reports its data directory, it should be visible in the
    /// merged status so the UI can validate path consistency.
    #[tokio::test]
    async fn adoption_data_dir_surfaces_in_status() {
        let mock = MockNativeState::new();
        let handle = setup_manager();

        send_set_mobile(&handle, mock.clone()).await;

        let mut ns = native_state(false);
        ns.data_dir = "/data/app/com.sila".into();
        mock.set_native_state(ns);

        let status = send_get_lifecycle_status(&handle).await;
        assert_eq!(
            status.data_dir,
            Some("/data/app/com.sila".to_string()),
            "data_dir from native should surface in status"
        );
    }

    /// AC3: setup_idle is reported as healthy, not failed.
    ///
    /// When Core reports setup_idle (account setup needed), the service
    /// is running in the background but waiting for user action.
    /// This is a valid healthy state, not an error.
    #[tokio::test]
    async fn adoption_setup_idle_is_healthy() {
        let mock = MockNativeState::new();
        let handle = setup_manager();

        send_set_mobile(&handle, mock.clone()).await;

        // Native reports setup_idle: service running, Core in setup mode.
        let mut ns = native_state(true);
        ns.durable_state = "setup_idle".into();
        mock.set_native_state(ns);

        let status = send_get_lifecycle_status(&handle).await;

        // Must NOT be Error or degraded.
        assert!(
            !matches!(status.state, LifecycleState::Error),
            "setup_idle should not be reported as Error"
        );
        assert_ne!(
            status.degraded,
            Some(true),
            "setup_idle should not be degraded"
        );

        // Must be reported as SetupIdle (not generic Running/Idle).
        assert!(
            matches!(status.state, LifecycleState::SetupIdle),
            "expected SetupIdle, got {:?}",
            status.state,
        );
    }

    /// AC4: locked_idle is reported as healthy, not failed.
    ///
    /// When Core reports locked_idle (account locked, needs unlock), the
    /// service is running in the background but waiting for user action.
    /// This is a valid healthy state, not an error.
    #[tokio::test]
    async fn adoption_locked_idle_is_healthy() {
        let mock = MockNativeState::new();
        let handle = setup_manager();

        send_set_mobile(&handle, mock.clone()).await;

        // Native reports locked_idle: service running, Core in locked mode.
        let mut ns = native_state(true);
        ns.durable_state = "locked_idle".into();
        mock.set_native_state(ns);

        let status = send_get_lifecycle_status(&handle).await;

        // Must NOT be Error or degraded.
        assert!(
            !matches!(status.state, LifecycleState::Error),
            "locked_idle should not be reported as Error"
        );
        assert_ne!(
            status.degraded,
            Some(true),
            "locked_idle should not be degraded"
        );

        // Must be reported as LockedIdle (not generic Running/Idle).
        assert!(
            matches!(status.state, LifecycleState::LockedIdle),
            "expected LockedIdle, got {:?}",
            status.state,
        );
    }

    // ── Step 12: Stale DurableState Safety Checks ──────────────────────────

    /// AC3: Stale timeout DurableState surfaces in status even when Rust is not running.
    ///
    /// Simulates: Android FGS timeout occurred (DurableState="timeout") but the
    /// app relaunched. Rust actor is idle. `get_lifecycle_status` must still
    /// surface the stale timeout so the UI can handle recovery.
    #[tokio::test]
    async fn stale_timeout_surfaces_in_status_when_not_running() {
        let mock = MockNativeState::new();
        let handle = setup_manager();

        send_set_mobile(&handle, mock.clone()).await;

        // Rust is idle (not started). Native reports NOT running, but DurableState
        // has "timeout" from a previous session.
        let mut ns = native_state(false);
        ns.durable_state = "timeout".into();
        ns.last_platform_error = Some("FGS timeout (type: remoteMessaging)".into());
        mock.set_native_state(ns);

        let status = send_get_lifecycle_status(&handle).await;
        assert!(
            status.degraded == Some(true) || status.degraded_reason.is_some(),
            "stale timeout DurableState should be reflected in status even when Rust is not running — \
             got degraded={:?}, degraded_reason={:?}",
            status.degraded,
            status.degraded_reason,
        );
    }

    // ── BGS-31: graceful SIGTERM stop (doc-08 Step 9) ──────────────────
    //
    // The SIGTERM handler's graceful path is factored into
    // `graceful_sigterm_shutdown` (desktop mod) so this test can drive the
    // SAME code the real signal arm runs — a real SIGTERM is not deliverable
    // in a unit test. Requires `--features desktop-service` (the desktop mod)
    // and is unix-only to match SIGTERM semantics.

    /// A service that stays running until its shutdown token is cancelled AND
    /// records `shutdown_gracefully` (the BGS-31 bounded-drain hook). The hook
    /// is stateless w.r.t. the instance — it sets a shared flag — so a fresh
    /// factory instance setting it is equivalent to the running instance.
    #[cfg(all(unix, feature = "desktop-service"))]
    struct GracefulShutdownService {
        drained: Arc<AtomicBool>,
    }

    #[cfg(all(unix, feature = "desktop-service"))]
    #[async_trait]
    impl BackgroundService<tauri::test::MockRuntime> for GracefulShutdownService {
        async fn init(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        async fn run(
            &mut self,
            ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            // Block until stopped so the running token stays Some — else Stop
            // returns NotRunning and SKIPS all bookkeeping (AC1 precondition).
            ctx.shutdown.cancelled().await;
            Ok(())
        }

        async fn shutdown_gracefully(
            &mut self,
            _ctx: &ServiceContext<tauri::test::MockRuntime>,
        ) -> Result<(), ServiceError> {
            self.drained.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// BGS-31 (doc-08 Step 9): SIGTERM/SIGINT triggers `ManagerCommand::Stop`
    /// that REACHES manager_loop (bookkeeping) AND `ManagerCommand::ShutdownGracefully`
    /// that drives the bounded-drain hook — NOT the abrupt Drop abort.
    #[cfg(all(unix, feature = "desktop-service"))]
    #[tokio::test]
    async fn bgs31_sigterm_graceful_stop() {
        use crate::desktop::headless::graceful_sigterm_shutdown;

        let drained = Arc::new(AtomicBool::new(false));
        let drained_for_factory = drained.clone();
        let handle = setup_manager_with_factory(Box::new(move || {
            Box::new(GracefulShutdownService {
                drained: drained_for_factory.clone(),
            })
        }));

        // AC1 precondition: arrange a RUNNING token first. Without a running
        // service, handle_stop_with_reason returns NotRunning and SKIPS all
        // bookkeeping.
        let app = tauri::test::mock_app();
        send_start(&handle, app.handle().clone())
            .await
            .expect("Start should succeed");
        wait_until_running(&handle).await;
        assert!(
            handle.is_running().await,
            "precondition: service should be running before SIGTERM"
        );

        // Drive the SIGTERM graceful path — the SAME code the signal arm runs.
        graceful_sigterm_shutdown(&handle.cmd_tx).await;

        // AC1 (i): the bookkeeping Stop REACHED manager_loop — is_running
        // flipped to false (handle_stop clears it; the token was Some, so the
        // bookkeeping ran). NOT a placebo: a send that never reached the loop
        // would leave is_running true.
        assert!(
            !handle.is_running().await,
            "Stop did not reach manager_loop (is_running still true)"
        );

        // AC1 (ii): shutdown_gracefully RAN (the bounded-drain hook fired via
        // the fresh factory service) — NOT the abrupt process-exit Drop abort.
        assert!(
            drained.load(Ordering::SeqCst),
            "shutdown_gracefully hook did not run (bounded drain not driven)"
        );
    }
}
