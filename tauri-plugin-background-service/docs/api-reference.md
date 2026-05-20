# API Reference

Complete reference for the Rust and TypeScript APIs provided by `tauri-plugin-background-service`.

---

## Rust API

### `BackgroundService<R>`

The trait you implement to define a background service. Uses [`#[async_trait]`](https://docs.rs/async-trait) for object safety, enabling the factory pattern: `Box<dyn BackgroundService<R>>`.

```rust
#[async_trait]
pub trait BackgroundService<R: Runtime>: Send + 'static {
    async fn init(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError>;
    async fn run(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError>;
}
```

#### Methods

| Method | Parameters | Returns | Description |
|--------|-----------|---------|-------------|
| `init` | `ctx: &ServiceContext<R>` | `Result<(), ServiceError>` | Called once before `run`. Use for setup that requires the Tauri context (e.g. opening database connections, registering event listeners). |
| `run` | `ctx: &ServiceContext<R>` | `Result<(), ServiceError>` | The main service loop. Must use `tokio::select!` with `ctx.shutdown.cancelled()` for cooperative cancellation. |

#### Object Safety

The trait is object-safe thanks to `#[async_trait]`. This allows the plugin to store and invoke services through `Box<dyn BackgroundService<R>>`. Do **not** add generic methods or associated types that would break `Box<dyn>` compatibility.

#### Example

```rust
use async_trait::async_trait;
use tauri::Runtime;
use tauri_plugin_background_service::{
    BackgroundService, ServiceContext, ServiceError,
};

struct MyService;

#[async_trait]
impl<R: Runtime> BackgroundService<R> for MyService {
    async fn init(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        // One-time setup (open DB, register listeners, etc.)
        Ok(())
    }

    async fn run(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        loop {
            tokio::select! {
                _ = ctx.shutdown.cancelled() => {
                    // Cooperative shutdown — clean up and return
                    break;
                }
                _ = do_work() => {
                    // Your background work here
                }
            }
        }
        Ok(())
    }
}
```

---

### `ServiceContext<R>`

Passed into both `init()` and `run()`. Provides everything your service needs to interact with the outside world.

```rust
pub struct ServiceContext<R: Runtime> {
    pub notifier: Notifier<R>,
    pub app: tauri::AppHandle<R>,
    pub shutdown: CancellationToken,
    #[cfg(mobile)]
    pub service_label: String,
    #[cfg(mobile)]
    pub foreground_service_type: String,
}
```

#### Fields

| Field | Type | Platforms | Description |
|-------|------|-----------|-------------|
| `notifier` | `Notifier<R>` | All | Fire a local notification. Works on all platforms. |
| `app` | `tauri::AppHandle<R>` | All | Emit events to the JS UI layer, access managed state. |
| `shutdown` | `CancellationToken` | All | Cancelled when `stopService()` is called. Always use in `tokio::select!` within `run()`. |
| `service_label` | `String` | Mobile only | Text shown in the Android persistent notification. Uses the `StartConfig` default (`"Service running"`) if not overridden. |
| `foreground_service_type` | `String` | Mobile only | Android foreground service type (e.g. `"dataSync"`, `"specialUse"`). Uses the `StartConfig` default (`"dataSync"`) if not overridden. |

> **Platform behavior:** `service_label` and `foreground_service_type` are `String` (not `Option<String>`) and only available on mobile platforms, guarded by `#[cfg(mobile)]`. They always contain a value because `StartConfig` provides defaults.

---

### `StartConfig`

Optional startup configuration forwarded from JavaScript through the plugin. Serialized as camelCase JSON.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartConfig {
    pub service_label: String,
    pub foreground_service_type: String,
}
```

#### Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `service_label` | `String` | Optional | `"Service running"` | Text shown in the Android persistent foreground notification. Ignored on desktop. |
| `foreground_service_type` | `String` | Optional | `"dataSync"` | Android foreground service type. Valid values: `"dataSync"`, `"mediaPlayback"`, `"phoneCall"`, `"location"`, `"connectedDevice"`, `"mediaProjection"`, `"camera"`, `"microphone"`, `"health"`, `"remoteMessaging"`, `"systemExempted"`, `"shortService"`, `"specialUse"`, `"mediaProcessing"`. Ignored on non-Android platforms. |

#### JSON format

```json
{
  "serviceLabel": "Syncing data",
  "foregroundServiceType": "dataSync"
}
```

All fields have defaults — an empty `{}` is valid and uses all defaults.

---

### `PluginConfig`

Plugin-level configuration, deserialized from the Tauri plugin config in `tauri.conf.json`. Controls iOS-specific timing parameters, Android foreground service type validation, Android timeout behavior, Android notification customization, and desktop service mode.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginConfig {
    pub ios_safety_timeout_secs: f64,
    pub ios_cancel_listener_timeout_secs: u64,
    pub ios_processing_safety_timeout_secs: f64,
    pub ios_earliest_refresh_begin_minutes: f64,
    pub ios_earliest_processing_begin_minutes: f64,
    pub ios_requires_external_power: bool,
    pub ios_requires_network_connectivity: bool,
    pub android_foreground_service_types: Vec<String>,
    pub android_validate_foreground_service_type: bool,
    pub android_on_timeout: String,
    pub android_notification_channel_id: String,
    pub android_notification_channel_name: String,
    pub android_notification_id: u32,
    pub android_notification_small_icon: Option<String>,
    pub android_show_stop_action: bool,
    pub android_request_notification_permission_on_load: bool,
    // Behind #[cfg(feature = "desktop-service")]:
    // pub desktop_service_mode: String,
    // pub desktop_service_label: Option<String>,
}
```

#### Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `ios_safety_timeout_secs` | `f64` | Optional | `28.0` | iOS safety timeout for the BGAppRefreshTask expiration handler. iOS only. |
| `ios_cancel_listener_timeout_secs` | `u64` | Optional | `14400` | iOS cancel listener timeout in seconds (4 hours). iOS only. |
| `ios_processing_safety_timeout_secs` | `f64` | Optional | `0.0` | iOS safety timeout for BGProcessingTask. `0.0` means no cap (iOS manages lifetime). iOS only. |
| `ios_earliest_refresh_begin_minutes` | `f64` | Optional | `15.0` | Minimum delay (in minutes) before iOS schedules a `BGAppRefreshTask`. iOS only. |
| `ios_earliest_processing_begin_minutes` | `f64` | Optional | `15.0` | Minimum delay (in minutes) before iOS schedules a `BGProcessingTask`. iOS only. |
| `ios_requires_external_power` | `bool` | Optional | `false` | Whether `BGProcessingTask` requires the device to be charging. iOS only. |
| `ios_requires_network_connectivity` | `bool` | Optional | `false` | Whether `BGProcessingTask` requires network connectivity. iOS only. |
| `android_foreground_service_types` | `string[]` | Optional | `["dataSync"]` | List of Android foreground service types allowed for `startService()`. The Kotlin preflight validation rejects any type not in this list when `android_validate_foreground_service_type` is `true`. Android only. |
| `android_validate_foreground_service_type` | `bool` | Optional | `true` | Whether to validate the requested foreground service type against `android_foreground_service_types` before starting the native service. Set to `false` to skip the allowlist check. Android only. |
| `android_on_timeout` | `string` | Optional | `"notifyUser"` | Timeout policy when Android calls `onTimeout()`. Valid values: `"stop"` (clean stop), `"notifyUser"` (stop + timeout notification), `"scheduleRecovery"` (stop + recovery pending + recovery notification). Android only. |
| `android_notification_channel_id` | `string` | Optional | `"bg_service"` | Notification channel ID for the foreground service notification. The channel is created automatically. Android only. |
| `android_notification_channel_name` | `string` | Optional | `"Background Service"` | Notification channel name visible to the user in system settings. Android only. |
| `android_notification_id` | `number` | Optional | `9001` | Notification ID for the foreground service notification. Must be unique within your app. Android only. |
| `android_notification_small_icon` | `string?` | Optional | `null` (system default) | Custom small icon resource name (without extension). The resource must exist in `res/drawable/`. Falls back to the system sync icon if not found. Android only. |
| `android_show_stop_action` | `boolean` | Optional | `true` | Whether to show a "Stop" action button on the foreground notification. Android only. |
| `android_request_notification_permission_on_load` | `boolean` | Optional | `true` | Whether to automatically request the `POST_NOTIFICATIONS` runtime permission when the plugin loads. Set to `false` if your app handles permission requests manually. Android only. |
| `desktop_service_mode` | `String` | Optional | `"inProcess"` | Desktop service mode: `"inProcess"` (default) or `"osService"`. Desktop only, requires `desktop-service` feature. |
| `desktop_service_label` | `Option<String>` | Optional | Auto-derived | Custom label for the OS service. Desktop only, requires `desktop-service` feature. |
| `desktop_service_autostart` | `boolean` | Optional | `false` | Whether the OS service starts automatically on boot (Linux) or login (macOS). Only applies when `desktopServiceMode` is `"osService"`. Desktop only, requires `desktop-service` feature. |
| `desktop_start_service_if_missing` | `boolean` | Optional | `false` | When `true`, calling `startService()` automatically starts the OS service sidecar if the IPC connection is not available. Only applies when `desktopServiceMode` is `"osService"`. Desktop only, requires `desktop-service` feature. |
| `desktop_service_start_timeout_ms` | `number` | Optional | `5000` | Timeout in milliseconds to wait for the IPC connection after starting the OS service sidecar. Only applies when `desktopStartServiceIfMissing` is `true`. Desktop only, requires `desktop-service` feature. |

#### Configuration example

```json
{
  "plugins": {
    "background-service": {
      "iosSafetyTimeoutSecs": 25.0,
      "iosCancelListenerTimeoutSecs": 7200,
      "iosProcessingSafetyTimeoutSecs": 600,
      "iosEarliestRefreshBeginMinutes": 15.0,
      "iosEarliestProcessingBeginMinutes": 30.0,
      "iosRequiresExternalPower": true,
      "iosRequiresNetworkConnectivity": false,
      "androidForegroundServiceTypes": ["dataSync"],
      "androidValidateForegroundServiceType": true,
      "androidOnTimeout": "notifyUser",
      "androidNotificationChannelId": "my_service_channel",
      "androidNotificationChannelName": "My Background Service",
      "androidNotificationId": 9100,
      "androidNotificationSmallIcon": "ic_notification",
      "androidShowStopAction": true,
      "androidRequestNotificationPermissionOnLoad": true,
      "desktopServiceMode": "osService",
      "desktopServiceLabel": "com.example.myapp.background",
      "desktopServiceAutostart": true,
      "desktopStartServiceIfMissing": true,
      "desktopServiceStartTimeoutMs": 5000
    }
  }
}
```

---

### `ServiceError`

Error type returned by service operations. Marked `#[non_exhaustive]` — new variants may be added in future versions.

```rust
#[derive(Debug, thiserror::Error, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ServiceError {
    #[error("Service is already running")]
    AlreadyRunning,
    #[error("Service is not running")]
    NotRunning,
    #[error("Initialisation failed: {0}")]
    Init(String),
    #[error("Runtime error: {0}")]
    Runtime(String),
    #[error("Platform error: {0}")]
    Platform(String),
    #[cfg(mobile)]
    #[error("Plugin invoke error: {0}")]
    PluginInvoke(String),
    #[cfg(feature = "desktop-service")]
    #[error("Service installation failed: {0}")]
    ServiceInstall(String),
    #[cfg(feature = "desktop-service")]
    #[error("Service uninstallation failed: {0}")]
    ServiceUninstall(String),
    #[cfg(feature = "desktop-service")]
    #[error("IPC error: {0}")]
    Ipc(String),
}
```

#### Variants

| Variant | Payload | When it occurs |
|---------|---------|---------------|
| `AlreadyRunning` | — | `startService()` called while a service is already active. |
| `NotRunning` | — | `stopService()` called when no service is active. |
| `Init(String)` | Error message | `init()` returned an error. |
| `Runtime(String)` | Error message | `run()` returned an error, or the actor channel closed. |
| `Platform(String)` | Error message | OS-specific failure (e.g. Android foreground service denied, iOS BGTask rejected, mobile keepalive failure). |
| `PluginInvoke(String)` | Error message | Mobile plugin invoke failed (Kotlin/Swift bridge error). Mobile only, behind `#[cfg(mobile)]`. |
| `ServiceInstall(String)` | Error message | Desktop service installation failed. Requires `desktop-service` feature. |
| `ServiceUninstall(String)` | Error message | Desktop service uninstallation failed. Requires `desktop-service` feature. |
| `Ipc(String)` | Error message | Desktop IPC communication error (socket connection, framing). Requires `desktop-service` feature. |

> **Non-exhaustive:** Match with a wildcard `_` arm to handle future variants gracefully.

---

### `PluginEvent`

Built-in event types emitted by the plugin to the JS UI layer. Serialized as a tagged JSON enum with `"type"` as the tag. Marked `#[non_exhaustive]`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
#[non_exhaustive]
pub enum PluginEvent {
    Started,
    Stopped { reason: StopReason },
    Error { message: String },
}
```

#### Variants

| Variant | Payload | JSON shape | When emitted |
|---------|---------|-----------|-------------|
| `Started` | — | `{ "type": "started" }` | After `init()` completes successfully. |
| `Stopped` | `reason: StopReason` | `{ "type": "stopped", "reason": "taskCompleted" }` | When `run()` returns or is cancelled. The `reason` is a structured `StopReason` enum (see below). |
| `Error` | `message: String` | `{ "type": "error", "message": "..." }` | When `init()` or `run()` returns an error. |

> **Backward compatibility:** The `reason` field in `Stopped` changed from a plain `String` to `StopReason` in v0.7.0. Legacy string values like `"completed"` and `"cancelled"` still deserialize correctly via built-in mappings.

---

### `StopReason`

Structured reason why the background service stopped. Replaces the previous plain `String` reason in `PluginEvent::Stopped`. Marked `#[non_exhaustive]`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum StopReason {
    UserStop,
    AppStop,
    PlatformTimeout,
    PlatformExpiration,
    NativeNotificationStop,
    OsRestart,
    BootRecovery,
    TaskCompleted,
    Error,
}
```

#### Variants

| Variant | JSON value | When |
|---------|-----------|------|
| `UserStop` | `"userStop"` | User called `stopService()`. |
| `AppStop` | `"appStop"` | Application is shutting down gracefully. |
| `PlatformTimeout` | `"platformTimeout"` | Platform killed the service due to a timeout (e.g. Android FGS timeout). |
| `PlatformExpiration` | `"platformExpiration"` | Platform expired the background execution window (e.g. iOS BGTask). |
| `NativeNotificationStop` | `"nativeNotificationStop"` | User pressed stop on the native notification. |
| `OsRestart` | `"osRestart"` | OS restarted the service after a reboot. |
| `BootRecovery` | `"bootRecovery"` | Service recovered after device boot. |
| `TaskCompleted` | `"taskCompleted"` | Service's `run()` returned `Ok(())` naturally. |
| `Error` | `"error"` | Service's `run()` returned an error. |

> **Backward compatibility:** Legacy string values `"completed"`, `"cancelled"`, and `"user"` still deserialize to `TaskCompleted` and `UserStop` respectively.

---

### `NativeLifecycleEvent`

Events originating in the native layer (Kotlin/Swift) and forwarded to the Rust actor. Tagged JSON enum. Marked `#[non_exhaustive]`.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
#[non_exhaustive]
pub enum NativeLifecycleEvent {
    AndroidNotificationStop,
    AndroidTimeout { fgs_type: Option<String> },
}
```

#### Variants

| Variant | Payload | When |
|---------|---------|------|
| `AndroidNotificationStop` | — | User pressed stop on the Android foreground service notification. |
| `AndroidTimeout` | `fgs_type: Option<String>` | Android system killed the foreground service due to a timeout. |

---

### `LifecycleState`

Fine-grained lifecycle state with 10 states, providing more detail than `ServiceState` (which has 4). Marked `#[non_exhaustive]`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum LifecycleState {
    Idle, Starting, Running, Stopping, Stopped,
    Recovering, RecoveryPending, Expired, Blocked, Error,
}
```

#### Variants

| Variant | JSON value | Description |
|---------|-----------|-------------|
| `Idle` | `"idle"` | No service has been started. |
| `Starting` | `"starting"` | Service `init()` is in progress. |
| `Running` | `"running"` | Service `run()` is executing. |
| `Stopping` | `"stopping"` | Service is being stopped (cancellation requested). |
| `Stopped` | `"stopped"` | Service has stopped. |
| `Recovering` | `"recovering"` | Service is recovering after a platform timeout or expiration. |
| `RecoveryPending` | `"recoveryPending"` | Recovery is pending (waiting for platform conditions). |
| `Expired` | `"expired"` | Background execution window has expired (e.g. iOS BGTask). |
| `Blocked` | `"blocked"` | Service is blocked by a platform issue (e.g. missing permission). |
| `Error` | `"error"` | Service encountered an error. |

---

### `LifecycleStatus`

Complete snapshot of the background service lifecycle status. Returned by `get_lifecycle_status`. Marked `#[non_exhaustive]`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct LifecycleStatus {
    pub state: LifecycleState,
    pub desired_running: bool,
    pub recovery_enabled: bool,
    pub recovery_pending: bool,
    pub recovery_reason: Option<String>,
    pub last_start_config: Option<StartConfig>,
    pub last_platform_state: Option<String>,
    pub last_platform_error: Option<String>,
    pub last_error: Option<String>,
    pub platform: Platform,
    pub capabilities: PlatformCapabilities,
    pub issues: Vec<ValidationIssue>,
}
```

#### Fields

| Field | Type | Description |
|-------|------|-------------|
| `state` | `LifecycleState` | Current lifecycle state (10 possible states). |
| `desired_running` | `bool` | Whether the service is desired to be running. |
| `recovery_enabled` | `bool` | Whether auto-recovery is enabled. |
| `recovery_pending` | `bool` | Whether a recovery is currently pending. |
| `recovery_reason` | `Option<String>` | Human-readable reason for the current recovery. |
| `last_start_config` | `Option<StartConfig>` | Configuration used for the last successful start. |
| `last_platform_state` | `Option<String>` | Last platform-native state string. |
| `last_platform_error` | `Option<String>` | Last platform-specific error message. |
| `last_error` | `Option<String>` | Last error message from service execution. |
| `platform` | `Platform` | Current runtime platform. |
| `capabilities` | `PlatformCapabilities` | Platform-specific background execution capabilities. |
| `issues` | `Vec<ValidationIssue>` | Current validation issues with severity levels. |

---

### `Severity`

Severity level for validation issues. Marked `#[non_exhaustive]`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Severity {
    Error,
    Warning,
    Info,
}
```

| Variant | JSON value | Description |
|---------|-----------|-------------|
| `Error` | `"error"` | Blocking issue that prevents service from working. |
| `Warning` | `"warning"` | Non-blocking issue that may cause degraded behavior. |
| `Info` | `"info"` | Informational note. |

---

### `ValidationIssue`

A single validation issue with severity, code, message, and optional fix. Part of `LifecycleStatus.issues` and `SetupValidationReport.issues`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ValidationIssue {
    pub severity: Severity,
    pub code: String,
    pub message: String,
    pub fix: Option<String>,
    pub platform: Platform,
}
```

#### Fields

| Field | Type | Description |
|-------|------|-------------|
| `severity` | `Severity` | Issue severity level. |
| `code` | `String` | Machine-readable error code. |
| `message` | `String` | Human-readable description. |
| `fix` | `Option<String>` | Suggested fix, if available. |
| `platform` | `Platform` | The platform this issue applies to. |

---

### `Notifier<R>`

Thin wrapper over `tauri-plugin-notification`. Fire-and-forget: errors are logged via `log::warn!` and never propagated to callers.

```rust
#[derive(Clone)]
pub struct Notifier<R: Runtime> { /* ... */ }

impl<R: Runtime> Notifier<R> {
    pub fn show(&self, title: &str, body: &str) { /* ... */ }
}
```

#### Methods

| Method | Parameters | Returns | Description |
|--------|-----------|---------|-------------|
| `show` | `title: &str`, `body: &str` | `()` | Show a local notification. Errors are logged but not returned — callers should not need to handle notification failures. |

> **Prerequisite:** `tauri-plugin-notification` must be registered before the background service plugin.

#### Example

```rust
ctx.notifier.show("Sync Complete", "All data uploaded successfully");
```

---

### `ServiceState`

Enum representing the lifecycle state of the background service. Marked `#[non_exhaustive]`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ServiceState {
    Idle,
    Initializing,
    Running,
    Stopped,
}
```

#### Variants

| Variant | JSON value | Description |
|---------|-----------|-------------|
| `Idle` | `"idle"` | No service has been started, or the service has been stopped and fully cleaned up. |
| `Initializing` | `"initializing"` | `init()` is currently running. |
| `Running` | `"running"` | `run()` is actively executing. |
| `Stopped` | `"stopped"` | The service has stopped (completed, cancelled, or errored). |

---

### `NativeState`

Platform-native state as reported by the OS service layer (Android foreground service, iOS BGTask handler, or desktop OS-service process). This is distinct from the plugin-internal `ServiceState` — it reflects what the OS observes, not what the plugin actor knows.

Marked `#[non_exhaustive]` — new variants may be added in future versions.

```rust
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
```

#### Variants

| Variant | JSON value | Description |
|---------|-----------|-------------|
| `Idle` | `"idle"` | No native service activity. |
| `Starting` | `"starting"` | The OS service layer is starting up (e.g. Android foreground service `onCreate`). |
| `Running` | `"running"` | The OS service layer reports active execution. |
| `Stopping` | `"stopping"` | The OS service layer is shutting down (e.g. Android `onDestroy`, iOS expiration handler triggered). |
| `Timeout` | `"timeout"` | The OS has signalled a timeout (e.g. Android 15 `dataSync` 6-hour cumulative limit). |
| `Expired` | `"expired"` | The OS task has expired (e.g. iOS BGTask expired before completion). |
| `Recovering` | `"recovering"` | The plugin is attempting recovery (e.g. boot recovery, auto-restart). |
| `Error` | `"error"` | The OS service layer reported an error. |

> **Non-exhaustive:** Match with a wildcard `_` arm to handle future variants gracefully.

---

### `ServiceStatus`

Snapshot of the service lifecycle status, returned by the `get_service_state` command. The first two fields (`state`, `last_error`) are always present. The remaining optional fields are populated from the desired-state backend when available and omitted from JSON when `None`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub state: ServiceState,
    pub last_error: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_running: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_state: Option<NativeState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_mode: Option<LifecycleMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_start_config: Option<StartConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_attempt: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_error: Option<String>,
}
```

#### Fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `state` | `ServiceState` | Yes | Current lifecycle state of the service. |
| `last_error` | `Option<String>` | Yes | Error message from the last failure, if any. `None` if no error has occurred. |
| `desired_running` | `Option<bool>` | No | Whether the service is desired to be running, as persisted in the desired-state backend. `true` after `startService()`, `false` after `stopService()`. |
| `native_state` | `Option<NativeState>` | No | Platform-native state as reported by the OS service layer. Reflects the OS perspective rather than the plugin-internal `ServiceState`. |
| `platform_mode` | `Option<LifecycleMode>` | No | The lifecycle mechanism in use on the current platform (e.g. `AndroidForegroundService`, `DesktopInProcess`). |
| `last_start_config` | `Option<StartConfig>` | No | Configuration used for the last successful `startService()` call. |
| `last_heartbeat_at` | `Option<u64>` | No | Epoch milliseconds of the last heartbeat received from the service. |
| `restart_attempt` | `Option<u32>` | No | How many restart attempts have been made since the last clean start. `None` when zero. |
| `recovery_reason` | `Option<String>` | No | Human-readable reason for the current recovery attempt (e.g. `"boot recovery"`, `"process killed"`). |
| `platform_error` | `Option<String>` | No | Last platform-specific error message (e.g. timeout detail, scheduler failure). |

> **Backward compatibility:** Old JSON containing only `state` and `lastError` deserializes correctly — all new fields default to `None`. New fields are omitted from JSON when `None` (`#[serde(skip_serializing_if)]`), so old clients are unaffected.

#### Platform field coverage

Not all fields are populated on every platform. The table below shows which fields are typically available:

| Field | Android | iOS | Desktop (in-process) | Desktop (OS service) |
|-------|---------|-----|---------------------|---------------------|
| `state` | Always | Always | Always | Always |
| `last_error` | Always | Always | Always | Always |
| `desired_running` | After start/stop | After start/stop | After start/stop | After start/stop |
| `native_state` | From FGS lifecycle | From BGTask handler | Not populated | From OS service |
| `platform_mode` | `androidForegroundService` | `iosBgTaskScheduler` | `desktopInProcess` | `desktopOsService` |
| `last_start_config` | After start | After start | After start | After start |
| `last_heartbeat_at` | When heartbeats active | When heartbeats active | When heartbeats active | When heartbeats active |
| `restart_attempt` | During recovery | During recovery | Not populated | During recovery |
| `recovery_reason` | During recovery | During recovery | Not populated | During recovery |
| `platform_error` | On OS error | On OS error | Not populated | On OS error |

---

### `get_service_state` command

Tauri command that queries the current service state. Exposed as `getServiceState()` in TypeScript.

```rust
#[tauri::command]
pub async fn get_service_state(
    state: tauri::State<'_, ServiceManagerHandle<R>>,
) -> Result<ServiceStatus, String>
```

#### Returns

`Result<ServiceStatus, String>` — the current service state, optional last error, and extended status fields populated from the desired-state backend.

---

### `init_with_service(factory)`

Creates the Tauri plugin with your service factory. This is the main entry point for registering the plugin.

```rust
pub fn init_with_service<R, S, F>(factory: F) -> TauriPlugin<R, PluginConfig>
where
    R: Runtime,
    S: BackgroundService<R>,
    F: Fn() -> S + Send + Sync + 'static,
```

#### Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `factory` | `F` where `F: Fn() -> S + Send + Sync + 'static` | Required | A zero-argument closure that produces a fresh `BackgroundService` instance. Called once per `startService()` invocation. |

#### Returns

`TauriPlugin<R, PluginConfig>` — pass this to `tauri::Builder::plugin()`.

#### Factory pattern

The factory creates a fresh service instance on each `startService()` call. This ensures clean state after stop-start cycles. The closure captures no mutable state — it only produces new instances.

#### Example

```rust
tauri::Builder::default()
    .plugin(tauri_plugin_notification::init())
    .plugin(tauri_plugin_background_service::init_with_service(|| MyService::new()))
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
```

> **Order matters:** Register `tauri-plugin-notification` **before** the background service plugin, because `Notifier` depends on it.

---

### `AutoStartConfig`

Platform-specific type used for Android auto-start. Deserialized from SharedPreferences values read by the Kotlin `getAutoStartConfig` bridge. Only used on Android.

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoStartConfig {
    pub pending: bool,
    pub label: Option<String>,
    pub service_type: Option<String>,
}
```

#### Fields

| Field | Type | Description |
|-------|------|-------------|
| `pending` | `bool` | Whether an auto-start is pending (set by `LifecycleService` after `START_STICKY` restart). |
| `label` | `Option<String>` | Service label from the original `StartConfig`. |
| `service_type` | `Option<String>` | Foreground service type from the original `StartConfig`. |

#### Methods

| Method | Returns | Description |
|--------|---------|-------------|
| `into_start_config(self)` | `Option<StartConfig>` | Converts to `StartConfig` if `pending` is `true` and `label` is `Some`. Returns `None` otherwise. |

> This type is rarely used directly — the plugin handles auto-start detection internally during setup on Android.

---

## TypeScript API

Import from `tauri-plugin-background-service`:

```typescript
import {
  startService,
  stopService,
  isServiceRunning,
  getServiceState,
  getPlatformCapabilities,
  getSchedulingStatus,
  onPluginEvent,
  installService,
  uninstallService,
  startOsService,
  stopOsService,
  restartOsService,
  getOsServiceStatus,
  type StartConfig,
  type ServiceState,
  type ServiceStatus,
  type PluginEvent,
  type Platform,
  type LifecycleMode,
  type LifecycleGuarantee,
  type PlatformCapabilities,
  type IOSSchedulingStatus,
  type OsServiceStatus,
  type OsServiceInstallState,
  enableAutoRestart,
  disableAutoRestart,
  getDesiredServiceState,
  type DesiredServiceState,
  validateBackgroundServiceSetup,
  type SetupValidationReport,
  type SetupIssue,
  normalizeBackgroundServiceError,
  type BackgroundServiceErrorCode,
  type BackgroundServiceError,
  getLifecycleStatus,
  configureRecovery,
  type StopReason,
  type LifecycleState,
  type LifecycleStatus,
  type Severity,
  type ValidationIssue,
} from 'tauri-plugin-background-service';
```

---

### `startService(config?)`

Start the background service. The service struct is already registered in Rust via `init_with_service` — this command tells the actor to begin the `init()` → `run()` lifecycle.

```typescript
async function startService(config?: StartConfig): Promise<void>
```

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `config` | `StartConfig` | Optional | `{}` | Startup configuration. All fields have defaults. |

#### Returns

`Promise<void>` — resolves on success, rejects with a string error message on failure.

#### Errors

| Error | When |
|-------|------|
| `"Service is already running"` | A service is already active. Call `stopService()` first. |
| `"Platform error: ..."` | OS-specific failure (e.g. Android foreground service denied). |

#### Example

```typescript
await startService({ serviceLabel: 'Syncing data' });
```

---

### `stopService()`

Stop the running background service. Cancels the shutdown token and stops mobile keepalive.

```typescript
async function stopService(): Promise<void>
```

#### Parameters

None.

#### Returns

`Promise<void>` — resolves on success, rejects with a string error message on failure.

#### Errors

| Error | When |
|-------|------|
| `"Service is not running"` | No service is currently active. |

#### Example

```typescript
await stopService();
```

---

### `isServiceRunning()`

Check whether the background service is currently running.

```typescript
async function isServiceRunning(): Promise<boolean>
```

#### Parameters

None.

#### Returns

`Promise<boolean>` — `true` if a service is active, `false` otherwise.

#### Example

```typescript
const running = await isServiceRunning();
console.log(running); // true or false
```

---

### `installService()` (Desktop only)

Install the background service as an OS-level daemon. Requires the `desktop-service` Cargo feature. On Linux, installs a systemd user unit. On macOS, installs a launchd user agent plist. On Windows, returns a platform error.

```typescript
async function installService(): Promise<void>
```

#### Parameters

None.

#### Returns

`Promise<void>` — resolves on success, rejects with a string error message on failure.

#### Errors

| Error | When |
|-------|------|
| `"Service installation failed: ..."` | OS-specific installation failure (permissions, service manager unavailable). |
| `"Platform error: ..."` | Platform does not support OS-service mode. |

#### Example

```typescript
await installService();
```

> **Note:** This function is only available when the `desktop-service` feature is enabled. On mobile platforms, calling it will fail with "command not found".

---

### `uninstallService()` (Desktop only)

Uninstall the OS-level daemon service. Requires the `desktop-service` Cargo feature. Stops the service if running, then removes the unit/plist file.

```typescript
async function uninstallService(): Promise<void>
```

#### Parameters

None.

#### Returns

`Promise<void>` — resolves on success, rejects with a string error message on failure.

#### Errors

| Error | When |
|-------|------|
| `"Service uninstallation failed: ..."` | OS-specific uninstallation failure. |
| `"Platform error: ..."` | Platform does not support OS-service mode. |

#### Example

```typescript
await uninstallService();
```

---

### `startOsService()` (Desktop only, Unix)

Start the OS-level background service. Requires the `desktop-service` Cargo feature. On Unix (Linux/macOS), delegates to the service manager. On Windows, returns a platform error.

```typescript
async function startOsService(): Promise<void>
```

#### Parameters

None.

#### Returns

`Promise<void>` — resolves on success, rejects with a string error message on failure.

#### Errors

| Error | When |
|-------|------|
| `"Service start failed: ..."` | The service manager could not start the service (not installed, already running, permission denied). |
| `"Platform error: Windows OS-service mode is not yet supported"` | Called on Windows. |

#### Example

```typescript
await startOsService();
```

---

### `stopOsService()` (Desktop only, Unix)

Stop the OS-level background service. Requires the `desktop-service` Cargo feature. On Unix (Linux/macOS), delegates to the service manager. On Windows, returns a platform error.

```typescript
async function stopOsService(): Promise<void>
```

#### Parameters

None.

#### Returns

`Promise<void>` — resolves on success, rejects with a string error message on failure.

#### Errors

| Error | When |
|-------|------|
| `"Service stop failed: ..."` | The service manager could not stop the service. |
| `"Platform error: Windows OS-service mode is not yet supported"` | Called on Windows. |

#### Example

```typescript
await stopOsService();
```

---

### `restartOsService()` (Desktop only, Unix)

Restart the OS-level background service. Performs a best-effort stop, then starts the service. Requires the `desktop-service` Cargo feature. On Windows, returns a platform error.

```typescript
async function restartOsService(): Promise<void>
```

#### Parameters

None.

#### Returns

`Promise<void>` — resolves when the service has been restarted successfully, rejects with a string error message on failure.

#### Errors

| Error | When |
|-------|------|
| `"Service start failed: ..."` | The service could not be started after stopping. |
| `"Platform error: Windows OS-service mode is not yet supported"` | Called on Windows. |

#### Example

```typescript
await restartOsService();
```

---

### `getOsServiceStatus()` (Desktop only)

Query the current status of the OS-level background service. Requires the `desktop-service` Cargo feature. Returns the service label, mode (service manager kind), install state, IPC connection status, socket path, and last error. On Windows, returns a platform error.

```typescript
async function getOsServiceStatus(): Promise<OsServiceStatus>
```

#### Parameters

None.

#### Returns

`Promise<OsServiceStatus>` — a snapshot of the OS service's current state.

#### Example

```typescript
const status = await getOsServiceStatus();
console.log(status.label);          // "com.example.myapp.background"
console.log(status.mode);           // "systemd" | "launchd"
console.log(status.installed);      // "notInstalled" | "installed" | "running"
console.log(status.ipcConnected);   // true | false
console.log(status.socketPath);     // "/run/user/1000/..." | undefined
console.log(status.lastError);      // string | undefined
```

---

### `OsServiceInstallState` (TypeScript)

String literal union representing the OS service install/running state.

```typescript
type OsServiceInstallState = 'notInstalled' | 'installed' | 'running';
```

| Value | Description |
|-------|-------------|
| `'notInstalled'` | The OS service is not installed. |
| `'installed'` | The OS service is installed but not currently running. |
| `'running'` | The OS service is installed and currently running. |

> **Non-exhaustive:** New values may be added in future versions. Code defensively — treat unknown values as an unknown state.

---

### `OsServiceStatus` (TypeScript)

```typescript
interface OsServiceStatus {
  /** The service label (e.g. "com.example.background-service"). */
  label: string;
  /** The service manager kind (e.g. "systemd", "launchd"). */
  mode: string;
  /** Whether the service is installed and/or running. */
  installed: OsServiceInstallState;
  /** Whether the IPC connection to the service sidecar is active. */
  ipcConnected: boolean;
  /** Path to the Unix domain socket used for IPC. Omitted when not available. */
  socketPath?: string;
  /** Last error message from the OS service. Omitted when no error. */
  lastError?: string;
}
```

#### Fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `label` | `string` | Yes | The service label derived from the app identifier or `desktopServiceLabel` config. |
| `mode` | `string` | Yes | The service manager kind: `"systemd"` on Linux, `"launchd"` on macOS. |
| `installed` | `OsServiceInstallState` | Yes | Whether the OS service is installed and/or currently running. |
| `ipcConnected` | `boolean` | Yes | Whether the IPC client in the GUI process is connected to the sidecar. |
| `socketPath` | `string?` | No | Path to the Unix domain socket. Omitted when the socket path cannot be determined. |
| `lastError` | `string?` | No | Last error message from the OS service, if any. Omitted when no error has occurred. |

---

### `enableAutoRestart(config?)`

Persist the intent to keep the background service running across process kills and device reboots, **without** starting the service now. This is the recovery-focused counterpart to `startService()`.

```typescript
async function enableAutoRestart(config?: StartConfig): Promise<void>
```

#### How it differs from `startService()`

| | `startService()` | `enableAutoRestart()` |
|---|---|---|
| Starts the service | Yes, immediately | No |
| Persists recovery intent | Yes (saves as side effect) | Yes (primary purpose) |
| Service must be stopped first | Yes (`AlreadyRunning` if running) | No (idempotent, independent of running state) |
| Use case | "Start syncing now" | "Ensure the service restarts after a crash or reboot" |

Call `enableAutoRestart()` after `startService()` to ensure the service recovers automatically. You can also call it before the first `startService()` to pre-register recovery intent.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `config` | `StartConfig` | Optional | `{}` | Configuration to use when the service is automatically restarted by a recovery mechanism (e.g. after boot). If omitted, the last config from `startService()` is used, or defaults if no prior start. |

#### Returns

`Promise<void>` — resolves on success, rejects with a string error message on failure.

#### Example

```typescript
// Start the service and enable recovery
await startService({ serviceLabel: 'Syncing data' });
await enableAutoRestart();

// Or pre-register intent before the first start
await enableAutoRestart({ serviceLabel: 'Syncing data' });
```

---

### `disableAutoRestart()`

Clear the persisted recovery intent. The service **keeps running** if it is currently active — this only affects future recovery attempts.

```typescript
async function disableAutoRestart(): Promise<void>
```

#### Parameters

None.

#### Returns

`Promise<void>` — resolves on success, rejects with a string error message on failure.

#### Example

```typescript
// Stop the service and disable recovery
await stopService();
await disableAutoRestart();
```

---

### `getDesiredServiceState()`

Query the persisted desired-state for the background service. Returns recovery intent and metadata, or `null` if no persistence backend is configured on the current platform.

```typescript
async function getDesiredServiceState(): Promise<DesiredServiceState | null>
```

#### Parameters

None.

#### Returns

`Promise<DesiredServiceState | null>` — the persisted desired-state, or `null` when no backend is available (e.g. desktop in-process mode).

#### Example

```typescript
const desired = await getDesiredServiceState();
if (desired) {
  console.log(desired.desiredRunning);    // true | false
  console.log(desired.recoveryPending);   // true | false
  console.log(desired.recoveryReason);    // "boot recovery" | undefined
  console.log(desired.restartAttempt);    // 0, 1, 2, ...
  console.log(desired.lastStartConfig);   // { serviceLabel, foregroundServiceType } | undefined
} else {
  console.log('No desired-state backend on this platform');
}
```

---

### `DesiredServiceState` (TypeScript)

```typescript
interface DesiredServiceState {
  /** Whether the user wants the service running (recovery intent). */
  desiredRunning: boolean;
  /** Configuration used for the last start (or enableAutoRestart). */
  lastStartConfig?: StartConfig;
  /** Epoch millis when the service was last started. */
  lastStartEpochMs?: number;
  /** Epoch millis of the last heartbeat from the service task. */
  lastHeartbeatEpochMs?: number;
  /** Last native platform state (e.g. "timeout", "expired"). */
  lastNativeState?: string;
  /** Last platform-specific error message. */
  lastPlatformError?: string;
  /** How many restart attempts have been made since the last clean start. */
  restartAttempt: number;
  /** Whether a recovery is pending (e.g. after boot). */
  recoveryPending: boolean;
  /** Why recovery was initiated. */
  recoveryReason?: string;
}
```

#### Fields

| Field | Type | Description |
|-------|------|-------------|
| `desiredRunning` | `boolean` | Whether recovery is enabled. `true` after `enableAutoRestart()` or `startService()`, `false` after `disableAutoRestart()` or `stopService()`. |
| `lastStartConfig` | `StartConfig?` | Configuration used for the last start or passed to `enableAutoRestart()`. Used by recovery mechanisms to restart with the same config. |
| `lastStartEpochMs` | `number?` | Epoch milliseconds when the service was last started. |
| `lastHeartbeatEpochMs` | `number?` | Epoch milliseconds of the last heartbeat from the service task. |
| `lastNativeState` | `string?` | Last platform-native state string (e.g. `"timeout"`, `"expired"`). |
| `lastPlatformError` | `string?` | Last platform-specific error message. |
| `restartAttempt` | `number` | Restart attempt counter since the last clean start. Resets on successful `startService()`. |
| `recoveryPending` | `boolean` | Whether a recovery is queued (e.g. boot recovery waiting for user tap). |
| `recoveryReason` | `string?` | Human-readable reason for the current recovery (e.g. `"boot recovery"`, `"process killed"`). |

---

### `validateBackgroundServiceSetup()`

Validate the background service setup for the current platform. Checks platform-specific prerequisites (permissions, manifest entries, service manager availability) and returns a report with errors (blocking) and warnings (non-blocking).

```typescript
async function validateBackgroundServiceSetup(): Promise<SetupValidationReport>
```

#### Returns

A `SetupValidationReport` with:
- `ok` — `true` when `errors` is empty (warnings do not affect this).
- `errors` — Blocking issues that prevent the service from working correctly.
- `warnings` — Non-blocking issues that may cause degraded behavior.

#### Platform checks

| Platform | What is checked |
|----------|----------------|
| Android | Foreground service type/permission, `POST_NOTIFICATIONS` runtime permission, boot receiver for recovery, `PROPERTY_SPECIAL_USE_FGS_SUBTYPE` for `specialUse` type |
| iOS | `UIBackgroundModes` in Info.plist, `BGTaskSchedulerPermittedIdentifiers` in Info.plist, background refresh status |
| Desktop | Service manager availability (systemd/launchd), systemd lingering (Linux), macOS sandbox |

> **Note:** Android and iOS checks return warnings because Rust cannot inspect native manifests at runtime — they serve as reminders. Desktop checks actually verify the runtime environment.

#### Example

```typescript
import { validateBackgroundServiceSetup } from 'tauri-plugin-background-service';

const report = await validateBackgroundServiceSetup();

if (report.ok) {
  console.log('Setup looks good');
  if (report.warnings.length > 0) {
    console.warn('Warnings:');
    for (const w of report.warnings) {
      console.warn(`  [${w.code}] ${w.message}`);
      if (w.fix) console.warn(`    Fix: ${w.fix}`);
    }
  }
} else {
  console.error('Setup issues found:');
  for (const err of report.errors) {
    console.error(`  [${err.code}] ${err.message}`);
    if (err.fix) console.error(`    Fix: ${err.fix}`);
  }
}
```

---

### `SetupValidationReport` (TypeScript)

```typescript
interface SetupValidationReport {
  /** True when errors is empty (warnings do not affect this). */
  ok: boolean;
  /** Blocking issues that prevent the service from working correctly. */
  errors: SetupIssue[];
  /** Non-blocking issues that may cause degraded behavior. */
  warnings: SetupIssue[];
  /** Unified issues with typed severity. */
  issues: ValidationIssue[];
}
```

#### Fields

| Field | Type | Description |
|-------|------|-------------|
| `ok` | `boolean` | `true` when `errors` is empty. Warnings do not affect this value. |
| `errors` | `SetupIssue[]` | Blocking issues that prevent the service from working correctly. |
| `warnings` | `SetupIssue[]` | Non-blocking issues that may cause degraded behavior (e.g. missing boot receiver). |
| `issues` | `ValidationIssue[]` | Unified list with typed severity (`error`, `warning`, `info`). Combines errors and warnings. |

---

### `SetupIssue` (TypeScript)

```typescript
interface SetupIssue {
  /** Machine-readable error code (e.g. "android_fgs_type"). */
  code: string;
  /** Human-readable description of the issue. */
  message: string;
  /** The platform this issue applies to. */
  platform: string;
  /** Suggested fix for the issue, if available. */
  fix?: string;
}
```

#### Fields

| Field | Type | Description |
|-------|------|-------------|
| `code` | `string` | Machine-readable error code for programmatic handling. Android codes: `android_fgs_type`, `android_post_notifications`, `android_boot_receiver`, `android_special_use_subtype`. iOS codes: `ios_ui_background_modes`, `ios_bg_task_identifiers`, `ios_background_refresh`. Desktop codes: `desktop_systemd_missing`, `desktop_systemd_no_linger`, `desktop_macos_sandbox`. |
| `message` | `string` | Human-readable description of the issue. |
| `platform` | `string` | The platform this issue applies to (e.g. `"android"`, `"ios"`, `"linux"`). |
| `fix` | `string?` | Suggested fix for the issue. Present when a specific remediation step is known. |

---

### Error Handling

When a Tauri `invoke()` call rejects, the error is a plain string. The `normalizeBackgroundServiceError()` helper parses this string into a typed `BackgroundServiceError` with a machine-readable `code`.

This is an **opt-in** helper — it does not change existing promise rejection behavior. Use it in your `catch` blocks.

#### `normalizeBackgroundServiceError(error)`

```typescript
function normalizeBackgroundServiceError(error: unknown): BackgroundServiceError
```

##### Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `error` | `unknown` | The value caught from a rejected `invoke()` call. |

##### Returns

A `BackgroundServiceError` with a typed `code` and the original `message`.

##### Matching strategy

The helper uses two strategies to extract the error code:

1. **Display prefix matching** (primary): Matches the Rust `ServiceError` `Display` output (e.g. `"Service is already running"` → `alreadyRunning`).
2. **Serde variant matching** (fallback): Matches the serde-serialized variant name (e.g. `"AlreadyRunning"` or `{"Init":"msg"}` → `init`).
3. **Fallback**: Returns `{ code: "unknown", message }`.

##### Example

```typescript
import {
  startService,
  normalizeBackgroundServiceError,
} from 'tauri-plugin-background-service';

try {
  await startService({ serviceLabel: 'Syncing' });
} catch (e) {
  const err = normalizeBackgroundServiceError(e);
  switch (err.code) {
    case 'alreadyRunning':
      console.log('Service is already running, ignoring');
      break;
    case 'platform':
      console.error('Platform error:', err.message);
      break;
    default:
      console.error(`Unexpected error (${err.code}):`, err.message);
  }
}
```

---

### `BackgroundServiceErrorCode` (TypeScript)

String union of all recognized background service error codes.

```typescript
type BackgroundServiceErrorCode =
  | 'alreadyRunning'
  | 'notRunning'
  | 'init'
  | 'runtime'
  | 'platform'
  | 'pluginInvoke'
  | 'serviceInstall'
  | 'serviceUninstall'
  | 'ipc'
  | 'serviceStart'
  | 'serviceStop'
  | 'unknown';
```

| Code | When |
|------|------|
| `alreadyRunning` | `startService()` called while a service is already active. |
| `notRunning` | `stopService()` called when no service is active. |
| `init` | `init()` returned an error. |
| `runtime` | `run()` returned an error, or the actor channel closed. |
| `platform` | OS-specific failure (Android foreground service denied, iOS BGTask rejected). |
| `pluginInvoke` | Mobile plugin invoke failed (Kotlin/Swift bridge error). Mobile only. |
| `serviceInstall` | Desktop service installation failed. Requires `desktop-service` feature. |
| `serviceUninstall` | Desktop service uninstallation failed. Requires `desktop-service` feature. |
| `ipc` | Desktop IPC communication error (socket connection, framing). Requires `desktop-service` feature. |
| `serviceStart` | Desktop OS service start failed. Requires `desktop-service` feature. |
| `serviceStop` | Desktop OS service stop failed. Requires `desktop-service` feature. |
| `unknown` | The error did not match any known pattern. |

> **Non-exhaustive:** New codes may be added in future versions. Always handle `unknown` and use a `default` case in switch statements.

---

### `BackgroundServiceError` (TypeScript)

```typescript
interface BackgroundServiceError {
  /** Machine-readable error code. */
  code: BackgroundServiceErrorCode;
  /** Human-readable error message (the original Tauri rejection string). */
  message: string;
}
```

#### Fields

| Field | Type | Description |
|-------|------|-------------|
| `code` | `BackgroundServiceErrorCode` | Machine-readable error code for programmatic handling. |
| `message` | `string` | The original error message from the Rust `ServiceError` or Tauri rejection. |

---

### `getServiceState()`

Query the detailed state of the background service, including the lifecycle state, optional last error, and extended status fields.

```typescript
async function getServiceState(): Promise<ServiceStatus>
```

#### Parameters

None.

#### Returns

`Promise<ServiceStatus>` — an object with `state` and `lastError` fields (always present), plus optional extended fields populated from the desired-state backend.

#### Example

```typescript
const status = await getServiceState();
console.log(status.state);            // 'idle' | 'initializing' | 'running' | 'stopped'
console.log(status.lastError);        // null or error message string
console.log(status.desiredRunning);   // true | false | undefined
console.log(status.nativeState);      // 'idle' | 'running' | 'timeout' | ... | undefined
console.log(status.platformMode);     // 'androidForegroundService' | ... | undefined
console.log(status.lastStartConfig);  // { serviceLabel, foregroundServiceType } | undefined
console.log(status.lastHeartbeatAt);  // epoch ms | undefined
console.log(status.restartAttempt);   // number | undefined
console.log(status.recoveryReason);   // string | undefined
console.log(status.platformError);    // string | undefined
```

---

### `getPlatformCapabilities()`

Query the current platform's background execution capabilities. Returns honest, platform-specific information about what each OS can guarantee for background service survival.

```typescript
async function getPlatformCapabilities(): Promise<PlatformCapabilities>
```

#### Parameters

None.

#### Returns

`Promise<PlatformCapabilities>` — an object describing the current platform, its lifecycle mode, guarantee levels for various scenarios, known limitations, and required setup steps.

#### Example

```typescript
const caps = await getPlatformCapabilities();
console.log(caps.platform);            // 'android' | 'ios' | 'linux' | ...
console.log(caps.lifecycleMode);       // 'androidForegroundService' | ...
console.log(caps.survivesAppClose);    // 'bestEffort' | 'guaranteed' | 'unsupported'
console.log(caps.survivesReboot);      // 'bestEffort' | 'guaranteed' | 'unsupported'
console.log(caps.survivesForceQuit);   // 'unsupported' on all platforms
console.log(caps.backgroundExecution); // 'guaranteed' | 'bestEffort' | 'unsupported'
console.log(caps.limitations);         // ['OEM battery optimization may kill services', ...]
console.log(caps.requiredSetup);       // ['FOREGROUND_SERVICE permission', ...]
```

> **Use case:** Display platform-appropriate expectations in your UI. For example, on iOS where `survivesForceQuit` is `'unsupported'`, show a message explaining that force-quitting the app prevents background execution.

---

### `Platform` (TypeScript)

String literal union representing the operating system platform.

```typescript
type Platform = 'android' | 'ios' | 'windows' | 'macos' | 'linux' | 'unknown';
```

---

### `LifecycleMode` (TypeScript)

String literal union representing the lifecycle mechanism used by the plugin on the current platform.

```typescript
type LifecycleMode =
  | 'androidForegroundService'
  | 'iosBgTaskScheduler'
  | 'desktopInProcess'
  | 'desktopOsService';
```

| Value | Platform | Description |
|-------|----------|-------------|
| `'androidForegroundService'` | Android | Android foreground service with persistent notification |
| `'iosBgTaskScheduler'` | iOS | iOS BGTaskScheduler (BGAppRefreshTask / BGProcessingTask) |
| `'desktopInProcess'` | Desktop | Background service runs in the main app process |
| `'desktopOsService'` | Desktop | Background service runs as an OS-managed daemon (systemd / launchd) |

---

### `LifecycleGuarantee` (TypeScript)

String literal union representing the guarantee level for a background execution scenario.

```typescript
type LifecycleGuarantee = 'guaranteed' | 'bestEffort' | 'unsupported';
```

| Value | Meaning |
|-------|---------|
| `'guaranteed'` | The platform reliably supports this scenario. |
| `'bestEffort'` | The platform may support this scenario but cannot guarantee it (e.g. depends on OEM behavior, battery level, or OS scheduling). |
| `'unsupported'` | The platform does not support this scenario (e.g. no service survives force-quit on any platform). |

---

### `PlatformCapabilities` (TypeScript)

```typescript
interface PlatformCapabilities {
  /** The current runtime platform. */
  platform: Platform;
  /** The lifecycle mechanism in use on this platform. */
  lifecycleMode: LifecycleMode;
  /** Guarantee level for service survival when the app is closed (not force-quit). */
  survivesAppClose: LifecycleGuarantee;
  /** Guarantee level for service survival after a device reboot. */
  survivesReboot: LifecycleGuarantee;
  /** Guarantee level for service survival when the app is force-quit. */
  survivesForceQuit: LifecycleGuarantee;
  /** Guarantee level for background execution while the app is in the background. */
  backgroundExecution: LifecycleGuarantee;
  /** Platform-specific limitations (e.g. "OEM battery optimization may kill services"). */
  limitations: string[];
  /** Setup steps required on this platform for background service to function. */
  requiredSetup: string[];
}
```

#### Platform examples

**Android** (foreground service active):
```json
{
  "platform": "android",
  "lifecycleMode": "androidForegroundService",
  "survivesAppClose": "bestEffort",
  "survivesReboot": "bestEffort",
  "survivesForceQuit": "unsupported",
  "backgroundExecution": "guaranteed",
  "limitations": ["OEM battery optimization may kill services", "Force stop clears recovery state", "Android 15: dataSync type has 6-hour cumulative timeout"],
  "requiredSetup": ["FOREGROUND_SERVICE permission", "FOREGROUND_SERVICE_DATA_SYNC permission (Android 14+)"]
}
```

**iOS** (BGTaskScheduler):
```json
{
  "platform": "ios",
  "lifecycleMode": "iosBgTaskScheduler",
  "survivesAppClose": "bestEffort",
  "survivesReboot": "bestEffort",
  "survivesForceQuit": "unsupported",
  "backgroundExecution": "bestEffort",
  "limitations": ["Cannot guarantee continuous execution", "Force-quit prevents relaunch", "BGAppRefreshTask ~30s window"],
  "requiredSetup": ["UIBackgroundModes in Info.plist", "BGTaskSchedulerPermittedIdentifiers in Info.plist"]
}
```

**Desktop in-process**:
```json
{
  "platform": "linux",
  "lifecycleMode": "desktopInProcess",
  "survivesAppClose": "unsupported",
  "survivesReboot": "unsupported",
  "survivesForceQuit": "unsupported",
  "backgroundExecution": "guaranteed",
  "limitations": [],
  "requiredSetup": []
}
```

---

### `ServiceState` (TypeScript)

String literal union representing the service lifecycle state.

```typescript
type ServiceState = 'idle' | 'initializing' | 'running' | 'stopped';
```

---

### `ServiceStatus` (TypeScript)

```typescript
interface ServiceStatus {
  /** Current lifecycle state. */
  state: ServiceState;
  /** Last error message, if the service stopped due to an error. */
  lastError: string | null;
  /** Whether the service is desired to be running (persisted across restarts). */
  desiredRunning?: boolean;
  /** Platform-native state as reported by the OS service layer (e.g. "idle", "running", "timeout"). */
  nativeState?: string;
  /** The lifecycle mechanism in use on the current platform (e.g. "androidForegroundService", "desktopInProcess"). */
  platformMode?: string;
  /** Configuration used for the last successful start. */
  lastStartConfig?: StartConfig;
  /** Epoch milliseconds of the last heartbeat received from the service. */
  lastHeartbeatAt?: number;
  /** How many restart attempts have been made since the last clean start. */
  restartAttempt?: number;
  /** Human-readable reason for the current recovery attempt. */
  recoveryReason?: string;
  /** Last platform-specific error message. */
  platformError?: string;
}
```

#### `nativeState` values

The `nativeState` field, when present, contains one of these string values (camelCase):

| Value | When |
|-------|------|
| `"idle"` | No native service activity |
| `"starting"` | OS service layer is starting up |
| `"running"` | OS service layer reports active execution |
| `"stopping"` | OS service layer is shutting down |
| `"timeout"` | OS signalled a timeout (e.g. Android 15 dataSync limit) |
| `"expired"` | OS task expired (e.g. iOS BGTask) |
| `"recovering"` | Plugin is attempting recovery |
| `"error"` | OS service layer reported an error |

> **Non-exhaustive:** New values may be added in future versions. Code defensively — treat unknown values as an unknown state.

#### `platformMode` values

The `platformMode` field, when present, identifies the lifecycle mechanism:

| Value | Platform |
|-------|----------|
| `"androidForegroundService"` | Android foreground service |
| `"iosBgTaskScheduler"` | iOS BGTaskScheduler |
| `"desktopInProcess"` | Desktop in-process (default) |
| `"desktopOsService"` | Desktop OS-managed service/daemon |

---

### `getSchedulingStatus()`

Query the iOS background task scheduling status from the native layer. On iOS, returns scheduling results and desired-state values persisted in `UserDefaults`. On other platforms, returns a default status with all fields set to `false`/`undefined`.

```typescript
async function getSchedulingStatus(): Promise<IOSSchedulingStatus>
```

#### Parameters

None.

#### Returns

`Promise<IOSSchedulingStatus>` — scheduling results and desired-state values from the native iOS layer.

#### Example

```typescript
import { getSchedulingStatus } from 'tauri-plugin-background-service';

const status = await getSchedulingStatus();
console.log(status.refreshScheduled);     // Was BGAppRefreshTask scheduled?
console.log(status.processingScheduled);  // Was BGProcessingTask scheduled?
console.log(status.refreshError);         // Error from refresh scheduling, if any
console.log(status.processingError);      // Error from processing scheduling, if any
```

> **Platform behavior:** On iOS, these values are read from `UserDefaults` and reflect the most recent scheduling attempt. On Android and Desktop, the command returns a default status (all `false`/`undefined`) because BGTaskScheduler is iOS-only.

---

### `IOSSchedulingStatus` (TypeScript)

```typescript
interface IOSSchedulingStatus {
  /** Whether a BGAppRefreshTask was successfully scheduled. */
  refreshScheduled: boolean;
  /** Whether a BGProcessingTask was successfully scheduled. */
  processingScheduled: boolean;
  /** Error from BGAppRefreshTask scheduling, if any. */
  refreshError?: string;
  /** Error from BGProcessingTask scheduling, if any. */
  processingError?: string;
}
```

#### Fields

| Field | Type | Description |
|-------|------|-------------|
| `refreshScheduled` | `boolean` | Whether `BGAppRefreshTaskRequest` was submitted successfully. |
| `processingScheduled` | `boolean` | Whether `BGProcessingTaskRequest` was submitted successfully. |
| `refreshError` | `string?` | Error from `BGAppRefreshTask` scheduling. `undefined` if scheduling succeeded. |
| `processingError` | `string?` | Error from `BGProcessingTask` scheduling. `undefined` if scheduling succeeded. |

> **Note:** When both `refreshScheduled` and `processingScheduled` are `false`, the Swift layer rejects with `"schedulerUnavailable"` instead of returning this object.

---

### `onPluginEvent(handler)`

Listen to built-in plugin lifecycle events. Your service can emit custom events via `ctx.app.emit()` — subscribe to those separately with Tauri's `listen()`.

```typescript
async function onPluginEvent(
  handler: (event: PluginEvent) => void
): Promise<UnlistenFn>
```

#### Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `handler` | `(event: PluginEvent) => void` | Required | Callback invoked for each plugin event. Receives a `PluginEvent` discriminated union. |

#### Returns

`Promise<UnlistenFn>` — call the returned function to stop listening and prevent memory leaks.

#### Example

```typescript
const unlisten = await onPluginEvent((event) => {
  switch (event.type) {
    case 'started':
      console.log('Service started');
      break;
    case 'stopped':
      console.log('Service stopped:', event.reason);
      break;
    case 'error':
      console.error('Service error:', event.message);
      break;
  }
});

// Clean up when done
unlisten();
```

---

### `getLifecycleStatus()`

Query a complete snapshot of the background service lifecycle status, including state, desired state, recovery config, platform capabilities, and validation issues.

```typescript
async function getLifecycleStatus(): Promise<LifecycleStatus>
```

#### Parameters

None.

#### Returns

`Promise<LifecycleStatus>` — a full lifecycle status snapshot.

#### Example

```typescript
const status = await getLifecycleStatus();
console.log(status.state);              // 'idle' | 'starting' | 'running' | 'stopped' | ...
console.log(status.desiredRunning);     // true | false
console.log(status.recoveryEnabled);    // true | false
console.log(status.recoveryPending);    // true | false
console.log(status.platform);           // 'android' | 'ios' | 'linux' | ...
console.log(status.issues.length);      // number of current validation issues
```

> **Note:** This command requires the `allow-get-lifecycle-status` permission (not included in `background-service:default`).

---

### `configureRecovery(enabled, config?)`

Enable or disable auto-recovery at runtime. When enabled, the service will be automatically restarted after platform-imposed stops (timeouts, expirations). When disabled, the service stops permanently until manually restarted.

```typescript
async function configureRecovery(
  enabled: boolean,
  config?: StartConfig
): Promise<void>
```

#### Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `enabled` | `boolean` | Required | `true` to enable auto-recovery, `false` to disable. |
| `config` | `StartConfig` | Optional | Configuration to use when the service is automatically restarted. |

#### Example

```typescript
// Enable recovery with a specific config
await configureRecovery(true, { serviceLabel: 'Background Sync' });

// Disable recovery
await configureRecovery(false);
```

> **Note:** This command requires the `allow-configure-recovery` permission (not included in `background-service:default`).

---

### `StopReason` (TypeScript)

String literal union representing structured stop reasons.

```typescript
type StopReason =
  | 'userStop'
  | 'appStop'
  | 'platformTimeout'
  | 'platformExpiration'
  | 'nativeNotificationStop'
  | 'osRestart'
  | 'bootRecovery'
  | 'taskCompleted'
  | 'error';
```

| Value | When |
|-------|------|
| `'userStop'` | User called `stopService()`. |
| `'appStop'` | Application is shutting down. |
| `'platformTimeout'` | Platform killed the service (e.g. Android FGS timeout). |
| `'platformExpiration'` | Background window expired (e.g. iOS BGTask). |
| `'nativeNotificationStop'` | User pressed stop on the native notification. |
| `'osRestart'` | OS restarted the service after reboot. |
| `'bootRecovery'` | Service recovered after device boot. |
| `'taskCompleted'` | Service `run()` returned naturally. |
| `'error'` | Service `run()` returned an error. |

---

### `LifecycleState` (TypeScript)

String literal union representing fine-grained lifecycle states.

```typescript
type LifecycleState =
  | 'idle' | 'starting' | 'running' | 'stopping' | 'stopped'
  | 'recovering' | 'recoveryPending' | 'expired' | 'blocked' | 'error';
```

---

### `Severity` (TypeScript)

String literal union for validation issue severity.

```typescript
type Severity = 'error' | 'warning' | 'info';
```

---

### `ValidationIssue` (TypeScript)

A typed validation issue with severity level.

```typescript
interface ValidationIssue {
  severity: Severity;
  code: string;
  message: string;
  fix?: string;
  platform: string;
}
```

---

### `LifecycleStatus` (TypeScript)

Complete lifecycle status snapshot.

```typescript
interface LifecycleStatus {
  state: LifecycleState;
  desiredRunning: boolean;
  recoveryEnabled: boolean;
  recoveryPending: boolean;
  recoveryReason?: string;
  lastStartConfig?: StartConfig;
  lastPlatformState?: string;
  lastPlatformError?: string;
  lastError?: string;
  platform: string;
  capabilities: PlatformCapabilities;
  issues: ValidationIssue[];
}
```

---

### `StartConfig` (TypeScript)

Startup configuration passed to `startService()`. All fields are optional with sensible defaults.

```typescript
interface StartConfig {
  /** Text shown in the Android persistent foreground notification */
  serviceLabel?: string;
  /**
   * Android foreground service type. Valid values: "dataSync" (default),
   * "mediaPlayback", "phoneCall", "location", "connectedDevice",
   * "mediaProjection", "camera", "microphone", "health", "remoteMessaging",
   * "systemExempted", "shortService", "specialUse", "mediaProcessing".
   * Ignored on non-Android platforms.
   */
  foregroundServiceType?: string;
}
```

#### Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `serviceLabel` | `string` | Optional | `"Service running"` | Text shown in the Android persistent notification. |
| `foregroundServiceType` | `string` | Optional | `"dataSync"` | Android foreground service type. See [Android Guide](./android.md) for all 14 valid types and their required permissions. Ignored on non-Android platforms. |

---

### `PluginEvent` (TypeScript)

Discriminated union type representing plugin lifecycle events. Use the `type` field to narrow in switch statements.

```typescript
type PluginEvent =
  | { type: 'started' }
  | { type: 'stopped';  reason: StopReason }
  | { type: 'error';    message: string };
```

#### Variants

| `type` value | Additional fields | When emitted |
|-------------|-------------------|-------------|
| `'started'` | — | After `init()` completes successfully. |
| `'stopped'` | `reason: StopReason` | When `run()` returns or is cancelled. The reason is a structured enum value (e.g. `"taskCompleted"`, `"platformTimeout"`, `"userStop"`). |
| `'error'` | `message: string` | When `init()` or `run()` returns an error. |

#### Type narrowing

```typescript
onPluginEvent((event) => {
  if (event.type === 'stopped') {
    // TypeScript knows event.reason exists here
    console.log(event.reason);
  }
});
```
