# Android Platform Guide

This guide covers Android-specific behavior for the background service plugin, including the foreground service architecture, required permissions, auto-restart mechanism, and debugging.

## How It Works

On Android, the plugin uses a **Foreground Service** to keep your service alive even when the app is in the background. A persistent notification is displayed in the status bar for the duration of the service.

The service lifecycle is managed by [`LifecycleService`](../android/src/main/java/app/tauri/backgroundservice/LifecycleService.kt), which extends Android's `Service` class. The plugin's Kotlin bridge (`BackgroundServicePlugin`) communicates with it via `Intent` actions.

### Architecture

```
JS: startService()
  → Tauri Command (start)
    → Actor: handle_start()
      → MobileLifecycle.start_keepalive()
        → BackgroundServicePlugin.startKeepalive()
          → startForegroundService(LifecycleService)
            → LifecycleService.onStartCommand()
              → startForeground(notification)
```

When the Rust actor starts the service, it calls `start_keepalive` on the mobile bridge. The Kotlin plugin starts `LifecycleService` as a foreground service with a persistent notification. The service returns `START_STICKY`, which tells Android to restart it if killed.

## Required Permissions

Add the following permissions to your app's `AndroidManifest.xml` (inside the `<manifest>` tag, before `<application>`):

```xml
<!-- Required for all foreground services -->
<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />

<!-- Required for the default foregroundServiceType "dataSync" -->
<uses-permission android:name="android.permission.FOREGROUND_SERVICE_DATA_SYNC" />

<!-- Required on Android 13+ (API 33) to show the foreground notification -->
<uses-permission android:name="android.permission.POST_NOTIFICATIONS" />
```

If you use `foregroundServiceType: "specialUse"`, replace `FOREGROUND_SERVICE_DATA_SYNC` with:

```xml
<uses-permission android:name="android.permission.FOREGROUND_SERVICE_SPECIAL_USE" />
```

### Permission Details

| Permission | Required Since | Purpose |
|-----------|---------------|---------|
| `FOREGROUND_SERVICE` | API 28 (Android 9) | Allows starting foreground services |
| `FOREGROUND_SERVICE_DATA_SYNC` | API 34 (Android 14) | Required for `dataSync` service type |
| `FOREGROUND_SERVICE_SPECIAL_USE` | API 34 (Android 14) | Required for `specialUse` service type |
| `POST_NOTIFICATIONS` | API 33 (Android 13) | Runtime permission for notifications |

The plugin automatically requests `POST_NOTIFICATIONS` at runtime when the WebView loads (see `BackgroundServicePlugin.load()`). No additional code is needed.

## Foreground Service Type

The `foregroundServiceType` parameter controls which Android permission category your service declares. It is passed via `StartConfig` from JavaScript:

```typescript
import { startService } from 'tauri-plugin-background-service';

await startService({
  serviceLabel: 'Syncing data',
  foregroundServiceType: 'dataSync'
});
```

### Available Types

| Type | Android Constant | Required Permission | Use Case |
|------|-----------------|---------------------|----------|
| `"dataSync"` (default) | `FOREGROUND_SERVICE_TYPE_DATA_SYNC` | `FOREGROUND_SERVICE_DATA_SYNC` | Data synchronization, file uploads/downloads, API polling |
| `"mediaPlayback"` | `FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK` | `FOREGROUND_SERVICE_MEDIA_PLAYBACK` | Audio/video playback |
| `"phoneCall"` | `FOREGROUND_SERVICE_TYPE_PHONE_CALL` | `FOREGROUND_SERVICE_PHONE_CALL` | Ongoing phone calls |
| `"location"` | `FOREGROUND_SERVICE_TYPE_LOCATION` | `FOREGROUND_SERVICE_LOCATION` | Location tracking |
| `"connectedDevice"` | `FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE` | `FOREGROUND_SERVICE_CONNECTED_DEVICE` | Communication with external devices (BLE, USB) |
| `"mediaProjection"` | `FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION` | `FOREGROUND_SERVICE_MEDIA_PROJECTION` | Screen sharing/recording |
| `"camera"` | `FOREGROUND_SERVICE_TYPE_CAMERA` | `FOREGROUND_SERVICE_CAMERA` | Camera access |
| `"microphone"` | `FOREGROUND_SERVICE_TYPE_MICROPHONE` | `FOREGROUND_SERVICE_MICROPHONE` | Microphone access |
| `"health"` | `FOREGROUND_SERVICE_TYPE_HEALTH` | `FOREGROUND_SERVICE_HEALTH` | Health/fitness data |
| `"remoteMessaging"` | `FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING` | `FOREGROUND_SERVICE_REMOTE_MESSAGING` | Push messaging |
| `"systemExempted"` | `FOREGROUND_SERVICE_TYPE_SYSTEM_EXEMPTED` | `FOREGROUND_SERVICE_SYSTEM_EXEMPTED` | System-critical operations |
| `"shortService"` | `FOREGROUND_SERVICE_TYPE_SHORT_SERVICE` | `FOREGROUND_SERVICE_SHORT_SERVICE` | Short-lived tasks (< 3 minutes) |
| `"specialUse"` | `FOREGROUND_SERVICE_TYPE_SPECIAL_USE` | `FOREGROUND_SERVICE_SPECIAL_USE` | Custom use cases (requires Play Console justification) |
| `"mediaProcessing"` | `FOREGROUND_SERVICE_TYPE_MEDIA_PROCESSING` | `FOREGROUND_SERVICE_MEDIA_PROCESSING` | Media transcoding/processing |

Unrecognized type strings fall back to `FOREGROUND_SERVICE_TYPE_DATA_SYNC` with a warning logged to logcat.

### Choosing a Type

- Use **`"dataSync"`** (default) for most background work: syncing data, periodic API calls, file transfers.
- Use **`"specialUse"`** only when your use case doesn't fit any standard category. Google Play requires you to declare a justification for this type in the Play Console under **App Content → Foreground Services**.

## Auto-Restart Mechanism

Android may kill your app's process to reclaim memory. The plugin uses `START_STICKY` to survive these kills.

### Restart Flow

```
1. Android kills app process
2. Android restarts LifecycleService (START_STICKY)
3. LifecycleService.onStartCommand() receives null/empty intent
4. handleOsRestart() reads SharedPreferences for saved config
5. If config exists:
   a. Writes auto-start flag to SharedPreferences
   b. Starts foreground notification immediately (Android 12+ requirement)
   c. Launches the app's Activity
6. Plugin setup detects auto-start flag
7. Service is started with original StartConfig
8. Activity is moved to background
```

### Persistence

When you start a service, the plugin saves the configuration to `SharedPreferences` (file: `"bg_service"`):

| Key | Value |
|-----|-------|
| `bg_service_label` | The notification text (e.g., `"Syncing data"`) |
| `bg_service_type` | The foreground service type (e.g., `"dataSync"`) |

When the service is stopped via `stopService()`, these preferences are cleared. If Android restarts the service after a kill, `handleOsRestart()` reads these values and re-launches the Activity to reinitialize the Tauri runtime.

### What Happens on Clean Stop

Calling `stopService()` clears all SharedPreferences and stops the foreground service with `STOP_FOREGROUND_REMOVE`. The service returns `START_NOT_STICKY`, so Android will not restart it.

## Service Lifecycle

`LifecycleService` extends `android.app.Service` (not `LifecycleService` from AndroidX). The key lifecycle methods:

### `onStartCommand(intent, flags, startId)`

This is the main entry point. It handles three cases:

1. **`ACTION_STOP`**: Clears preferences, stops foreground, calls `stopSelf()`. Returns `START_NOT_STICKY`.
2. **Null intent or null action**: OS-initiated restart. Calls `handleOsRestart()`.
3. **`ACTION_START`** (normal start): Creates notification channel, calls `startForeground()`, sets `isRunning = true`. Returns `START_STICKY`.

### `onDestroy()`

Resets `isRunning` and `autoRestarting` flags to `false`.

### `onTimeout(startId, fgsType)` (Android 14+)

Called when the system determines the foreground service has run too long. Stops the foreground service and calls `stopSelf()`.

## Notification

The plugin creates a low-priority notification channel (`"bg_keepalive"`, name: `"Service Status"`) and a persistent notification with ID `9001`.

The notification shows:
- **Title**: Your app's name
- **Text**: The `serviceLabel` from `StartConfig` (default: `"Service running"`)
- **Icon**: Android system sync icon (`stat_notify_sync`)
- **Tap action**: Opens your app's main Activity

## Known Limitations

### Android 12+ (API 31)

Foreground services have stricter launch requirements:
- You must call `startForeground()` within the service's `onStartCommand()` immediately. The plugin handles this.
- Apps in the background have ~5 seconds to call `startForeground()` before the system crashes the service.

### Android 14+ (API 34)

- Foreground service types are mandatory. Each type requires its corresponding permission.
- The system enforces a timeout via `onTimeout()`. Long-running services may be killed.

### OEM Battery Optimization

Some device manufacturers (Xiaomi, Huawei, Samsung) implement aggressive battery optimization that can kill foreground services despite `START_STICKY`. Common workarounds:

- Ask users to disable battery optimization for your app in system settings
- Use `ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` intent to prompt directly
- Test on real devices, not just the emulator

## Debugging

### Logcat Filters

```bash
# Filter for the plugin's foreground service
adb logcat -s LifecycleService

# Filter for the Tauri plugin bridge
adb logcat -s BackgroundServicePlugin

# Filter for all background service related tags
adb logcat -s LifecycleService BackgroundServicePlugin tauri
```

### Checking Service State

```bash
# List running foreground services
adb shell dumpsys activity foreground

# Check if your service is running
adb shell dumpsys activity services app.tauri.backgroundservice/LifecycleService
```

### SharedPreferences

```bash
# Read the plugin's SharedPreferences
adb shell run-as <your.app.id> cat shared_prefs/bg_service.xml
```

### Common Issues

**Service crashes immediately on Android 12+:**
Ensure you're calling `startForeground()` in `onStartCommand()`. The plugin handles this, but if you're customizing the service, make sure the notification is posted within 5 seconds.

**Auto-restart doesn't work after OEM kill:**
Check if your app is excluded from battery optimization. Some OEMs ignore `START_STICKY` entirely for battery-optimized apps.

**Notification not showing on Android 13+:**
Verify `POST_NOTIFICATIONS` permission is granted. The plugin requests it automatically, but users can deny it.
