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

## Foreground Service Type Configuration

The plugin validates the foreground service type passed to `startService()` against a configurable allowlist. This prevents runtime errors from undeclared types and ensures your manifest has the required permissions.

### Configuration

Add these fields to your plugin config in `tauri.conf.json`:

```json
{
  "plugins": {
    "background-service": {
      "androidForegroundServiceTypes": ["dataSync"],
      "androidValidateForegroundServiceType": true
    }
  }
}
```

#### Config Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `androidForegroundServiceTypes` | `string[]` | `["dataSync"]` | List of foreground service types allowed for `startService()`. The preflight validation rejects any type not in this list. |
| `androidValidateForegroundServiceType` | `boolean` | `true` | Whether to validate the requested type against the allowlist before starting the service. Set to `false` to skip validation. |

If you use multiple foreground service types (e.g., your app supports both `dataSync` and `specialUse`), declare all of them:

```json
{
  "plugins": {
    "background-service": {
      "androidForegroundServiceTypes": ["dataSync", "specialUse"]
    }
  }
}
```

### Preflight Validation

When `androidValidateForegroundServiceType` is `true` (the default), the Kotlin layer checks the `foregroundServiceType` from `startService()` against the `androidForegroundServiceTypes` allowlist before starting the native service. If the type is not in the allowlist, `startService()` rejects with a descriptive error:

```
foreground service type 'specialUse' is not in the configured allowlist [dataSync].
Add it to androidForegroundServiceTypes in your plugin config.
```

Set `androidValidateForegroundServiceType` to `false` to bypass this check. This is useful during development or if your app dynamically determines the service type at runtime.

## Manifest Setup

Your app's `AndroidManifest.xml` must declare the permissions and service configuration that match the foreground service types you use.

### Required Permissions

Declare the `FOREGROUND_SERVICE` permission plus a type-specific permission for each type in your `androidForegroundServiceTypes`:

```xml
<!-- Required for all foreground services -->
<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />

<!-- Required for "dataSync" (default) -->
<uses-permission android:name="android.permission.FOREGROUND_SERVICE_DATA_SYNC" />
```

### Service Declaration

The plugin's `LifecycleService` is declared in the plugin's own manifest with `foregroundServiceType="dataSync|specialUse"`. This over-declaration is intentional — the manifest is static, while the actual type is selected at runtime via `startService()` config.

### specialUse Type

If you use `foregroundServiceType: "specialUse"`, you must also:

1. Declare the permission:
   ```xml
   <uses-permission android:name="android.permission.FOREGROUND_SERVICE_SPECIAL_USE" />
   ```

2. Add the `PROPERTY_SPECIAL_USE_FGS_SUBTYPE` property to the service declaration. The plugin's manifest already includes this:
   ```xml
   <property
       android:name="android.app.PROPERTY_SPECIAL_USE_FGS_SUBTYPE"
       android:value="Background service for continuous task execution" />
   ```

3. Provide justification in Google Play Console under **App Content → Foreground Services**.

### Manifest Checklist

| If using | Add to manifest |
|----------|----------------|
| `"dataSync"` (default) | `FOREGROUND_SERVICE_DATA_SYNC` permission |
| `"specialUse"` | `FOREGROUND_SERVICE_SPECIAL_USE` permission |
| `"location"` | `FOREGROUND_SERVICE_LOCATION` + `ACCESS_FINE_LOCATION` or `ACCESS_COARSE_LOCATION` |
| `"camera"` | `FOREGROUND_SERVICE_CAMERA` + `CAMERA` |
| `"microphone"` | `FOREGROUND_SERVICE_MICROPHONE` + `RECORD_AUDIO` |
| `"health"` | `FOREGROUND_SERVICE_HEALTH` + `ACTIVITY_RECOGNITION` or `HIGH_SAMPLING_RATE_SENSORS` |
| `"connectedDevice"` | `FOREGROUND_SERVICE_CONNECTED_DEVICE` |
| `"mediaPlayback"` | `FOREGROUND_SERVICE_MEDIA_PLAYBACK` |
| `"phoneCall"` | `FOREGROUND_SERVICE_PHONE_CALL` |
| `"mediaProjection"` | `FOREGROUND_SERVICE_MEDIA_PROJECTION` |
| `"remoteMessaging"` | `FOREGROUND_SERVICE_REMOTE_MESSAGING` |
| `"systemExempted"` | `FOREGROUND_SERVICE_SYSTEM_EXEMPTED` |
| `"shortService"` | `FOREGROUND_SERVICE_SHORT_SERVICE` |
| `"mediaProcessing"` | `FOREGROUND_SERVICE_MEDIA_PROCESSING` |

## Auto-Restart API

Use the desired-state API to persist the intent to keep the service running across process kills and device reboots. This is the public API layer on top of the internal mechanisms documented below.

### `enableAutoRestart(config?)`

Persists `desiredRunning = true` in the durable state store. The plugin's recovery mechanisms (START_STICKY restart, boot receiver, app update receiver) will use this flag and the stored config to automatically restart the service.

```typescript
import { enableAutoRestart, startService } from 'tauri-plugin-background-service';

// Start the service and enable recovery
await startService({ serviceLabel: 'Syncing data' });
await enableAutoRestart();
```

You can also call `enableAutoRestart()` before the first `startService()` to pre-register recovery intent. The optional `config` parameter sets the `StartConfig` used for future automatic restarts.

### `disableAutoRestart()`

Persists `desiredRunning = false` and clears recovery fields. The service keeps running if it is currently active — this only affects future recovery attempts.

```typescript
await disableAutoRestart();
```

### Recovery triggers

When `desiredRunning = true`, the following events trigger automatic recovery:

| Trigger | Mechanism | Behavior |
|---------|-----------|----------|
| Process killed by OS | `START_STICKY` | LifecycleService restarted, posts recovery notification for user to tap |
| Device reboot | `BootReceiver` | Reads DurableState, starts service directly or posts notification (API 35+ restrictions apply) |
| App updated | `BootReceiver` (`MY_PACKAGE_REPLACED`) | Reads DurableState, starts service directly (not subject to boot-time restrictions) |

### Force-stop limitation

When the user force-stops your app from system settings, Android clears all `SharedPreferences` — including the plugin's durable state. Recovery is not possible until the user manually launches the app again. This is an Android design limitation that no app can bypass.

### Checking recovery state

Use `getDesiredServiceState()` to inspect the current recovery state:

```typescript
const desired = await getDesiredServiceState();
if (desired?.desiredRunning) {
  console.log('Recovery enabled');
  console.log('Pending recovery:', desired.recoveryPending);
  console.log('Restart attempts:', desired.restartAttempt);
}
```

---

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
   c. Posts recovery notification for user to tap and resume the service
6. User taps recovery notification → app Activity launches
7. Plugin setup detects auto-start flag
8. Service is started with original StartConfig
```

### Persistence

When you start a service, the plugin saves the configuration to `SharedPreferences` (file: `"bg_service"`):

| Key | Value |
|-----|-------|
| `bg_service_label` | The notification text (e.g., `"Syncing data"`) |
| `bg_service_type` | The foreground service type (e.g., `"dataSync"`) |

When the service is stopped via `stopService()`, these preferences are cleared. If Android restarts the service after a kill, `handleOsRestart()` reads these values and posts a recovery notification. Tapping the notification launches the Activity to reinitialize the Tauri runtime.

### What Happens on Clean Stop

Calling `stopService()` clears all SharedPreferences and stops the foreground service with `STOP_FOREGROUND_REMOVE`. The service returns `START_NOT_STICKY`, so Android will not restart it.

## Boot Recovery

The plugin can automatically recover your background service after a device reboot or app update. This uses a `BroadcastReceiver` that listens for system broadcasts and a `DurableState` store that persists the service's "desired running" state across restarts.

### How It Works

When the service is running, the plugin persists `desiredRunning = true` in `DurableState` (a separate `SharedPreferences` file named `"tauri_bg_service_state"`). On reboot or app update, the `BootReceiver` reads this state and either starts the service directly or posts a recovery notification, depending on the foreground service type and Android version.

```
Service starts → DurableState.desiredRunning = true
Device reboots → BootReceiver.onReceive(BOOT_COMPLETED)
  → Read DurableState
  → If !desiredRunning → return (no action)
  → If FGS type is blocked on API 35+ → post recovery notification
  → Otherwise → start service directly

App updated → BootReceiver.onReceive(MY_PACKAGE_REPLACED)
  → Read DurableState
  → If desiredRunning → start service directly
```

### Manifest Setup

The plugin's manifest already declares the required permission and receiver:

```xml
<uses-permission android:name="android.permission.RECEIVE_BOOT_COMPLETED" />

<receiver
    android:name=".BootReceiver"
    android:enabled="true"
    android:exported="false">
    <intent-filter>
        <action android:name="android.intent.action.BOOT_COMPLETED" />
        <action android:name="android.intent.action.LOCKED_BOOT_COMPLETED" />
        <action android:name="android.intent.action.MY_PACKAGE_REPLACED" />
    </intent-filter>
</receiver>
```

The receiver is `exported="false"` — system broadcasts are still delivered, but third-party apps cannot trigger it. `LOCKED_BOOT_COMPLETED` is handled but ignored because credential-encrypted storage is not accessible in direct-boot mode.

### Android 15 (API 35+) Restrictions

Android 15 blocks certain foreground service types from being started by boot-time receivers. If your service uses one of these types, the `BootReceiver` cannot start it directly after a reboot. Instead, it posts a recovery notification for the user to tap.

**Blocked types on API 35+:**

| Type | Boot Recovery Behavior |
|------|----------------------|
| `"dataSync"` | Blocked — posts recovery notification |
| `"camera"` | Blocked — posts recovery notification |
| `"mediaPlayback"` | Blocked — posts recovery notification |
| `"phoneCall"` | Blocked — posts recovery notification |
| `"mediaProjection"` | Blocked — posts recovery notification |
| `"microphone"` | Blocked — posts recovery notification |

**Unaffected types:**

| Type | Boot Recovery Behavior |
|------|----------------------|
| `"specialUse"` | Starts directly |
| `"location"` | Starts directly |
| `"connectedDevice"` | Starts directly |
| `"health"` | Starts directly |
| `"remoteMessaging"` | Starts directly |
| `"systemExempted"` | Starts directly |
| `"shortService"` | Starts directly |
| `"mediaProcessing"` | Starts directly |

**Workarounds for blocked types:**

- Switch to `"specialUse"` if your use case qualifies (requires Play Console justification).
- Accept notification-based recovery — the user taps the notification to resume the service after reboot.
- On devices running API 34 or below, all types start directly after reboot.

### App Update Recovery (MY_PACKAGE_REPLACED)

When your app is updated (either via Google Play or sideload), the `MY_PACKAGE_REPLACED` broadcast fires. This broadcast is **not** subject to the boot-time FGS type restrictions — the service starts directly regardless of type, as long as `desiredRunning = true`.

### Recovery Notification

When the `BootReceiver` cannot start the service directly (blocked type on API 35+), or when the `LifecycleService` is restarted by `START_STICKY` after an OS kill, it posts a recovery notification:

- **Channel:** `"bg_service_recovery"` (importance: high)
- **ID:** `9002`
- **Title:** Your app's name
- **Text:** `"Tap to resume: {serviceLabel}"`
- **Ongoing:** Yes (cannot be dismissed by swiping)
- **Tap action:** Opens your app's main Activity

Tapping the notification launches the app, the plugin detects the auto-start flag, and the service resumes with its original configuration.

The recovery notification is automatically cancelled when the service starts normally (e.g., after the user taps the notification and the Activity loads).

> **Note:** On Android 13+ (API 33), if the user has not granted `POST_NOTIFICATIONS` permission, the recovery notification is silently dropped by the OS. The service state is still persisted — the user can manually open the app to resume the service.

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

Called when the system determines the foreground service has run too long. The plugin persists a timeout state, applies the configured timeout policy, emits a `stopped` event with `reason: "timeout"` to the JS layer, then stops the foreground service and calls `stopSelf()`. See [Timeout Handling](#timeout-handling) for details.

## Timeout Handling

Android 15 (API 35) enforces a **6-hour cumulative time limit** for foreground services using the `dataSync` type. When this limit is exceeded, the system calls `onTimeout()` on the foreground service. The plugin handles this by:

1. Persisting `lastNativeState = "timeout"` and `lastPlatformError` to `DurableState`.
2. Applying the configured timeout policy (see below).
3. Emitting a `stopped` event with `reason: "timeout"` to the JS layer via `onPluginEvent()`.
4. Stopping the foreground service.

### Timeout Policies

Configure the timeout policy via `androidOnTimeout` in your plugin config. Three policies are available:

| Policy | Behavior | Use Case |
|--------|----------|----------|
| `"notifyUser"` (default) | Stops the service and posts a high-priority timeout notification to the status bar. `desiredRunning` stays `true` in persistent state. | Most apps — the user sees the notification and can restart the service. |
| `"stop"` | Stops the service cleanly. No notification is posted. | Apps that don't need to recover from timeout. |
| `"scheduleRecovery"` | Stops the service and sets `recoveryPending = true` with `recoveryReason = "timeout"`. Posts a recovery notification via `BootReceiver`. | Apps that want automatic recovery on the next app launch or device reboot. |

### Configuration

```json
{
  "plugins": {
    "background-service": {
      "androidOnTimeout": "notifyUser"
    }
  }
}
```

### Timeout Notification

When the policy is `"notifyUser"`, the plugin posts a high-priority notification to a separate channel:

- **Channel ID:** `"bg_service_timeout"` (importance: high)
- **Notification ID:** `9003`
- **Title:** Your app's name
- **Text:** `"Background service timed out: {serviceLabel}"`
- **Icon:** Same as the foreground service notification icon (configured or default)
- **Tap action:** Opens your app's main Activity

This notification is separate from the foreground service notification and the recovery notification. It is automatically cancelled when the service starts normally again.

### Receiving Timeout Events

The timeout event is delivered to the JS layer via `onPluginEvent()`:

```typescript
import { onPluginEvent } from 'tauri-plugin-background-service';

const unlisten = await onPluginEvent((event) => {
  if (event.type === 'stopped' && event.reason === 'timeout') {
    console.log('Service was stopped due to Android timeout');
    // Optionally restart or notify the user
  }
});
```

### Android 15 dataSync Timeout Details

The 6-hour limit is **cumulative** — it counts all time the foreground service has been running with the `dataSync` type since the app was last started (not since the device was booted). After the timeout fires:

- The foreground service is killed by the system if `onTimeout()` does not stop it.
- The plugin always stops the service in `onTimeout()` to comply with Android requirements.
- Other foreground service types (`specialUse`, `location`, etc.) are not subject to this specific timeout, but may have their own limits.

## Notification Customization

The foreground service notification is customizable via plugin config fields. All notification customization applies to Android only.

### Configuration

```json
{
  "plugins": {
    "background-service": {
      "androidNotificationChannelId": "my_service_channel",
      "androidNotificationChannelName": "My Background Service",
      "androidNotificationId": 9100,
      "androidNotificationSmallIcon": "ic_notification",
      "androidShowStopAction": true
    }
  }
}
```

### Config Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `androidNotificationChannelId` | `string` | `"bg_service"` | Notification channel ID. The channel is created automatically with the configured name. |
| `androidNotificationChannelName` | `string` | `"Background Service"` | Notification channel name visible to the user in system settings. |
| `androidNotificationId` | `number` | `9001` | Notification ID for the foreground service notification. Must be unique within your app. |
| `androidNotificationSmallIcon` | `string?` | `null` (system default) | Custom small icon resource name (without extension). The resource must exist in `res/drawable/`. Falls back to the system sync icon (`stat_notify_sync`) if the resource is not found. |
| `androidShowStopAction` | `boolean` | `true` | Whether to show a "Stop" action button on the foreground notification. When `true`, the user can stop the service directly from the notification. |

### Custom Small Icon

To use a custom notification icon:

1. Add a drawable resource to your Android project at `app/src/main/res/drawable/ic_notification.png` (or XML vector drawable).

2. Set the config field to the resource name without the extension:
   ```json
   {
     "plugins": {
       "background-service": {
         "androidNotificationSmallIcon": "ic_notification"
       }
     }
   }
   ```

3. The plugin looks up the resource using `Resources.getIdentifier()`. If the resource is not found, it falls back to `android.R.drawable.stat_notify_sync`.

> **Tip:** Android recommends using monochrome white icons (24dp) for notification small icons. See [Android's notification design guidelines](https://material.io/design/platform-guidance/android-notifications.html) for best practices.

## Notification

The plugin creates a notification channel and persistent notification for the foreground service. By default, these use the following values (all configurable — see [Notification Customization](#notification-customization)):

- **Channel ID:** `"bg_service"` (importance: low)
- **Channel name:** `"Background Service"`
- **Notification ID:** `9001`

The notification shows:
- **Title**: Your app's name
- **Text**: The `serviceLabel` from `StartConfig` (default: `"Service running"`)
- **Icon**: Android system sync icon (`stat_notify_sync`), or your custom icon if configured
- **Stop action**: A "Stop" button (when `androidShowStopAction` is `true`)
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
