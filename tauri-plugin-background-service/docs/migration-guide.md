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
tauri-plugin-background-service = { version = "0.5", features = ["desktop-service"] }
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

## Version History

_No versions with breaking changes yet._

## Planned Breaking Changes

_No planned breaking changes at this time._

When planning a breaking change, document it here before release so users can prepare. Include the target version, the planned change, and the recommended migration path.
