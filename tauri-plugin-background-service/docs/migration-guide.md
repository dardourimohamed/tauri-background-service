# Migration Guide

This guide covers breaking changes and migration steps between major versions of `tauri-plugin-background-service`.

## 0.1 → 0.2 Migration

Version 0.2 adds **iOS BGProcessingTask support** and a **desktop OS service mode**. There are **no breaking changes** to the existing API — all 0.1 code continues to work unchanged.

### What's New

| Feature | Platform | Description |
|---------|----------|-------------|
| `BGProcessingTask` | iOS | Longer background execution windows (minutes/hours instead of ~30 seconds) |
| `iosProcessingSafetyTimeoutSecs` config | iOS | Configurable safety timeout for processing tasks (default: 0.0, no cap) |
| `desktop-service` feature | Desktop | Cargo feature enabling OS-level daemon mode (systemd / launchd) |
| `desktopServiceMode` config | Desktop | `"inProcess"` (default) or `"osService"` for OS daemon mode |
| `desktopServiceLabel` config | Desktop | Custom label for the OS service |
| `installService()` | Desktop | TypeScript API to install OS service |
| `uninstallService()` | Desktop | TypeScript API to uninstall OS service |

### Required iOS Changes

Update your `Info.plist` to support `BGProcessingTask`:

**Before (0.1):**

```xml
<key>BGTaskSchedulerPermittedIdentifiers</key>
<array>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-refresh</string>
</array>
<key>UIBackgroundModes</key>
<array>
    <string>fetch</string>
</array>
```

**After (0.2):**

```xml
<key>BGTaskSchedulerPermittedIdentifiers</key>
<array>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-refresh</string>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-processing</string>
</array>
<key>UIBackgroundModes</key>
<array>
    <string>fetch</string>
    <string>processing</string>
</array>
```

### Optional: Desktop OS Service Mode

To use the desktop OS service mode:

1. Enable the feature in `Cargo.toml`:

```toml
[dependencies]
tauri-plugin-background-service = { version = "1.0", features = ["desktop-service"] }
```

2. Configure in `tauri.conf.json`:

```json
{
    "plugins": {
        "background-service": {
            "desktopServiceMode": "osService"
        }
    }
}
```

3. Add desktop service permissions to your capabilities.

### No Action Required For

- Existing `startService()` / `stopService()` / `isServiceRunning()` calls
- Existing `BackgroundService<R>` trait implementations
- Existing `PluginConfig` fields (`iosSafetyTimeoutSecs`, `iosCancelListenerTimeoutSecs`)
- Android foreground service behavior

## 0.4 → 0.5 Migration

There are **no breaking changes** in 0.5. All 0.4 code continues to work unchanged.

### What's New

| Feature | Platform | Description |
|---------|----------|-------------|
| Documentation overhaul | All | All docs updated to reflect current API |

### No Action Required

All existing APIs, configurations, and behavior are unchanged from 0.4.

## 0.3 → 0.4 Migration

There are **no breaking changes** in 0.4. All 0.3 code continues to work unchanged.

### What's New

| Feature | Platform | Description |
|---------|----------|-------------|
| `ServiceState` enum | All | Fine-grained lifecycle states: Idle, Initializing, Running, Stopped |
| `ServiceStatus` struct | All | State + optional last error |
| `getServiceState()` | All | TypeScript API to query detailed service state |
| `get_service_state` command | All | Rust Tauri command |
| Platform-specific `ServiceContext` | All | `service_label` and `foreground_service_type` are now `String` (mobile only, behind `#[cfg(mobile)]`) |
| IPC transport layer | Desktop | Length-prefixed JSON frames for sidecar communication |

### New API: getServiceState()

If you were using `isServiceRunning()` for a simple boolean check, you can now get more detail:

```typescript
// Before (0.3): simple boolean
const running = await isServiceRunning();

// After (0.4): detailed state
const status = await getServiceState();
console.log(status.state); // 'idle' | 'initializing' | 'running' | 'stopped'
```

### No Action Required For

- Existing `startService()` / `stopService()` / `isServiceRunning()` calls
- Existing `BackgroundService<R>` trait implementations
- Existing `PluginConfig` fields

## 0.2 → 0.3 Migration

There are **no breaking changes** in 0.3. All 0.2 code continues to work unchanged.

### What's New

| Feature | Platform | Description |
|---------|----------|-------------|
| 14 foreground service types | Android | Expanded from 2 to 14 valid `foregroundServiceType` values |
| `validate_foreground_service_type()` | Android | Rejects invalid types at Rust and Kotlin layers |
| Enhanced desktop IPC | Desktop | Persistent client with exponential backoff |

### New Foreground Service Types

If you were using custom string values for `foregroundServiceType`, they may now be rejected by the validation function. Use only the 14 valid types:

```
dataSync, mediaPlayback, phoneCall, location, connectedDevice,
mediaProjection, camera, microphone, health, remoteMessaging,
systemExempted, shortService, specialUse, mediaProcessing
```

### No Action Required For

- Existing `"dataSync"` or `"specialUse"` configurations
- Existing `startService()` / `stopService()` / `isServiceRunning()` calls
- Existing `BackgroundService<R>` trait implementations

## Change Type Classification

| Type | Meaning | Migration Required |
|------|---------|--------------------|
| **API Changed** | Function signature, parameter, or return type changed | Yes — update call sites |
| **Behavior Changed** | Runtime behavior changed without signature change | Possibly — verify assumptions |
| **Default Changed** | Default value for a configuration option changed | Possibly — check if relying on old default |
| **Deprecated** | Feature still works but will be removed in a future version | Recommended — plan migration |
| **Removed** | Feature no longer exists | Yes — replace with alternative |

## Migration Template

When a breaking change is documented, it follows this format:

```markdown
### [VERSION] Change Title (Change Type)

**Affected:** Who is affected (e.g., "All users", "Android only")

**Before:**

```rust
// Old API or configuration
```

**After:**

```rust
// New API or configuration
```

**Steps:**
1. Concrete action to migrate
2. Another concrete action
```

## 0.5 → 0.6 Migration

There are **no breaking changes** in 0.6. All 0.5 code continues to work unchanged.

### What's New

| Feature | Platform | Description |
|---------|----------|-------------|
| `getPlatformCapabilities()` | All | Reports platform-specific background execution guarantees |
| Extended `ServiceStatus` | All | New optional fields: `desiredRunning`, `nativeState`, `platformMode`, `lastHeartbeatAt`, `restartAttempt`, `recoveryReason`, `platformError` |
| `enableAutoRestart(config?)` | All | Persists intent to restart without starting the service now |
| `disableAutoRestart()` | All | Clears the auto-restart flag without stopping the service |
| `getDesiredServiceState()` | All | Reads the persisted desired-state (survives process kill) |
| `validateBackgroundServiceSetup()` | All | Checks platform prerequisites and returns errors/warnings |
| `normalizeBackgroundServiceError()` | All | Parses unknown errors into typed `BackgroundServiceError` objects |
| `installService()` / `uninstallService()` | Desktop | Install/uninstall OS-level service (systemd / launchd) |
| `startOsService()` / `stopOsService()` / `restartOsService()` | Desktop | Manage OS service lifecycle |
| `getOsServiceStatus()` | Desktop | Query OS service install state, IPC status, socket path |
| Android boot recovery | Android | `BootReceiver` auto-starts service after reboot (via `enableAutoRestart`) |
| Android timeout handling | Android | Configurable policies for Android 15 `onTimeout()` (`stop`, `notifyUser`, `scheduleRecovery`) |
| Android notification customization | Android | Configurable channel, icon, ID, and stop-action button |
| iOS scheduling result reporting | iOS | Structured `IOSSchedulingStatus` with per-task-type errors |
| iOS desired state | iOS | `ios_desired_running` persists across app launches |
| `NativeState` enum | All | `foregroundService`, `bgAppRefresh`, `bgProcessing`, `timeout`, etc. |

### New PluginConfig Fields

All new fields are optional with sensible defaults. Add them to your `tauri.conf.json` only if you need non-default values:

**Android:**

```json
{
  "plugins": {
    "background-service": {
      "androidForegroundServiceTypes": ["dataSync"],
      "androidValidateForegroundServiceType": true,
      "androidOnTimeout": "notifyUser",
      "androidNotificationChannelId": "bg_service",
      "androidNotificationChannelName": "Background Service",
      "androidNotificationId": 9001,
      "androidNotificationSmallIcon": "ic_notification",
      "androidShowStopAction": true
    }
  }
}
```

**iOS:**

```json
{
  "plugins": {
    "background-service": {
      "iosSafetyTimeoutSecs": 28.0,
      "iosCancelListenerTimeoutSecs": 14400,
      "iosProcessingSafetyTimeoutSecs": 0.0,
      "iosEarliestRefreshBeginMinutes": 15.0,
      "iosEarliestProcessingBeginMinutes": 15.0,
      "iosRequiresExternalPower": false,
      "iosRequiresNetworkConnectivity": false
    }
  }
}
```

**Desktop (requires `desktop-service` feature):**

```json
{
  "plugins": {
    "background-service": {
      "desktopServiceMode": "osService",
      "desktopServiceLabel": "com.example.myapp.background",
      "desktopServiceAutostart": true,
      "desktopStartServiceIfMissing": true,
      "desktopServiceStartTimeoutMs": 5000
    }
  }
}
```

### New TypeScript Error Handling

Opt-in helper to get typed error codes from rejected promises:

```typescript
import { normalizeBackgroundServiceError } from "tauri-plugin-background-service";

try {
  await startService();
} catch (err) {
  const typed = normalizeBackgroundServiceError(err);
  console.log(typed.code);    // "alreadyRunning" | "platform" | "ipc" | ...
  console.log(typed.message); // human-readable message
}
```

### No Action Required For

- Existing `startService()` / `stopService()` / `isServiceRunning()` calls
- Existing `BackgroundService<R>` trait implementations
- Existing `onPluginEvent()` listeners
- Existing `ServiceStatus` consumers — new fields are optional and omitted when not applicable

## 0.6 → 0.7 Migration

There are **no breaking changes** in 0.7. All 0.6 code continues to work unchanged.

### What's New

| Feature | Platform | Description |
|---------|----------|-------------|
| `StopReason` enum | All | 9 structured stop reason variants replacing plain strings in `PluginEvent.Stopped` |
| `NativeLifecycleEvent` | All | OS-signaled lifecycle transitions from Kotlin/Swift to Rust actor |
| `LifecycleState` / `LifecycleStatus` | All | 10-state lifecycle model with full status snapshot |
| `getLifecycleStatus()` | All | New command for complete lifecycle status |
| `configureRecovery()` | All | Runtime control of auto-recovery behavior |
| `Severity` / `ValidationIssue` | All | Typed validation with severity levels |
| `issues` field on `SetupValidationReport` | All | Unified issues list with typed severity alongside `errors`/`warnings` |
| `android_request_notification_permission_on_load` | Android | Configurable notification permission prompt on plugin load (default: `false`) |
| Desktop `FileDesiredStateBackend` | Desktop | File-based desired state persistence for auto-recovery across app restarts |
| iOS UserDefaults persistence | iOS | Pending BGTask info persisted to survive timing gaps |
| `consumedAt` on `PendingTaskInfo` | iOS | Track when auto-start consumed the pending task |
| `label` serde alias on `StartConfig.service_label` | iOS | Config migration support for iOS |
| 3 new permissions | All | `allow-get-lifecycle-status`, `allow-configure-recovery`, `allow-native-lifecycle-event` |

### Changed: PluginEvent.Stopped.reason type

The `reason` field in `PluginEvent.Stopped` changed from a plain `String` to a structured `StopReason` enum. The TypeScript type is now:

```typescript
// Before (0.6):
type PluginEvent = { type: 'stopped'; reason: string } | ...

// After (0.7):
type PluginEvent = { type: 'stopped'; reason: StopReason } | ...
```

**Backward compatibility:** Old string values (`"completed"`, `"cancelled"`, `"user"`) still deserialize correctly via built-in legacy mappings. Existing code that checks `event.reason === "completed"` should update to use the new enum values (`"taskCompleted"`, `"userStop"`).

### New PluginConfig Field

```json
{
  "plugins": {
    "background-service": {
      "androidRequestNotificationPermissionOnLoad": false
    }
  }
}
```

Set to `false` if your app handles the `POST_NOTIFICATIONS` permission request manually. Default is `true` for backward compatibility.

### New Permissions

Three new permissions are available but **not** included in `background-service:default` (opt-in):

| Permission | Command |
|-----------|---------|
| `allow-get-lifecycle-status` | `getLifecycleStatus()` |
| `allow-configure-recovery` | `configureRecovery()` |
| `allow-native-lifecycle-event` | `native_lifecycle_event` (internal) |

Add them to your capabilities file if you use the new APIs:

```json
{
  "permissions": [
    "background-service:default",
    "background-service:allow-get-lifecycle-status",
    "background-service:allow-configure-recovery"
  ]
}
```

### No Action Required For

- Existing `startService()` / `stopService()` / `isServiceRunning()` calls
- Existing `BackgroundService<R>` trait implementations
- Existing `onPluginEvent()` listeners — backward-compatible deserialization handles legacy strings
- Existing `ServiceStatus` consumers — no changes to this type
- Existing `SetupValidationReport` consumers — `issues` field has a default empty array

## 0.7 → 1.0 Migration

1.0 is the first stable release. It ports the production implementation and
**decouples the plugin from any host-app native core** — the plugin ships no
native library, and apps that bridge to their own native core do so via
pluggable seams (no-op by default).

### Breaking: `AutoStartConfig` removed

The legacy `AutoStartConfig` struct (and its `#[doc(hidden)]` re-export) is gone,
replaced by the desired-state recovery machinery.

- Before: `AutoStartConfig { ... }` + `get_auto_start_config` / `clear_auto_start_config`.
- After: `enable_auto_restart()` / `disable_auto_restart()` / `configure_recovery(...)`
  + `get_desired_service_state()`, backed by `DesiredState` / `FileDesiredStateBackend`.

### Breaking: Android native core renamed + decoupled

- `HeadlessCoreBridge` → `HeadlessBridge` (JNI symbols are now
  `Java_app_tauri_backgroundservice_HeadlessBridge_*`).
- `SilaConnectionService` → `BackgroundCallConnectionService`.
- The native library name is now configurable via `HeadlessBridge.nativeLibName`
  (default `"app_core"`). Set it to your cdylib name before the service starts:

  ```kotlin
  HeadlessBridge.nativeLibName = "app_core"
  ```

  A missing library yields a typed `native_library_load_failed` result (no crash);
  the lifecycle-only path (foreground service + Rust `BackgroundService<R>` task)
  is unaffected.

### Breaking: iOS `SilaNativeFFI` removed

The four `@_silgen_name("sila_*")` symbols are deleted, so the Swift package now
links for any host app. If you bridged CallKit perform-actions to a native core,
inject a closure via the **public** `BackgroundServicePlugin.callActionHandler`
(main-thread static):

```swift
BackgroundServicePlugin.callActionHandler = { callId, action in /* route to your core */ }
```

The default is a no-op (with missing-integration logging), so the plugin builds
and runs standalone. **PushKit was removed** (IOS-PUSH-01): the plugin ships no
VoIP/APNs relay and no `pushTokenSink`. If you previously relayed PushKit tokens,
that path is gone — a suspended/terminated app can no longer be woken to ring
(see the iOS guide's "CallKit (Active-Process Only)" section).

### New `desktop-service` feature (opt-in)

Managed OS service support (systemd on Linux, launchd on macOS — **Unix only**;
Windows is in-process) is behind the `desktop-service` cargo feature:

```toml
tauri-plugin-background-service = { version = "1.0", features = ["desktop-service"] }
```

### New notification / permission APIs (additive)

`getNotificationPermissionStatus`, `requestNotificationPermission`,
`requestBatteryExemption`, `canUseFullScreenIntent`,
`openFullScreenIntentSettings`, `getDesiredStateStatus`,
`startNativeLifecycleBridge`, `onPlatformError`.

### No action required for

- `startService()` / `stopService()` / `isServiceRunning()` and
  `BackgroundService<R>` trait implementations.
- `onPluginEvent()` listeners (backward-compatible deserialization).

## Version History

_No versions with breaking changes yet._

## Planned Breaking Changes

_No planned breaking changes at this time._

When planning a breaking change, document it here before release so users can prepare. Include the target version, the planned change, and the recommended migration path.
