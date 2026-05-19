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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartConfig {
    /// Text shown in the Android persistent foreground notification.
    #[serde(default = "default_label", alias = "label")]
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

    /// Android foreground service types allowed for `startService()`.
    /// Default: `["dataSync"]`. The preflight validation rejects any type
    /// not in this list when `android_validate_foreground_service_type` is true.
    #[serde(default = "default_android_foreground_service_types")]
    pub android_foreground_service_types: Vec<String>,

    /// Whether to validate the requested foreground service type against
    /// `android_foreground_service_types` before starting the native service.
    /// Default: true. Set to false to skip the allowlist check.
    #[serde(default = "default_true")]
    pub android_validate_foreground_service_type: bool,

    /// Timeout policy for Android foreground service.
    /// Values: "stop", "notifyUser" (default), "scheduleRecovery".
    /// - "stop": set desiredRunning=false, stop service.
    /// - "notifyUser": stop service, post timeout notification, keep desiredRunning=true.
    /// - "scheduleRecovery": stop service, set recoveryPending=true, attempt reschedule.
    #[serde(default = "default_android_on_timeout")]
    pub android_on_timeout: String,

    /// Android notification channel ID for the foreground service notification.
    /// Default: "bg_service".
    #[serde(default = "default_android_notification_channel_id")]
    pub android_notification_channel_id: String,

    /// Android notification channel name (visible to the user in system settings).
    /// Default: "Background Service".
    #[serde(default = "default_android_notification_channel_name")]
    pub android_notification_channel_name: String,

    /// Android notification ID for the foreground service notification.
    /// Default: 9001.
    #[serde(default = "default_android_notification_id")]
    pub android_notification_id: u32,

    /// Custom small icon resource name for the foreground notification.
    /// When `None`, the system default (`android.R.drawable.ic_dialog_info`) is used.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub android_notification_small_icon: Option<String>,

    /// Whether to show a stop action button on the foreground notification.
    /// Default: true.
    #[serde(default = "default_true")]
    pub android_show_stop_action: bool,

    /// Whether to automatically request POST_NOTIFICATIONS permission on
    /// Android API 33+ when the plugin loads. Default: true (backward compat).
    /// Set to false to manage permission requests explicitly from JS.
    #[serde(default = "default_true")]
    pub android_request_notification_permission_on_load: bool,

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

    /// Whether the OS service should start automatically on boot (Linux) or
    /// login (macOS). Only applies when `desktop_service_mode` is `"osService"`.
    /// Default: false.
    #[cfg(feature = "desktop-service")]
    #[serde(default)]
    pub desktop_service_autostart: bool,

    /// When `true`, calling `startService()` will automatically start the OS
    /// service if it is not already running (i.e. IPC is disconnected).
    /// Only applies when `desktop_service_mode` is `"osService"`.
    /// Default: false.
    #[cfg(feature = "desktop-service")]
    #[serde(default)]
    pub desktop_start_service_if_missing: bool,

    /// Timeout in milliseconds to wait for the IPC connection to become ready
    /// after starting the OS service sidecar. Only applies when
    /// `desktop_start_service_if_missing` is `true`.
    /// Default: 5000 (5 seconds).
    #[cfg(feature = "desktop-service")]
    #[serde(default = "default_desktop_service_start_timeout_ms")]
    pub desktop_service_start_timeout_ms: u64,
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

fn default_android_foreground_service_types() -> Vec<String> {
    vec!["dataSync".into()]
}

fn default_android_on_timeout() -> String {
    "notifyUser".into()
}

fn default_android_notification_channel_id() -> String {
    "bg_service".into()
}

fn default_android_notification_channel_name() -> String {
    "Background Service".into()
}

fn default_android_notification_id() -> u32 {
    9001
}

fn default_true() -> bool {
    true
}

fn default_channel_capacity() -> usize {
    16
}

#[cfg(feature = "desktop-service")]
fn default_desktop_service_mode() -> String {
    "inProcess".into()
}

#[cfg(feature = "desktop-service")]
fn default_desktop_service_start_timeout_ms() -> u64 {
    5000
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

/// Unified lifecycle state for the background service.
///
/// Provides fine-grained visibility into the service's current state,
/// combining internal state, recovery status, and platform conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum LifecycleState {
    /// No service has been started.
    Idle,
    /// Service init() is in progress.
    Starting,
    /// Service run() is executing.
    Running,
    /// Service is being stopped (cancellation requested).
    Stopping,
    /// Service has stopped.
    Stopped,
    /// Service is recovering after a platform timeout or expiration.
    Recovering,
    /// Recovery is pending (waiting for platform conditions).
    RecoveryPending,
    /// Background execution window has expired (e.g. iOS BGTask).
    Expired,
    /// Service is blocked by a platform issue (e.g. missing permission).
    Blocked,
    /// Service encountered an error.
    Error,
}

impl From<ServiceState> for LifecycleState {
    fn from(state: ServiceState) -> Self {
        match state {
            ServiceState::Idle => LifecycleState::Idle,
            ServiceState::Initializing => LifecycleState::Starting,
            ServiceState::Running => LifecycleState::Running,
            ServiceState::Stopped => LifecycleState::Stopped,
        }
    }
}

/// Native platform-side state reported by the OS service layer.
///
/// Reflects the state as observed by the Android foreground service, iOS
/// BGTask handler, or desktop OS-service process — distinct from the
/// plugin-internal [`ServiceState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum NativeState {
    Idle,
    Starting,
    Running,
    Stopping,
    Timeout,
    Expired,
    Recovering,
    Error,
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

    // --- Extended fields (Step 4) ---
    /// Whether the service is desired to be running (persisted across restarts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_running: Option<bool>,
    /// Platform-native state as reported by the OS service layer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_state: Option<NativeState>,
    /// The lifecycle mechanism in use on the current platform.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_mode: Option<LifecycleMode>,
    /// Configuration used for the last successful start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_start_config: Option<StartConfig>,
    /// Epoch milliseconds of the last heartbeat received from the service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<u64>,
    /// How many restart attempts have been made since the last clean start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_attempt: Option<u32>,
    /// Human-readable reason for the current recovery attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_reason: Option<String>,
    /// Last platform-specific error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_error: Option<String>,
}

impl Default for ServiceStatus {
    fn default() -> Self {
        Self {
            state: ServiceState::Idle,
            last_error: None,
            desired_running: None,
            native_state: None,
            platform_mode: None,
            last_start_config: None,
            last_heartbeat_at: None,
            restart_attempt: None,
            recovery_reason: None,
            platform_error: None,
        }
    }
}

/// The operating system platform.
///
/// Returned by `get_platform_capabilities` to identify the current runtime environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Platform {
    Android,
    Ios,
    Windows,
    Macos,
    Linux,
    Unknown,
}

/// Severity level for a validation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// The lifecycle mechanism used by the plugin on the current platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum LifecycleMode {
    AndroidForegroundService,
    IosBgTaskScheduler,
    DesktopInProcess,
    DesktopOsService,
}

/// Guarantee level for a specific background execution scenario.
///
/// - `Guaranteed`: The platform reliably supports this scenario.
/// - `BestEffort`: The platform may support this scenario but cannot guarantee it.
/// - `Unsupported`: The platform does not support this scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum LifecycleGuarantee {
    Guaranteed,
    BestEffort,
    Unsupported,
}

/// Platform-specific background execution capabilities.
///
/// Returned by the `get_platform_capabilities` Tauri command. Provides honest
/// reporting of what each platform can guarantee for background service survival.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PlatformCapabilities {
    pub platform: Platform,
    pub lifecycle_mode: LifecycleMode,
    pub survives_app_close: LifecycleGuarantee,
    pub survives_reboot: LifecycleGuarantee,
    pub survives_force_quit: LifecycleGuarantee,
    pub background_execution: LifecycleGuarantee,
    pub limitations: Vec<String>,
    pub required_setup: Vec<String>,
}

/// OS service install/running state.
///
/// Reported by [`OsServiceStatus`] to indicate whether the OS-level service
/// (systemd, launchd, etc.) is installed and/or currently running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum OsServiceInstallState {
    /// The OS service is not installed.
    NotInstalled,
    /// The OS service is installed but not currently running.
    Installed,
    /// The OS service is installed and currently running.
    Running,
}

/// Snapshot of an OS-level service's status.
///
/// Returned by `get_os_service_status` to report the state of the desktop
/// OS service (systemd user service, launchd agent).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct OsServiceStatus {
    /// The service label (e.g. `com.example.background-service`).
    pub label: String,
    /// The service manager kind (e.g. `systemd`, `launchd`).
    pub mode: String,
    /// Whether the service is installed and/or running.
    pub installed: OsServiceInstallState,
    /// Whether the IPC connection to the service sidecar is active.
    pub ipc_connected: bool,
    /// Path to the Unix domain socket used for IPC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
    /// Last error message from the OS service, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Reason why the background service stopped.
///
/// Structured stop reasons that distinguish between user-initiated stops,
/// platform-imposed terminations, and natural task completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum StopReason {
    /// User called stopService().
    UserStop,
    /// Application is shutting down gracefully.
    AppStop,
    /// Platform killed the service due to a timeout (e.g. Android FGS timeout).
    PlatformTimeout,
    /// Platform expired the background execution window (e.g. iOS BGTask).
    PlatformExpiration,
    /// User pressed stop on the native notification.
    NativeNotificationStop,
    /// OS restarted the service after a reboot.
    OsRestart,
    /// Service recovered after device boot.
    BootRecovery,
    /// Service's run() returned Ok(()) naturally.
    TaskCompleted,
    /// Service's run() returned an error.
    Error,
}

impl<'de> serde::Deserialize<'de> for StopReason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "userStop" => Ok(Self::UserStop),
            "appStop" => Ok(Self::AppStop),
            "platformTimeout" => Ok(Self::PlatformTimeout),
            "platformExpiration" => Ok(Self::PlatformExpiration),
            "nativeNotificationStop" => Ok(Self::NativeNotificationStop),
            "osRestart" => Ok(Self::OsRestart),
            "bootRecovery" => Ok(Self::BootRecovery),
            "taskCompleted" => Ok(Self::TaskCompleted),
            "error" => Ok(Self::Error),
            // Legacy string mappings for backward compatibility
            "completed" => Ok(Self::TaskCompleted),
            "cancelled" | "user" => Ok(Self::UserStop),
            _ => Err(serde::de::Error::unknown_variant(
                &s,
                &[
                    "userStop",
                    "appStop",
                    "platformTimeout",
                    "platformExpiration",
                    "nativeNotificationStop",
                    "osRestart",
                    "bootRecovery",
                    "taskCompleted",
                    "error",
                ],
            )),
        }
    }
}

/// Native platform lifecycle events sent from Kotlin/Swift to the Rust actor.
///
/// These events originate in the native layer and are forwarded to the actor
/// via [`ManagerCommand::NativeLifecycleEvent`](crate::manager::ManagerCommand).
/// The actor maps each variant to the appropriate [`StopReason`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
#[non_exhaustive]
pub enum NativeLifecycleEvent {
    /// User pressed stop on the Android foreground service notification.
    AndroidNotificationStop,
    /// Android system killed the foreground service due to a timeout.
    AndroidTimeout {
        /// The foreground service type that timed out (e.g. "dataSync").
        #[serde(skip_serializing_if = "Option::is_none")]
        fgs_type: Option<String>,
    },
}

impl NativeLifecycleEvent {
    /// Map this native event to the corresponding [`StopReason`].
    pub fn to_stop_reason(&self) -> StopReason {
        match self {
            Self::AndroidNotificationStop => StopReason::NativeNotificationStop,
            Self::AndroidTimeout { .. } => StopReason::PlatformTimeout,
        }
    }
}

/// Built-in event types emitted by the runner itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
#[non_exhaustive]
pub enum PluginEvent {
    /// init() completed successfully
    Started,
    /// run() returned or was cancelled
    Stopped { reason: StopReason },
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
            android_foreground_service_types: default_android_foreground_service_types(),
            android_validate_foreground_service_type: default_true(),
            android_on_timeout: default_android_on_timeout(),
            android_notification_channel_id: default_android_notification_channel_id(),
            android_notification_channel_name: default_android_notification_channel_name(),
            android_notification_id: default_android_notification_id(),
            android_notification_small_icon: None,
            android_show_stop_action: default_true(),
            android_request_notification_permission_on_load: default_true(),
            #[cfg(feature = "desktop-service")]
            desktop_service_mode: default_desktop_service_mode(),
            #[cfg(feature = "desktop-service")]
            desktop_service_label: None,
            #[cfg(feature = "desktop-service")]
            desktop_service_autostart: false,
            #[cfg(feature = "desktop-service")]
            desktop_start_service_if_missing: false,
            #[cfg(feature = "desktop-service")]
            desktop_service_start_timeout_ms: default_desktop_service_start_timeout_ms(),
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

/// Information about a pending iOS background task that launched the app.
///
/// Returned by `getPendingBgTask()` on iOS when the app was launched by iOS
/// for a background task (BGAppRefreshTask or BGProcessingTask). Used by the
/// Rust auto-start logic to detect OS-initiated launches and automatically
/// start the service if `desired_running` is true.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PendingTaskInfo {
    /// The kind of background task: "refresh" or "processing".
    pub task_kind: String,
    /// The task identifier (e.g. "com.example.app.bg-refresh").
    pub identifier: String,
    /// Epoch timestamp (seconds) when the task was received by the native layer.
    pub received_at: f64,
    /// Epoch timestamp (seconds) when the pending task was consumed by the Rust
    /// auto-start logic. `None` until `clear_pending_bg_task` is called.
    #[serde(default)]
    pub consumed_at: Option<f64>,
}

/// iOS scheduling status returned by the native layer.
///
/// Reports which background task types were successfully scheduled
/// and any errors that occurred during scheduling. Returned by
/// `get_scheduling_status` and parsed from `startKeepalive` on iOS.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct IOSSchedulingStatus {
    /// Whether a `BGAppRefreshTask` was successfully scheduled.
    pub refresh_scheduled: bool,
    /// Whether a `BGProcessingTask` was successfully scheduled.
    pub processing_scheduled: bool,
    /// Error from `BGAppRefreshTask` scheduling, if any.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_error: Option<String>,
    /// Error from `BGProcessingTask` scheduling, if any.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processing_error: Option<String>,
}

/// A single setup issue found during validation.
///
/// Part of [`SetupValidationReport`]. Each issue has a machine-readable code,
/// a human-readable message, the platform it applies to, and an optional fix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SetupIssue {
    /// Machine-readable error code (e.g. "android_fgs_type").
    pub code: String,
    /// Human-readable description of the issue.
    pub message: String,
    /// The platform this issue applies to.
    pub platform: Platform,
    /// Suggested fix for the issue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl SetupIssue {
    /// Convert this `SetupIssue` into a [`ValidationIssue`] with the given severity.
    pub fn to_validation_issue(&self, severity: Severity) -> ValidationIssue {
        ValidationIssue {
            severity,
            code: self.code.clone(),
            message: self.message.clone(),
            fix: self.fix.clone(),
            platform: self.platform,
        }
    }
}

/// Result of validating background service setup prerequisites.
///
/// Returned by `validateBackgroundServiceSetup()`. Contains `errors` (blocking
/// issues that prevent the service from working) and `warnings` (non-blocking
/// issues that may cause degraded behavior).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SetupValidationReport {
    /// `true` when `errors` is empty (warnings do not affect this).
    pub ok: bool,
    /// Blocking issues that prevent the service from working correctly.
    pub errors: Vec<SetupIssue>,
    /// Non-blocking issues that may cause degraded behavior.
    pub warnings: Vec<SetupIssue>,
    /// Unified issues with typed severity.
    ///
    /// Combines `errors` (as [`Severity::Error`]) and `warnings` (as
    /// [`Severity::Warning`]) into a single list. Populated automatically
    /// by [`crate::validator::SetupValidator::validate`].
    #[serde(default)]
    pub issues: Vec<ValidationIssue>,
}

/// A single validation issue found during lifecycle validation.
///
/// Each issue has a severity level, machine-readable code, human-readable
/// message, optional fix suggestion, and the platform it applies to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ValidationIssue {
    pub severity: Severity,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    pub platform: Platform,
}

/// Complete snapshot of the background service lifecycle status.
///
/// Provides a unified view of service state, desired state, recovery status,
/// platform capabilities, and validation issues.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct LifecycleStatus {
    pub state: LifecycleState,
    pub desired_running: bool,
    pub recovery_enabled: bool,
    pub recovery_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_start_config: Option<StartConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_platform_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_platform_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub platform: Platform,
    pub capabilities: PlatformCapabilities,
    pub issues: Vec<ValidationIssue>,
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

    // --- StartConfig alias tests ---

    #[test]
    fn start_config_legacy_label_alias_decodes() {
        let json = r#"{"label":"Legacy name"}"#;
        let de: StartConfig = serde_json::from_str(json).unwrap();
        assert_eq!(de.service_label, "Legacy name");
    }

    #[test]
    fn start_config_both_label_and_service_label_rejected() {
        let json = r#"{"serviceLabel":"New name","label":"Old name"}"#;
        let result = serde_json::from_str::<StartConfig>(json);
        assert!(result.is_err(), "should reject duplicate field via alias");
    }

    #[test]
    fn start_config_unknown_fields_ignored() {
        let json = r#"{"serviceLabel":"test","unknownField":42,"extra":"data"}"#;
        let de: StartConfig = serde_json::from_str(json).unwrap();
        assert_eq!(de.service_label, "test");
        assert_eq!(de.foreground_service_type, "dataSync");
    }

    #[test]
    fn start_config_camel_case_key_still_works() {
        let json = r#"{"serviceLabel":"Modern name"}"#;
        let de: StartConfig = serde_json::from_str(json).unwrap();
        assert_eq!(de.service_label, "Modern name");
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
            reason: StopReason::UserStop,
        };
        let json = serde_json::to_string(&event).unwrap();
        let de: PluginEvent = serde_json::from_str(&json).unwrap();
        match de {
            PluginEvent::Stopped { reason } => assert_eq!(reason, StopReason::UserStop),
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
            reason: StopReason::TaskCompleted,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"stopped\""), "Tag: {json}");
        assert!(
            json.contains("\"reason\":\"taskCompleted\""),
            "Reason: {json}"
        );
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

    // --- StopReason tests ---

    #[test]
    fn stop_reason_all_variants_serialize_to_camel_case() {
        assert_eq!(
            serde_json::to_string(&StopReason::UserStop).unwrap(),
            "\"userStop\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::AppStop).unwrap(),
            "\"appStop\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::PlatformTimeout).unwrap(),
            "\"platformTimeout\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::PlatformExpiration).unwrap(),
            "\"platformExpiration\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::NativeNotificationStop).unwrap(),
            "\"nativeNotificationStop\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::OsRestart).unwrap(),
            "\"osRestart\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::BootRecovery).unwrap(),
            "\"bootRecovery\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::TaskCompleted).unwrap(),
            "\"taskCompleted\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::Error).unwrap(),
            "\"error\""
        );
    }

    #[test]
    fn stop_reason_roundtrip_all_variants() {
        for variant in [
            StopReason::UserStop,
            StopReason::AppStop,
            StopReason::PlatformTimeout,
            StopReason::PlatformExpiration,
            StopReason::NativeNotificationStop,
            StopReason::OsRestart,
            StopReason::BootRecovery,
            StopReason::TaskCompleted,
            StopReason::Error,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let de: StopReason = serde_json::from_str(&json).unwrap();
            assert_eq!(de, variant, "roundtrip failed for {variant:?}");
        }
    }

    #[test]
    fn stop_reason_legacy_completed_maps_to_task_completed() {
        let json = "\"completed\"";
        let de: StopReason = serde_json::from_str(json).unwrap();
        assert_eq!(de, StopReason::TaskCompleted);
    }

    #[test]
    fn stop_reason_legacy_cancelled_maps_to_user_stop() {
        let json = "\"cancelled\"";
        let de: StopReason = serde_json::from_str(json).unwrap();
        assert_eq!(de, StopReason::UserStop);
    }

    #[test]
    fn stop_reason_legacy_user_maps_to_user_stop() {
        let json = "\"user\"";
        let de: StopReason = serde_json::from_str(json).unwrap();
        assert_eq!(de, StopReason::UserStop);
    }

    #[test]
    fn stop_reason_unknown_variant_returns_error() {
        let json = "\"unknownReason\"";
        let result = serde_json::from_str::<StopReason>(json);
        assert!(
            result.is_err(),
            "unknown variant should fail to deserialize"
        );
    }

    // --- NativeLifecycleEvent tests ---

    #[test]
    fn native_lifecycle_event_android_notification_stop_roundtrip() {
        let event = NativeLifecycleEvent::AndroidNotificationStop;
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"type":"androidNotificationStop"}"#);
        let de: NativeLifecycleEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(de, event);
    }

    #[test]
    fn native_lifecycle_event_android_timeout_roundtrip() {
        let event = NativeLifecycleEvent::AndroidTimeout {
            fgs_type: Some("dataSync".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let de: NativeLifecycleEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(de, event);
    }

    #[test]
    fn native_lifecycle_event_android_timeout_without_fgs_type() {
        let event = NativeLifecycleEvent::AndroidTimeout { fgs_type: None };
        let json = serde_json::to_string(&event).unwrap();
        // skip_serializing_if means fgs_type is omitted when None
        assert!(!json.contains("fgsType"), "{json}");
        let de: NativeLifecycleEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(de, event);
    }

    #[test]
    fn native_lifecycle_event_to_stop_reason_mapping() {
        assert_eq!(
            NativeLifecycleEvent::AndroidNotificationStop.to_stop_reason(),
            StopReason::NativeNotificationStop
        );
        assert_eq!(
            NativeLifecycleEvent::AndroidTimeout { fgs_type: None }.to_stop_reason(),
            StopReason::PlatformTimeout
        );
        assert_eq!(
            NativeLifecycleEvent::AndroidTimeout {
                fgs_type: Some("dataSync".into())
            }
            .to_stop_reason(),
            StopReason::PlatformTimeout
        );
    }

    #[test]
    fn plugin_event_stopped_with_stop_reason_roundtrip() {
        let event = PluginEvent::Stopped {
            reason: StopReason::TaskCompleted,
        };
        let json = serde_json::to_string(&event).unwrap();
        let de: PluginEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(
            de,
            PluginEvent::Stopped {
                reason: StopReason::TaskCompleted
            }
        );
    }

    #[test]
    fn plugin_event_stopped_legacy_reason_deserializes() {
        // Simulates receiving a legacy event with reason "completed"
        let json = r#"{"type":"stopped","reason":"completed"}"#;
        let de: PluginEvent = serde_json::from_str(json).unwrap();
        match de {
            PluginEvent::Stopped { reason } => {
                assert_eq!(reason, StopReason::TaskCompleted);
            }
            other => panic!("Expected Stopped, got {other:?}"),
        }
    }

    #[test]
    fn plugin_event_stopped_legacy_cancelled_deserializes() {
        let json = r#"{"type":"stopped","reason":"cancelled"}"#;
        let de: PluginEvent = serde_json::from_str(json).unwrap();
        match de {
            PluginEvent::Stopped { reason } => {
                assert_eq!(reason, StopReason::UserStop);
            }
            other => panic!("Expected Stopped, got {other:?}"),
        }
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

    // --- PluginConfig android FGS type fields tests ---

    #[test]
    fn plugin_config_android_fgs_types_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.android_foreground_service_types, vec!["dataSync"]);
    }

    #[test]
    fn plugin_config_android_fgs_types_custom() {
        let json = r#"{"androidForegroundServiceTypes":["dataSync","specialUse"]}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.android_foreground_service_types,
            vec!["dataSync", "specialUse"]
        );
    }

    #[test]
    fn plugin_config_android_fgs_types_serde_roundtrip() {
        let config = PluginConfig {
            android_foreground_service_types: vec!["location".into(), "connectedDevice".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            de.android_foreground_service_types,
            vec!["location", "connectedDevice"]
        );
    }

    #[test]
    fn plugin_config_android_fgs_types_json_key_camel_case() {
        let config = PluginConfig {
            android_foreground_service_types: vec!["specialUse".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("androidForegroundServiceTypes"),
            "JSON should use camelCase: {json}"
        );
    }

    #[test]
    fn plugin_config_android_validate_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(config.android_validate_foreground_service_type);
    }

    #[test]
    fn plugin_config_android_validate_false() {
        let json = r#"{"androidValidateForegroundServiceType":false}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(!config.android_validate_foreground_service_type);
    }

    #[test]
    fn plugin_config_android_validate_serde_roundtrip() {
        let config = PluginConfig {
            android_validate_foreground_service_type: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert!(!de.android_validate_foreground_service_type);
    }

    #[test]
    fn plugin_config_android_validate_json_key_camel_case() {
        let config = PluginConfig {
            android_validate_foreground_service_type: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("androidValidateForegroundServiceType"),
            "JSON should use camelCase: {json}"
        );
    }

    // --- PluginConfig Android timeout/notification config tests ---

    #[test]
    fn plugin_config_android_on_timeout_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.android_on_timeout, "notifyUser");
    }

    #[test]
    fn plugin_config_android_on_timeout_custom() {
        let json = r#"{"androidOnTimeout":"stop"}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.android_on_timeout, "stop");
    }

    #[test]
    fn plugin_config_android_on_timeout_schedule_recovery() {
        let json = r#"{"androidOnTimeout":"scheduleRecovery"}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.android_on_timeout, "scheduleRecovery");
    }

    #[test]
    fn plugin_config_android_on_timeout_serde_roundtrip() {
        let config = PluginConfig {
            android_on_timeout: "stop".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.android_on_timeout, "stop");
    }

    #[test]
    fn plugin_config_android_on_timeout_json_key_camel_case() {
        let config = PluginConfig {
            android_on_timeout: "notifyUser".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("androidOnTimeout"),
            "JSON should use camelCase: {json}"
        );
    }

    #[test]
    fn plugin_config_android_notification_channel_id_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.android_notification_channel_id, "bg_service");
    }

    #[test]
    fn plugin_config_android_notification_channel_id_custom() {
        let json = r#"{"androidNotificationChannelId":"my_channel"}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.android_notification_channel_id, "my_channel");
    }

    #[test]
    fn plugin_config_android_notification_channel_id_serde_roundtrip() {
        let config = PluginConfig {
            android_notification_channel_id: "custom_ch".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.android_notification_channel_id, "custom_ch");
    }

    #[test]
    fn plugin_config_android_notification_channel_id_json_key_camel_case() {
        let config = PluginConfig {
            android_notification_channel_id: "test".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("androidNotificationChannelId"),
            "JSON should use camelCase: {json}"
        );
    }

    #[test]
    fn plugin_config_android_notification_channel_name_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.android_notification_channel_name,
            "Background Service"
        );
    }

    #[test]
    fn plugin_config_android_notification_channel_name_custom() {
        let json = r#"{"androidNotificationChannelName":"My Service"}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.android_notification_channel_name, "My Service");
    }

    #[test]
    fn plugin_config_android_notification_channel_name_serde_roundtrip() {
        let config = PluginConfig {
            android_notification_channel_name: "Sync Service".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.android_notification_channel_name, "Sync Service");
    }

    #[test]
    fn plugin_config_android_notification_channel_name_json_key_camel_case() {
        let config = PluginConfig {
            android_notification_channel_name: "Test".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("androidNotificationChannelName"),
            "JSON should use camelCase: {json}"
        );
    }

    #[test]
    fn plugin_config_android_notification_id_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.android_notification_id, 9001);
    }

    #[test]
    fn plugin_config_android_notification_id_custom() {
        let json = r#"{"androidNotificationId":1234}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.android_notification_id, 1234);
    }

    #[test]
    fn plugin_config_android_notification_id_serde_roundtrip() {
        let config = PluginConfig {
            android_notification_id: 42,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.android_notification_id, 42);
    }

    #[test]
    fn plugin_config_android_notification_id_json_key_camel_case() {
        let config = PluginConfig {
            android_notification_id: 5555,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("androidNotificationId"),
            "JSON should use camelCase: {json}"
        );
    }

    #[test]
    fn plugin_config_android_notification_small_icon_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.android_notification_small_icon, None);
    }

    #[test]
    fn plugin_config_android_notification_small_icon_custom() {
        let json = r#"{"androidNotificationSmallIcon":"ic_notification"}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.android_notification_small_icon,
            Some("ic_notification".to_string())
        );
    }

    #[test]
    fn plugin_config_android_notification_small_icon_serde_roundtrip() {
        let config = PluginConfig {
            android_notification_small_icon: Some("my_icon".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.android_notification_small_icon, Some("my_icon".into()));
    }

    #[test]
    fn plugin_config_android_notification_small_icon_absent_when_none() {
        let config = PluginConfig {
            android_notification_small_icon: None,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("androidNotificationSmallIcon"),
            "should be absent when None: {json}"
        );
    }

    #[test]
    fn plugin_config_android_notification_small_icon_json_key_camel_case() {
        let config = PluginConfig {
            android_notification_small_icon: Some("icon".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("androidNotificationSmallIcon"),
            "JSON should use camelCase: {json}"
        );
    }

    #[test]
    fn plugin_config_android_show_stop_action_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(config.android_show_stop_action);
    }

    #[test]
    fn plugin_config_android_show_stop_action_false() {
        let json = r#"{"androidShowStopAction":false}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(!config.android_show_stop_action);
    }

    #[test]
    fn plugin_config_android_show_stop_action_serde_roundtrip() {
        let config = PluginConfig {
            android_show_stop_action: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert!(!de.android_show_stop_action);
    }

    #[test]
    fn plugin_config_android_show_stop_action_json_key_camel_case() {
        let config = PluginConfig {
            android_show_stop_action: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("androidShowStopAction"),
            "JSON should use camelCase: {json}"
        );
    }

    // --- PluginConfig android_request_notification_permission_on_load tests ---

    #[test]
    fn plugin_config_android_request_notification_permission_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(config.android_request_notification_permission_on_load);
    }

    #[test]
    fn plugin_config_android_request_notification_permission_false() {
        let json = r#"{"androidRequestNotificationPermissionOnLoad":false}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(!config.android_request_notification_permission_on_load);
    }

    #[test]
    fn plugin_config_android_request_notification_permission_serde_roundtrip() {
        let config = PluginConfig {
            android_request_notification_permission_on_load: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert!(!de.android_request_notification_permission_on_load);
    }

    #[test]
    fn plugin_config_android_timeout_notification_full_roundtrip() {
        let config = PluginConfig {
            android_on_timeout: "scheduleRecovery".into(),
            android_notification_channel_id: "my_ch".into(),
            android_notification_channel_name: "My Channel".into(),
            android_notification_id: 42,
            android_notification_small_icon: Some("ic_bg".into()),
            android_show_stop_action: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.android_on_timeout, "scheduleRecovery");
        assert_eq!(de.android_notification_channel_id, "my_ch");
        assert_eq!(de.android_notification_channel_name, "My Channel");
        assert_eq!(de.android_notification_id, 42);
        assert_eq!(de.android_notification_small_icon, Some("ic_bg".into()));
        assert!(!de.android_show_stop_action);
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

    // --- PluginConfig desktop autostart/start-if-missing/timeout tests ---

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_autostart_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(!config.desktop_service_autostart);
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_autostart_true() {
        let json = r#"{"desktopServiceAutostart":true}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(config.desktop_service_autostart);
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_autostart_serde_roundtrip() {
        let config = PluginConfig {
            desktop_service_autostart: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert!(de.desktop_service_autostart);
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_autostart_json_key_camel_case() {
        let config = PluginConfig {
            desktop_service_autostart: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("desktopServiceAutostart"),
            "JSON should use camelCase: {json}"
        );
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_start_if_missing_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(!config.desktop_start_service_if_missing);
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_start_if_missing_true() {
        let json = r#"{"desktopStartServiceIfMissing":true}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert!(config.desktop_start_service_if_missing);
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_start_if_missing_serde_roundtrip() {
        let config = PluginConfig {
            desktop_start_service_if_missing: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert!(de.desktop_start_service_if_missing);
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_start_if_missing_json_key_camel_case() {
        let config = PluginConfig {
            desktop_start_service_if_missing: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("desktopStartServiceIfMissing"),
            "JSON should use camelCase: {json}"
        );
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_start_timeout_default() {
        let json = "{}";
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.desktop_service_start_timeout_ms, 5000);
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_start_timeout_custom() {
        let json = r#"{"desktopServiceStartTimeoutMs":10000}"#;
        let config: PluginConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.desktop_service_start_timeout_ms, 10000);
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_start_timeout_serde_roundtrip() {
        let config = PluginConfig {
            desktop_service_start_timeout_ms: 15000,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.desktop_service_start_timeout_ms, 15000);
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_start_timeout_json_key_camel_case() {
        let config = PluginConfig {
            desktop_service_start_timeout_ms: 3000,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("desktopServiceStartTimeoutMs"),
            "JSON should use camelCase: {json}"
        );
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn plugin_config_desktop_all_new_fields_roundtrip() {
        let config = PluginConfig {
            desktop_service_autostart: true,
            desktop_start_service_if_missing: true,
            desktop_service_start_timeout_ms: 8000,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: PluginConfig = serde_json::from_str(&json).unwrap();
        assert!(de.desktop_service_autostart);
        assert!(de.desktop_start_service_if_missing);
        assert_eq!(de.desktop_service_start_timeout_ms, 8000);
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"state\":"), "state key: {json}");
        assert!(json.contains("\"lastError\":"), "lastError key: {json}");
    }

    #[test]
    fn service_status_json_null_last_error() {
        let status = ServiceStatus {
            state: ServiceState::Idle,
            ..Default::default()
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            json.contains("\"lastError\":null"),
            "lastError should be null: {json}"
        );
    }

    // --- Platform tests ---

    #[test]
    fn platform_serde_roundtrip() {
        for variant in [
            Platform::Android,
            Platform::Ios,
            Platform::Windows,
            Platform::Macos,
            Platform::Linux,
            Platform::Unknown,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let de: Platform = serde_json::from_str(&json).unwrap();
            assert_eq!(de, variant);
        }
    }

    #[test]
    fn platform_json_values_are_camel_case() {
        assert_eq!(
            serde_json::to_string(&Platform::Android).unwrap(),
            "\"android\""
        );
        assert_eq!(serde_json::to_string(&Platform::Ios).unwrap(), "\"ios\"");
        assert_eq!(
            serde_json::to_string(&Platform::Windows).unwrap(),
            "\"windows\""
        );
        assert_eq!(
            serde_json::to_string(&Platform::Macos).unwrap(),
            "\"macos\""
        );
        assert_eq!(
            serde_json::to_string(&Platform::Linux).unwrap(),
            "\"linux\""
        );
        assert_eq!(
            serde_json::to_string(&Platform::Unknown).unwrap(),
            "\"unknown\""
        );
    }

    // --- LifecycleMode tests ---

    #[test]
    fn lifecycle_mode_serde_roundtrip() {
        for variant in [
            LifecycleMode::AndroidForegroundService,
            LifecycleMode::IosBgTaskScheduler,
            LifecycleMode::DesktopInProcess,
            LifecycleMode::DesktopOsService,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let de: LifecycleMode = serde_json::from_str(&json).unwrap();
            assert_eq!(de, variant);
        }
    }

    #[test]
    fn lifecycle_mode_json_values_are_camel_case() {
        assert_eq!(
            serde_json::to_string(&LifecycleMode::AndroidForegroundService).unwrap(),
            "\"androidForegroundService\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleMode::IosBgTaskScheduler).unwrap(),
            "\"iosBgTaskScheduler\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleMode::DesktopInProcess).unwrap(),
            "\"desktopInProcess\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleMode::DesktopOsService).unwrap(),
            "\"desktopOsService\""
        );
    }

    // --- LifecycleGuarantee tests ---

    #[test]
    fn lifecycle_guarantee_serde_roundtrip() {
        for variant in [
            LifecycleGuarantee::Guaranteed,
            LifecycleGuarantee::BestEffort,
            LifecycleGuarantee::Unsupported,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let de: LifecycleGuarantee = serde_json::from_str(&json).unwrap();
            assert_eq!(de, variant);
        }
    }

    #[test]
    fn lifecycle_guarantee_json_values_are_camel_case() {
        assert_eq!(
            serde_json::to_string(&LifecycleGuarantee::Guaranteed).unwrap(),
            "\"guaranteed\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleGuarantee::BestEffort).unwrap(),
            "\"bestEffort\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleGuarantee::Unsupported).unwrap(),
            "\"unsupported\""
        );
    }

    // --- PlatformCapabilities tests ---

    #[test]
    fn platform_capabilities_serde_roundtrip() {
        let caps = PlatformCapabilities {
            platform: Platform::Android,
            lifecycle_mode: LifecycleMode::AndroidForegroundService,
            survives_app_close: LifecycleGuarantee::BestEffort,
            survives_reboot: LifecycleGuarantee::BestEffort,
            survives_force_quit: LifecycleGuarantee::Unsupported,
            background_execution: LifecycleGuarantee::Guaranteed,
            limitations: vec!["OEM battery optimization".into()],
            required_setup: vec!["FOREGROUND_SERVICE permission".into()],
        };
        let json = serde_json::to_string(&caps).unwrap();
        let de: PlatformCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(de, caps);
    }

    #[test]
    fn platform_capabilities_json_keys_camel_case() {
        let caps = PlatformCapabilities {
            platform: Platform::Linux,
            lifecycle_mode: LifecycleMode::DesktopInProcess,
            survives_app_close: LifecycleGuarantee::Unsupported,
            survives_reboot: LifecycleGuarantee::Unsupported,
            survives_force_quit: LifecycleGuarantee::Unsupported,
            background_execution: LifecycleGuarantee::Guaranteed,
            limitations: vec![],
            required_setup: vec![],
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(json.contains("\"platform\":"), "platform: {json}");
        assert!(json.contains("\"lifecycleMode\":"), "lifecycleMode: {json}");
        assert!(
            json.contains("\"survivesAppClose\":"),
            "survivesAppClose: {json}"
        );
        assert!(
            json.contains("\"survivesReboot\":"),
            "survivesReboot: {json}"
        );
        assert!(
            json.contains("\"survivesForceQuit\":"),
            "survivesForceQuit: {json}"
        );
        assert!(
            json.contains("\"backgroundExecution\":"),
            "backgroundExecution: {json}"
        );
        assert!(json.contains("\"limitations\":"), "limitations: {json}");
        assert!(json.contains("\"requiredSetup\":"), "requiredSetup: {json}");
    }

    #[test]
    fn platform_capabilities_empty_collections_serialize() {
        let caps = PlatformCapabilities {
            platform: Platform::Unknown,
            lifecycle_mode: LifecycleMode::DesktopInProcess,
            survives_app_close: LifecycleGuarantee::Unsupported,
            survives_reboot: LifecycleGuarantee::Unsupported,
            survives_force_quit: LifecycleGuarantee::Unsupported,
            background_execution: LifecycleGuarantee::Unsupported,
            limitations: vec![],
            required_setup: vec![],
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(json.contains("\"limitations\":[]"), "{json}");
        assert!(json.contains("\"requiredSetup\":[]"), "{json}");
    }

    // --- NativeState tests ---

    #[test]
    fn native_state_serde_roundtrip() {
        for variant in [
            NativeState::Idle,
            NativeState::Starting,
            NativeState::Running,
            NativeState::Stopping,
            NativeState::Timeout,
            NativeState::Expired,
            NativeState::Recovering,
            NativeState::Error,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let de: NativeState = serde_json::from_str(&json).unwrap();
            assert_eq!(de, variant, "roundtrip failed for {variant:?}");
        }
    }

    #[test]
    fn native_state_json_values_are_camel_case() {
        assert_eq!(
            serde_json::to_string(&NativeState::Idle).unwrap(),
            "\"idle\""
        );
        assert_eq!(
            serde_json::to_string(&NativeState::Starting).unwrap(),
            "\"starting\""
        );
        assert_eq!(
            serde_json::to_string(&NativeState::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&NativeState::Stopping).unwrap(),
            "\"stopping\""
        );
        assert_eq!(
            serde_json::to_string(&NativeState::Timeout).unwrap(),
            "\"timeout\""
        );
        assert_eq!(
            serde_json::to_string(&NativeState::Expired).unwrap(),
            "\"expired\""
        );
        assert_eq!(
            serde_json::to_string(&NativeState::Recovering).unwrap(),
            "\"recovering\""
        );
        assert_eq!(
            serde_json::to_string(&NativeState::Error).unwrap(),
            "\"error\""
        );
    }

    // --- Extended ServiceStatus tests ---

    #[test]
    fn service_status_backward_compat_deserialize_old_json() {
        let old_json = r#"{"state":"running","lastError":null}"#;
        let status: ServiceStatus = serde_json::from_str(old_json).unwrap();
        assert_eq!(status.state, ServiceState::Running);
        assert_eq!(status.last_error, None);
        assert_eq!(status.desired_running, None);
        assert_eq!(status.native_state, None);
        assert_eq!(status.platform_mode, None);
        assert_eq!(status.last_start_config, None);
        assert_eq!(status.last_heartbeat_at, None);
        assert_eq!(status.restart_attempt, None);
        assert_eq!(status.recovery_reason, None);
        assert_eq!(status.platform_error, None);
    }

    #[test]
    fn service_status_new_fields_serialize_when_present() {
        let status = ServiceStatus {
            state: ServiceState::Running,
            last_error: None,
            desired_running: Some(true),
            native_state: Some(NativeState::Running),
            platform_mode: Some(LifecycleMode::AndroidForegroundService),
            last_start_config: Some(StartConfig::default()),
            last_heartbeat_at: Some(1234567890),
            restart_attempt: Some(2),
            recovery_reason: Some("boot recovery".into()),
            platform_error: Some("timeout exceeded".into()),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"desiredRunning\":true"), "{json}");
        assert!(json.contains("\"nativeState\":\"running\""), "{json}");
        assert!(
            json.contains("\"platformMode\":\"androidForegroundService\""),
            "{json}"
        );
        assert!(json.contains("\"lastHeartbeatAt\":1234567890"), "{json}");
        assert!(json.contains("\"restartAttempt\":2"), "{json}");
        assert!(
            json.contains("\"recoveryReason\":\"boot recovery\""),
            "{json}"
        );
        assert!(
            json.contains("\"platformError\":\"timeout exceeded\""),
            "{json}"
        );
    }

    #[test]
    fn service_status_new_fields_absent_when_none() {
        let status = ServiceStatus {
            state: ServiceState::Idle,
            last_error: None,
            desired_running: None,
            native_state: None,
            platform_mode: None,
            last_start_config: None,
            last_heartbeat_at: None,
            restart_attempt: None,
            recovery_reason: None,
            platform_error: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("desiredRunning"), "should be absent: {json}");
        assert!(!json.contains("nativeState"), "should be absent: {json}");
        assert!(!json.contains("platformMode"), "should be absent: {json}");
        assert!(
            !json.contains("lastStartConfig"),
            "should be absent: {json}"
        );
        assert!(
            !json.contains("lastHeartbeatAt"),
            "should be absent: {json}"
        );
        assert!(!json.contains("restartAttempt"), "should be absent: {json}");
        assert!(!json.contains("recoveryReason"), "should be absent: {json}");
        assert!(!json.contains("platformError"), "should be absent: {json}");
    }

    #[test]
    fn service_status_default_impl() {
        let status = ServiceStatus::default();
        assert_eq!(status.state, ServiceState::Idle);
        assert_eq!(status.last_error, None);
        assert_eq!(status.desired_running, None);
        assert_eq!(status.native_state, None);
        assert_eq!(status.platform_mode, None);
        assert_eq!(status.last_start_config, None);
        assert_eq!(status.last_heartbeat_at, None);
        assert_eq!(status.restart_attempt, None);
        assert_eq!(status.recovery_reason, None);
        assert_eq!(status.platform_error, None);
    }

    #[test]
    fn service_status_full_roundtrip_with_all_fields() {
        let status = ServiceStatus {
            state: ServiceState::Running,
            last_error: Some("previous crash".into()),
            desired_running: Some(true),
            native_state: Some(NativeState::Recovering),
            platform_mode: Some(LifecycleMode::IosBgTaskScheduler),
            last_start_config: Some(StartConfig {
                service_label: "Sync".into(),
                foreground_service_type: "dataSync".into(),
            }),
            last_heartbeat_at: Some(999),
            restart_attempt: Some(3),
            recovery_reason: Some("force stop".into()),
            platform_error: Some("scheduler busy".into()),
        };
        let json = serde_json::to_string(&status).unwrap();
        let de: ServiceStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(de.state, ServiceState::Running);
        assert_eq!(de.last_error, Some("previous crash".into()));
        assert_eq!(de.desired_running, Some(true));
        assert_eq!(de.native_state, Some(NativeState::Recovering));
        assert_eq!(de.platform_mode, Some(LifecycleMode::IosBgTaskScheduler));
        assert!(de.last_start_config.is_some());
        assert_eq!(de.last_heartbeat_at, Some(999));
        assert_eq!(de.restart_attempt, Some(3));
        assert_eq!(de.recovery_reason, Some("force stop".into()));
        assert_eq!(de.platform_error, Some("scheduler busy".into()));
    }

    #[test]
    fn platform_capabilities_deserialize_from_json() {
        let json = r#"{
            "platform":"ios",
            "lifecycleMode":"iosBgTaskScheduler",
            "survivesAppClose":"bestEffort",
            "survivesReboot":"bestEffort",
            "survivesForceQuit":"unsupported",
            "backgroundExecution":"bestEffort",
            "limitations":["Cannot guarantee continuous execution"],
            "requiredSetup":["UIBackgroundModes in Info.plist"]
        }"#;
        let caps: PlatformCapabilities = serde_json::from_str(json).unwrap();
        assert_eq!(caps.platform, Platform::Ios);
        assert_eq!(caps.lifecycle_mode, LifecycleMode::IosBgTaskScheduler);
        assert_eq!(caps.survives_app_close, LifecycleGuarantee::BestEffort);
        assert_eq!(caps.background_execution, LifecycleGuarantee::BestEffort);
        assert_eq!(caps.limitations.len(), 1);
        assert_eq!(caps.required_setup.len(), 1);
    }

    // --- IOSSchedulingStatus tests ---

    #[test]
    fn ios_scheduling_status_both_scheduled() {
        let json = r#"{"refreshScheduled":true,"processingScheduled":true}"#;
        let status: IOSSchedulingStatus = serde_json::from_str(json).unwrap();
        assert!(status.refresh_scheduled);
        assert!(status.processing_scheduled);
        assert_eq!(status.refresh_error, None);
        assert_eq!(status.processing_error, None);
    }

    #[test]
    fn ios_scheduling_status_partial_success() {
        let json = r#"{"refreshScheduled":true,"processingScheduled":false,"processingError":"not permitted"}"#;
        let status: IOSSchedulingStatus = serde_json::from_str(json).unwrap();
        assert!(status.refresh_scheduled);
        assert!(!status.processing_scheduled);
        assert_eq!(status.refresh_error, None);
        assert_eq!(status.processing_error, Some("not permitted".to_string()));
    }

    #[test]
    fn ios_scheduling_status_with_errors() {
        let json = r#"{"refreshScheduled":false,"processingScheduled":false,"refreshError":"err1","processingError":"err2"}"#;
        let status: IOSSchedulingStatus = serde_json::from_str(json).unwrap();
        assert!(!status.refresh_scheduled);
        assert!(!status.processing_scheduled);
        assert_eq!(status.refresh_error, Some("err1".to_string()));
        assert_eq!(status.processing_error, Some("err2".to_string()));
    }

    #[test]
    fn ios_scheduling_status_serde_roundtrip() {
        let status = IOSSchedulingStatus {
            refresh_scheduled: true,
            processing_scheduled: false,
            refresh_error: None,
            processing_error: Some("busy".into()),
        };
        let json = serde_json::to_string(&status).unwrap();
        let de: IOSSchedulingStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(de, status);
    }

    #[test]
    fn ios_scheduling_status_json_keys_camel_case() {
        let status = IOSSchedulingStatus {
            refresh_scheduled: true,
            processing_scheduled: true,
            refresh_error: Some("err".into()),
            processing_error: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"refreshScheduled\":"), "{json}");
        assert!(json.contains("\"processingScheduled\":"), "{json}");
        assert!(json.contains("\"refreshError\":"), "{json}");
        assert!(
            !json.contains("processingError"),
            "None fields should be absent: {json}"
        );
    }

    #[test]
    fn ios_scheduling_status_from_value_null_errors() {
        // Simulates the Swift response where errors are NSNull()
        let json = r#"{"refreshScheduled":true,"processingScheduled":true,"refreshError":null,"processingError":null}"#;
        let status: IOSSchedulingStatus = serde_json::from_str(json).unwrap();
        assert!(status.refresh_scheduled);
        assert!(status.processing_scheduled);
        assert_eq!(status.refresh_error, None);
        assert_eq!(status.processing_error, None);
    }

    #[test]
    fn ios_scheduling_status_from_value_missing_errors() {
        // Swift may not include error fields when scheduling succeeds
        let json = r#"{"refreshScheduled":true,"processingScheduled":true}"#;
        let status: IOSSchedulingStatus = serde_json::from_str(json).unwrap();
        assert!(status.refresh_scheduled);
        assert!(status.processing_scheduled);
        assert_eq!(status.refresh_error, None);
        assert_eq!(status.processing_error, None);
    }

    // --- OsServiceInstallState tests ---

    #[test]
    fn os_service_install_state_serde_roundtrip() {
        for variant in [
            OsServiceInstallState::NotInstalled,
            OsServiceInstallState::Installed,
            OsServiceInstallState::Running,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let de: OsServiceInstallState = serde_json::from_str(&json).unwrap();
            assert_eq!(de, variant, "roundtrip failed for {variant:?}");
        }
    }

    #[test]
    fn os_service_install_state_json_values_camel_case() {
        assert_eq!(
            serde_json::to_string(&OsServiceInstallState::NotInstalled).unwrap(),
            "\"notInstalled\""
        );
        assert_eq!(
            serde_json::to_string(&OsServiceInstallState::Installed).unwrap(),
            "\"installed\""
        );
        assert_eq!(
            serde_json::to_string(&OsServiceInstallState::Running).unwrap(),
            "\"running\""
        );
    }

    // --- OsServiceStatus tests ---

    #[test]
    fn os_service_status_serde_roundtrip() {
        let status = OsServiceStatus {
            label: "com.example.bg-service".into(),
            mode: "systemd".into(),
            installed: OsServiceInstallState::Running,
            ipc_connected: true,
            socket_path: Some("/tmp/test.sock".into()),
            last_error: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        let de: OsServiceStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(de.label, "com.example.bg-service");
        assert_eq!(de.mode, "systemd");
        assert_eq!(de.installed, OsServiceInstallState::Running);
        assert!(de.ipc_connected);
        assert_eq!(de.socket_path, Some("/tmp/test.sock".into()));
        assert_eq!(de.last_error, None);
    }

    #[test]
    fn os_service_status_json_keys_camel_case() {
        let status = OsServiceStatus {
            label: "test".into(),
            mode: "launchd".into(),
            installed: OsServiceInstallState::Installed,
            ipc_connected: false,
            socket_path: Some("/run/test.sock".into()),
            last_error: Some("timeout".into()),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"label\":"), "{json}");
        assert!(json.contains("\"mode\":"), "{json}");
        assert!(json.contains("\"installed\":"), "{json}");
        assert!(json.contains("\"ipcConnected\":"), "{json}");
        assert!(json.contains("\"socketPath\":"), "{json}");
        assert!(json.contains("\"lastError\":"), "{json}");
    }

    #[test]
    fn os_service_status_optional_fields_absent_when_none() {
        let status = OsServiceStatus {
            label: "test".into(),
            mode: "systemd".into(),
            installed: OsServiceInstallState::NotInstalled,
            ipc_connected: false,
            socket_path: None,
            last_error: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("socketPath"), "should be absent: {json}");
        assert!(!json.contains("lastError"), "should be absent: {json}");
    }

    #[test]
    fn os_service_status_with_all_optional_fields() {
        let status = OsServiceStatus {
            label: "com.test".into(),
            mode: "launchd".into(),
            installed: OsServiceInstallState::Running,
            ipc_connected: true,
            socket_path: Some("/var/run/com.test.sock".into()),
            last_error: Some("connection refused".into()),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            json.contains("\"socketPath\":\"/var/run/com.test.sock\""),
            "{json}"
        );
        assert!(
            json.contains("\"lastError\":\"connection refused\""),
            "{json}"
        );
    }

    #[test]
    fn os_service_status_deserialize_from_json() {
        let json = r#"{
            "label":"com.example.svc",
            "mode":"systemd",
            "installed":"running",
            "ipcConnected":true,
            "socketPath":"/tmp/test.sock"
        }"#;
        let status: OsServiceStatus = serde_json::from_str(json).unwrap();
        assert_eq!(status.label, "com.example.svc");
        assert_eq!(status.mode, "systemd");
        assert_eq!(status.installed, OsServiceInstallState::Running);
        assert!(status.ipc_connected);
        assert_eq!(status.socket_path, Some("/tmp/test.sock".into()));
        assert_eq!(status.last_error, None);
    }

    // --- PendingTaskInfo tests ---

    #[test]
    fn pending_task_info_serde_roundtrip() {
        let info = PendingTaskInfo {
            task_kind: "refresh".into(),
            identifier: "com.example.app.bg-refresh".into(),
            received_at: 1700000000.123,
            consumed_at: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        let de: PendingTaskInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(de, info);
    }

    #[test]
    fn pending_task_info_json_keys_camel_case() {
        let info = PendingTaskInfo {
            task_kind: "processing".into(),
            identifier: "test-id".into(),
            received_at: 123456.0,
            consumed_at: Some(123500.0),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"taskKind\":"), "{json}");
        assert!(json.contains("\"identifier\":"), "{json}");
        assert!(json.contains("\"receivedAt\":"), "{json}");
        assert!(json.contains("\"consumedAt\":"), "{json}");
    }

    #[test]
    fn pending_task_info_from_native_response() {
        // Simulates the Swift response when a pending task exists
        let json = r#"{"taskKind":"refresh","identifier":"com.example.bg-refresh","receivedAt":1700000000.456}"#;
        let info: PendingTaskInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.task_kind, "refresh");
        assert_eq!(info.identifier, "com.example.bg-refresh");
        assert!((info.received_at - 1700000000.456).abs() < f64::EPSILON);
        assert_eq!(info.consumed_at, None);
    }

    #[test]
    fn pending_task_info_processing_kind() {
        let json = r#"{"taskKind":"processing","identifier":"com.example.bg-processing","receivedAt":1700000000.0}"#;
        let info: PendingTaskInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.task_kind, "processing");
        assert_eq!(info.identifier, "com.example.bg-processing");
        assert_eq!(info.consumed_at, None);
    }

    #[test]
    fn pending_task_info_consumed_at_roundtrip() {
        let info = PendingTaskInfo {
            task_kind: "refresh".into(),
            identifier: "com.example.bg-refresh".into(),
            received_at: 1700000000.0,
            consumed_at: Some(1700000060.5),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"consumedAt\":1700000060.5"), "{json}");
        let de: PendingTaskInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(de.consumed_at, Some(1700000060.5));
    }

    #[test]
    fn pending_task_info_consumed_at_null_deserializes_to_none() {
        let json = r#"{"taskKind":"refresh","identifier":"id","receivedAt":1.0,"consumedAt":null}"#;
        let info: PendingTaskInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.consumed_at, None);
    }

    // --- LifecycleState tests ---

    #[test]
    fn lifecycle_state_all_variants_serde_roundtrip() {
        for variant in [
            LifecycleState::Idle,
            LifecycleState::Starting,
            LifecycleState::Running,
            LifecycleState::Stopping,
            LifecycleState::Stopped,
            LifecycleState::Recovering,
            LifecycleState::RecoveryPending,
            LifecycleState::Expired,
            LifecycleState::Blocked,
            LifecycleState::Error,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let de: LifecycleState = serde_json::from_str(&json).unwrap();
            assert_eq!(de, variant, "roundtrip failed for {variant:?}");
        }
    }

    #[test]
    fn lifecycle_state_json_values_are_camel_case() {
        assert_eq!(
            serde_json::to_string(&LifecycleState::Idle).unwrap(),
            "\"idle\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleState::Starting).unwrap(),
            "\"starting\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleState::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleState::Stopping).unwrap(),
            "\"stopping\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleState::Stopped).unwrap(),
            "\"stopped\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleState::Recovering).unwrap(),
            "\"recovering\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleState::RecoveryPending).unwrap(),
            "\"recoveryPending\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleState::Expired).unwrap(),
            "\"expired\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleState::Blocked).unwrap(),
            "\"blocked\""
        );
        assert_eq!(
            serde_json::to_string(&LifecycleState::Error).unwrap(),
            "\"error\""
        );
    }

    // --- ServiceState → LifecycleState computation tests ---

    #[test]
    fn service_state_idle_maps_to_lifecycle_idle() {
        assert_eq!(
            LifecycleState::from(ServiceState::Idle),
            LifecycleState::Idle
        );
    }

    #[test]
    fn service_state_initializing_maps_to_lifecycle_starting() {
        assert_eq!(
            LifecycleState::from(ServiceState::Initializing),
            LifecycleState::Starting
        );
    }

    #[test]
    fn service_state_running_maps_to_lifecycle_running() {
        assert_eq!(
            LifecycleState::from(ServiceState::Running),
            LifecycleState::Running
        );
    }

    #[test]
    fn service_state_stopped_maps_to_lifecycle_stopped() {
        assert_eq!(
            LifecycleState::from(ServiceState::Stopped),
            LifecycleState::Stopped
        );
    }

    // --- Severity tests ---

    #[test]
    fn severity_all_variants_serde_roundtrip() {
        for variant in [Severity::Error, Severity::Warning, Severity::Info] {
            let json = serde_json::to_string(&variant).unwrap();
            let de: Severity = serde_json::from_str(&json).unwrap();
            assert_eq!(de, variant, "roundtrip failed for {variant:?}");
        }
    }

    #[test]
    fn severity_json_values_are_camel_case() {
        assert_eq!(
            serde_json::to_string(&Severity::Error).unwrap(),
            "\"error\""
        );
        assert_eq!(
            serde_json::to_string(&Severity::Warning).unwrap(),
            "\"warning\""
        );
        assert_eq!(serde_json::to_string(&Severity::Info).unwrap(), "\"info\"");
    }

    // --- ValidationIssue tests ---

    #[test]
    fn validation_issue_serde_roundtrip() {
        let issue = ValidationIssue {
            severity: Severity::Error,
            code: "ANDROID_MISSING_PERMISSION".into(),
            message: "Missing FOREGROUND_SERVICE permission".into(),
            fix: Some("Add FOREGROUND_SERVICE permission to AndroidManifest.xml".into()),
            platform: Platform::Android,
        };
        let json = serde_json::to_string(&issue).unwrap();
        let de: ValidationIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(de, issue);
    }

    #[test]
    fn validation_issue_without_fix() {
        let issue = ValidationIssue {
            severity: Severity::Warning,
            code: "IOS_SCHEDULER_BUSY".into(),
            message: "BGTaskScheduler is busy".into(),
            fix: None,
            platform: Platform::Ios,
        };
        let json = serde_json::to_string(&issue).unwrap();
        assert!(
            !json.contains("fix"),
            "fix should be absent when None: {json}"
        );
        let de: ValidationIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(de, issue);
    }

    #[test]
    fn validation_issue_json_keys_camel_case() {
        let issue = ValidationIssue {
            severity: Severity::Info,
            code: "TEST".into(),
            message: "test".into(),
            fix: Some("do something".into()),
            platform: Platform::Linux,
        };
        let json = serde_json::to_string(&issue).unwrap();
        assert!(json.contains("\"severity\":"), "{json}");
        assert!(json.contains("\"code\":"), "{json}");
        assert!(json.contains("\"message\":"), "{json}");
        assert!(json.contains("\"fix\":"), "{json}");
        assert!(json.contains("\"platform\":"), "{json}");
    }

    // --- LifecycleStatus tests ---

    #[test]
    fn lifecycle_status_serde_roundtrip_minimal() {
        let status = LifecycleStatus {
            state: LifecycleState::Idle,
            desired_running: false,
            recovery_enabled: false,
            recovery_pending: false,
            recovery_reason: None,
            last_start_config: None,
            last_platform_state: None,
            last_platform_error: None,
            last_error: None,
            platform: Platform::Unknown,
            capabilities: PlatformCapabilities {
                platform: Platform::Unknown,
                lifecycle_mode: LifecycleMode::DesktopInProcess,
                survives_app_close: LifecycleGuarantee::Unsupported,
                survives_reboot: LifecycleGuarantee::Unsupported,
                survives_force_quit: LifecycleGuarantee::Unsupported,
                background_execution: LifecycleGuarantee::Unsupported,
                limitations: vec![],
                required_setup: vec![],
            },
            issues: vec![],
        };
        let json = serde_json::to_string(&status).unwrap();
        let de: LifecycleStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(de.state, LifecycleState::Idle);
        assert!(!de.desired_running);
        assert!(!de.recovery_enabled);
        assert!(!de.recovery_pending);
        assert_eq!(de.recovery_reason, None);
        assert_eq!(de.last_start_config, None);
        assert_eq!(de.last_platform_state, None);
        assert_eq!(de.last_platform_error, None);
        assert_eq!(de.last_error, None);
        assert_eq!(de.platform, Platform::Unknown);
        assert!(de.issues.is_empty());
    }

    #[test]
    fn lifecycle_status_optional_fields_absent_when_none() {
        let status = LifecycleStatus {
            state: LifecycleState::Idle,
            desired_running: false,
            recovery_enabled: false,
            recovery_pending: false,
            recovery_reason: None,
            last_start_config: None,
            last_platform_state: None,
            last_platform_error: None,
            last_error: None,
            platform: Platform::Unknown,
            capabilities: PlatformCapabilities {
                platform: Platform::Unknown,
                lifecycle_mode: LifecycleMode::DesktopInProcess,
                survives_app_close: LifecycleGuarantee::Unsupported,
                survives_reboot: LifecycleGuarantee::Unsupported,
                survives_force_quit: LifecycleGuarantee::Unsupported,
                background_execution: LifecycleGuarantee::Unsupported,
                limitations: vec![],
                required_setup: vec![],
            },
            issues: vec![],
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("recoveryReason"), "should be absent: {json}");
        assert!(
            !json.contains("lastStartConfig"),
            "should be absent: {json}"
        );
        assert!(
            !json.contains("lastPlatformState"),
            "should be absent: {json}"
        );
        assert!(
            !json.contains("lastPlatformError"),
            "should be absent: {json}"
        );
        assert!(!json.contains("lastError"), "should be absent: {json}");
    }

    #[test]
    fn lifecycle_status_full_roundtrip_with_all_fields() {
        let status = LifecycleStatus {
            state: LifecycleState::Running,
            desired_running: true,
            recovery_enabled: true,
            recovery_pending: false,
            recovery_reason: Some("boot recovery".into()),
            last_start_config: Some(StartConfig {
                service_label: "Sync".into(),
                foreground_service_type: "dataSync".into(),
            }),
            last_platform_state: Some("running".into()),
            last_platform_error: Some("timeout exceeded".into()),
            last_error: Some("previous crash".into()),
            platform: Platform::Android,
            capabilities: PlatformCapabilities {
                platform: Platform::Android,
                lifecycle_mode: LifecycleMode::AndroidForegroundService,
                survives_app_close: LifecycleGuarantee::BestEffort,
                survives_reboot: LifecycleGuarantee::BestEffort,
                survives_force_quit: LifecycleGuarantee::Unsupported,
                background_execution: LifecycleGuarantee::Guaranteed,
                limitations: vec!["OEM battery optimization".into()],
                required_setup: vec!["FOREGROUND_SERVICE permission".into()],
            },
            issues: vec![ValidationIssue {
                severity: Severity::Warning,
                code: "ANDROID_BATTERY_OPTIMIZED".into(),
                message: "Battery optimization may kill the service".into(),
                fix: Some("Request REQUEST_IGNORE_BATTERY_OPTIMIZATIONS".into()),
                platform: Platform::Android,
            }],
        };
        let json = serde_json::to_string(&status).unwrap();
        let de: LifecycleStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(de.state, LifecycleState::Running);
        assert!(de.desired_running);
        assert!(de.recovery_enabled);
        assert!(!de.recovery_pending);
        assert_eq!(de.recovery_reason, Some("boot recovery".into()));
        assert!(de.last_start_config.is_some());
        assert_eq!(de.last_platform_state, Some("running".into()));
        assert_eq!(de.last_platform_error, Some("timeout exceeded".into()));
        assert_eq!(de.last_error, Some("previous crash".into()));
        assert_eq!(de.platform, Platform::Android);
        assert_eq!(de.issues.len(), 1);
    }

    #[test]
    fn lifecycle_status_json_keys_camel_case() {
        let status = LifecycleStatus {
            state: LifecycleState::RecoveryPending,
            desired_running: true,
            recovery_enabled: true,
            recovery_pending: true,
            recovery_reason: Some("platform timeout".into()),
            last_start_config: None,
            last_platform_state: Some("timeout".into()),
            last_platform_error: None,
            last_error: None,
            platform: Platform::Ios,
            capabilities: PlatformCapabilities {
                platform: Platform::Ios,
                lifecycle_mode: LifecycleMode::IosBgTaskScheduler,
                survives_app_close: LifecycleGuarantee::BestEffort,
                survives_reboot: LifecycleGuarantee::BestEffort,
                survives_force_quit: LifecycleGuarantee::Unsupported,
                background_execution: LifecycleGuarantee::BestEffort,
                limitations: vec![],
                required_setup: vec![],
            },
            issues: vec![],
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"state\":"), "{json}");
        assert!(json.contains("\"desiredRunning\":"), "{json}");
        assert!(json.contains("\"recoveryEnabled\":"), "{json}");
        assert!(json.contains("\"recoveryPending\":"), "{json}");
        assert!(json.contains("\"recoveryReason\":"), "{json}");
        assert!(json.contains("\"lastPlatformState\":"), "{json}");
        assert!(json.contains("\"platform\":"), "{json}");
        assert!(json.contains("\"capabilities\":"), "{json}");
        assert!(json.contains("\"issues\":"), "{json}");
    }
}
