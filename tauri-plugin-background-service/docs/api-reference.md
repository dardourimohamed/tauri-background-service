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

Plugin-level configuration, deserialized from the Tauri plugin config in `tauri.conf.json`. Controls iOS-specific timing parameters and desktop service mode.

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
| `desktop_service_mode` | `String` | Optional | `"inProcess"` | Desktop service mode: `"inProcess"` (default) or `"osService"`. Desktop only, requires `desktop-service` feature. |
| `desktop_service_label` | `Option<String>` | Optional | Auto-derived | Custom label for the OS service. Desktop only, requires `desktop-service` feature. |

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
      "desktopServiceMode": "osService",
      "desktopServiceLabel": "com.example.myapp.background"
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
    Stopped { reason: String },
    Error { message: String },
}
```

#### Variants

| Variant | Payload | JSON shape | When emitted |
|---------|---------|-----------|-------------|
| `Started` | — | `{ "type": "started" }` | After `init()` completes successfully. |
| `Stopped` | `reason: String` | `{ "type": "stopped", "reason": "..." }` | When `run()` returns `Ok(())`. Currently always emits `reason: "completed"`. |
| `Error` | `message: String` | `{ "type": "error", "message": "..." }` | When `init()` or `run()` returns an error. |

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

### `ServiceStatus`

Struct returned by the `get_service_state` command. Provides the current state and an optional last error message.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub state: ServiceState,
    pub last_error: Option<String>,
}
```

#### Fields

| Field | Type | Description |
|-------|------|-------------|
| `state` | `ServiceState` | Current lifecycle state of the service. |
| `last_error` | `Option<String>` | Error message from the last failure, if any. `None` if no error has occurred. |

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

`Result<ServiceStatus, String>` — the current service state and optional last error.

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
  onPluginEvent,
  installService,
  uninstallService,
  type StartConfig,
  type ServiceState,
  type ServiceStatus,
  type PluginEvent,
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

Install the background service as an OS-level daemon. Requires the `desktop-service` Cargo feature.

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
| `"Platform error: ..."` | OS-specific installation failure (permissions, service manager unavailable). |

#### Example

```typescript
await installService();
```

> **Note:** This function is only available when the `desktop-service` feature is enabled. On mobile platforms, calling it will fail with "command not found".

---

### `uninstallService()` (Desktop only)

Uninstall the OS-level daemon service. Requires the `desktop-service` Cargo feature.

```typescript
async function uninstallService(): Promise<void>
```

#### Parameters

None.

#### Returns

`Promise<void>` — resolves on success, rejects with a string error message on failure.

#### Example

```typescript
await uninstallService();
```

---

### `getServiceState()`

Query the detailed state of the background service, including the lifecycle state and any last error.

```typescript
async function getServiceState(): Promise<ServiceStatus>
```

#### Parameters

None.

#### Returns

`Promise<ServiceStatus>` — an object with `state` and `lastError` fields.

#### Example

```typescript
const status = await getServiceState();
console.log(status.state);     // 'idle' | 'initializing' | 'running' | 'stopped'
console.log(status.lastError); // null or error message string
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
  state: ServiceState;
  lastError: string | null;
}
```

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
  | { type: 'stopped';  reason: string }
  | { type: 'error';    message: string };
```

#### Variants

| `type` value | Additional fields | When emitted |
|-------------|-------------------|-------------|
| `'started'` | — | After `init()` completes successfully. |
| `'stopped'` | `reason: string` | When `run()` returns `Ok(())`. Currently always emits `reason: "completed"`. |
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
