# Troubleshooting

Common issues and solutions when integrating `tauri-plugin-background-service`. Each entry is tagged by platform.

---

### [Android] Service dies immediately on Android 12+

**Symptom:** The background service starts but is killed within seconds. Logcat shows a `ForegroundServiceStartNotAllowedException` or a message about missing foreground service type.

**Root cause:** Android 12 (API 31) requires a foreground service type in the manifest. Android 14 (API 34) further requires declaring the specific type at runtime and handling the `onTimeout` callback.

**Solution:**

1. Add the foreground service permission with type to your `AndroidManifest.xml`:

```xml
<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
<uses-permission android:name="android.permission.FOREGROUND_SERVICE_DATA_SYNC" />
```

2. Declare the service type on the `<service>` element:

```xml
<service
  android:name="app.tauri.backgroundservice.LifecycleService"
  android:foregroundServiceType="dataSync"
  android:exported="false" />
```

3. The plugin defaults to `"dataSync"`. If you use `specialUse`, also add:

```xml
<uses-permission android:name="android.permission.FOREGROUND_SERVICE_SPECIAL_USE" />
```

4. Make sure you're calling `startService()` from a foreground context (visible Activity). Android 12+ restricts background starts.

**See also:** [Android Platform Guide](./android.md)

---

### [Android] Service does not restart after OS kills the app

**Symptom:** The service runs while the app is alive, but after Android kills the process (low memory, swipe-away), the service never comes back.

**Root cause:** The auto-restart mechanism stores a flag in `SharedPreferences` before starting the foreground service. When `LifecycleService` is re-created by `START_STICKY`, the plugin reads this flag during the Activity's `load()` lifecycle and restarts the service automatically. If the flag was never written (e.g., the service was stopped cleanly via `stopService()`), no restart occurs.

**Solution:**

1. Verify auto-restart state by checking the `SharedPreferences` flags:

```bash
adb shell run-as <your.package> cat shared_prefs/bg_service.xml
```

If auto-restart is pending, you'll see `bg_auto_start_pending` set to `true` with the original `bg_auto_start_label` and `bg_auto_start_type` values.

2. The `LifecycleService` does not produce log output during `handleOsRestart()`. To verify the mechanism is working, check that:
   - `bg_service_label` and `bg_service_type` exist in SharedPreferences (written by `BackgroundServicePlugin.startKeepalive()`)
   - The app's main Activity is launched after an OS restart (the `autoRestarting` flag is set to `true`)
   - The plugin's `load()` method reads `bg_auto_start_pending` and re-calls `startKeepalive()`

3. Check that `LifecycleService` is declared in your manifest with `android:foregroundServiceType`.

4. If you explicitly stopped the service with `stopService()`, the `bg_service_label` key is cleared and no auto-restart happens. This is expected behavior — call `startService()` again from your app UI.

**See also:** [Android Platform Guide](./android.md#auto-restart-mechanism)

---

### [Android] Boot recovery not working on Android 15

**Symptom:** The background service does not automatically start after a device reboot on Android 15 (API 35), even though `desiredRunning` was `true` before reboot.

**Root cause:** Android 15 blocks certain foreground service types from being started by `BOOT_COMPLETED` receivers. The default type `"dataSync"` is one of the blocked types. The `BootReceiver` cannot start the foreground service directly — it posts a recovery notification instead.

**Blocked types on API 35+:** `dataSync`, `camera`, `mediaPlayback`, `phoneCall`, `mediaProjection`, `microphone`.

**Solution:**

1. Check if a recovery notification was posted. After reboot, look for a high-priority notification from your app with text "Tap to resume: {label}". Tapping it opens the app and resumes the service.

2. If you need automatic start without user interaction after reboot, switch to an unblocked foreground service type:

   ```json
   {
     "plugins": {
       "background-service": {
         "androidForegroundServiceTypes": ["specialUse"]
       }
     }
   }
   ```

   Note: `specialUse` requires a Play Console justification under **App Content → Foreground Services**.

3. On Android 13+ (API 33), also verify that `POST_NOTIFICATIONS` permission is granted. Without it, the recovery notification is silently dropped by the OS.

4. App updates (`MY_PACKAGE_REPLACED`) are **not** subject to this restriction — the service starts directly after an app update regardless of type.

**See also:** [Android Platform Guide — Boot Recovery](./android.md#boot-recovery)

---

### [Android] Service does not recover after force stop

**Symptom:** After force-stopping the app (via Settings → Force Stop or `adb shell am force-stop`), the service never restarts, even on reboot.

**Root cause:** This is an Android design limitation. When the user force-stops an app:

1. Android kills the entire app process, including all services.
2. The app enters a "stopped" state — it cannot receive implicit broadcasts (including `BOOT_COMPLETED`) until the user explicitly launches it again.
3. `AlarmManager` and `JobScheduler` entries for the app are also cancelled.

This behavior is intentional and cannot be overridden. It applies to all apps, not just this plugin.

**Solution:**

1. The user must manually open the app to exit the "stopped" state. After that, the service will resume normally.

2. If the service was running before force-stop, `DurableState.desiredRunning` remains `true`. When the user reopens the app, the plugin can detect this and offer to restart the service.

3. This is not a bug — it is Android's intended behavior for force-stop. Do not attempt to work around it.

**See also:** [Android Platform Guide — Boot Recovery](./android.md#boot-recovery)

---

### [Android] Notification permission not granted on Android 13+

**Symptom:** The foreground service starts but no notification appears, or the service crashes with a `SecurityException` about `POST_NOTIFICATIONS`.

**Root cause:** Android 13 (API 33) requires runtime permission for posting notifications. The plugin requests this automatically during setup, but if the user denied it, the foreground notification cannot be shown.

**Solution:**

1. The plugin auto-requests `POST_NOTIFICATIONS` in `BackgroundServicePlugin.load()`. Check your app's permission settings:

```bash
adb shell dumpsys notification policy
```

2. If denied, prompt the user to grant the permission via system settings:

```bash
adb shell am start -a android.settings.APP_NOTIFICATION_SETTINGS \
  --es android.provider.extra.APP_PACKAGE <your.package>
```

3. You can also request the permission from your Tauri app before starting the service using the `@tauri-apps/plugin-notification` permission API.

4. On Android 14+, the notification channel `"bg_keepalive"` (ID `9001`) must have at least `IMPORTANCE_LOW`. The plugin creates this channel automatically.

**See also:** [Android Platform Guide](./android.md#notification)

---

### [iOS] Background service stops after approximately 30 seconds

**Symptom:** The service starts successfully, but iOS terminates it around 28-30 seconds after the app enters the background.

**Root cause:** This is expected iOS behavior. iOS grants background execution time in short bursts (typically 30 seconds) via `BGAppRefreshTask`. The plugin uses a safety timer (default: 28 seconds) to complete the task gracefully before iOS kills it.

**Solution:**

1. This is not a bug — it is a platform limitation. Design your `run()` method to handle cooperative cancellation via `CancellationToken`:

```rust
async fn run(&self, ctx: &ServiceContext<R>) -> Result<(), Box<dyn Error + Send + Sync>> {
    tokio::select! {
        _ = ctx.shutdown.cancelled() => {
            // iOS expiration handler fired — clean up quickly
            Ok(())
        }
        _ = do_work(ctx) => {
            Ok(())
        }
    }
}
```

2. The plugin automatically schedules the next `BGAppRefreshTask` after the current one completes. iOS decides when to run it (minimum 15 minutes between invocations via `earliestBeginDate`).

3. To increase the safety margin, configure `iosSafetyTimeoutSecs` in your Tauri plugin config (default is `28.0`):

```json
{
  "plugins": {
    "background-service": {
      "iosSafetyTimeoutSecs": 25.0
    }
  }
}
```

Keep it below 30 to avoid iOS forcefully terminating the task.

**See also:** [iOS Platform Guide](./ios.md#foreground-vs-background-behavior)

---

### [iOS] Scheduler unavailable

**Symptom:** `startService()` rejects with:

```
Platform error: schedulerUnavailable
```

**Root cause:** Both `BGAppRefreshTask` and `BGProcessingTask` scheduling attempts failed. This typically means the required `Info.plist` entries are missing or incorrect.

**Solution:**

1. Verify `BGTaskSchedulerPermittedIdentifiers` in your `Info.plist` includes both identifiers:

```xml
<key>BGTaskSchedulerPermittedIdentifiers</key>
<array>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-refresh</string>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-processing</string>
</array>
```

2. Verify `UIBackgroundModes` includes both `fetch` and `processing`:

```xml
<key>UIBackgroundModes</key>
<array>
    <string>fetch</string>
    <string>processing</string>
</array>
```

3. Check the scheduling error details using `getSchedulingStatus()`:

```typescript
import { getSchedulingStatus } from 'tauri-plugin-background-service';

const status = await getSchedulingStatus();
console.log(status.refreshError);     // Error from BGAppRefreshTask scheduling
console.log(status.processingError);  // Error from BGProcessingTask scheduling
```

4. Clean and rebuild the app after modifying `Info.plist` — cached builds may not pick up plist changes.

**See also:** [iOS Platform Guide — Scheduling Results](./ios.md#scheduling-results)

---

### [iOS] Background tasks not running after force quit

**Symptom:** After force-quitting the app (swipe-up in app switcher), background tasks never execute again until the user manually opens the app.

**Root cause:** This is an iOS design limitation. When the user force-quits an app:

1. iOS immediately terminates all background tasks. `setTaskCompleted` is never called.
2. The app is removed from BGTaskScheduler's eligible pool.
3. iOS will not relaunch the app for background execution until the user explicitly taps the app icon.

This applies to all apps, not just this plugin. Only `location`, `audio`, and `VoIP` background modes can trigger relaunch after force-quit, and App Store review validates legitimate use of these modes.

**Solution:**

1. This is not a bug — it is intentional iOS behavior. No workaround exists.
2. When the user reopens the app, `ios_desired_running` in `UserDefaults` will still be `true` if the service was running before force-quit. Your app can check this and offer to restart the service:

```typescript
import { getSchedulingStatus } from 'tauri-plugin-background-service';

const status = await getSchedulingStatus();
if (status.desiredRunning) {
  // Service was running before — offer to restart
  await startService();
}
```

3. Do not attempt to work around this limitation with location/audio/VoIP background modes unless your app genuinely uses those capabilities.

**See also:** [iOS Platform Guide — Limitations](./ios.md#limitations)

---

### [All] `ServiceError::AlreadyRunning` when calling `startService()`

**Symptom:** Calling `startService()` returns an error:

```json
"AlreadyRunning"
```

Or in Rust:

```
Service is already running
```

**Root cause:** The actor rejects duplicate starts. Only one service instance can run at a time. This is checked in `manager.rs` before any side-effects occur.

**Solution:**

1. Check if the service is already running before calling start:

```typescript
import { isServiceRunning, startService } from 'tauri-plugin-background-service';

if (!await isServiceRunning()) {
  await startService();
}
```

2. Or stop the existing service first:

```typescript
import { stopService, startService } from 'tauri-plugin-background-service';

await stopService();
await startService();
```

3. If you encounter this unexpectedly, check that a previous `startService()` call succeeded. Listen for the `started` event to confirm:

```typescript
import { onPluginEvent } from 'tauri-plugin-background-service';

const unlisten = await onPluginEvent((event) => {
  if (event.type === 'started') {
    console.log('Service confirmed started');
  }
});
```

**See also:** [API Reference](./api-reference.md#serviceerror)

---

### [All] `ServiceError::NotRunning` when calling `stopService()`

**Symptom:** Calling `stopService()` returns an error:

```json
"NotRunning"
```

Or in Rust:

```
Service is not running
```

**Root cause:** No service is currently active. This happens if the service already completed, was never started, or was already stopped.

**Solution:**

1. Guard the stop call with `isServiceRunning()`:

```typescript
import { isServiceRunning, stopService } from 'tauri-plugin-background-service';

if (await isServiceRunning()) {
  await stopService();
}
```

2. If the service should be running but isn't, check for errors in the `onPluginEvent` listener. The service may have failed during `init()` and emitted an `error` event:

```typescript
import { onPluginEvent } from 'tauri-plugin-background-service';

await onPluginEvent((event) => {
  if (event.type === 'error') {
    console.error('Service error:', event.message);
  }
});
```

3. On Android, check SharedPreferences (`bg_service_label` / `bg_service_type`) — if the keys are absent, the OS killed and failed to restart the service (see the [Android restart troubleshooting](#android-service-does-not-restart-after-os-kills-the-app) entry).

**See also:** [API Reference](./api-reference.md#serviceerror)

---

### [Android] Invalid foreground service type

**Symptom:** `startService()` rejects with an error message like:

```
foreground service type 'specialUse' is not in the configured allowlist [dataSync].
Add it to androidForegroundServiceTypes in your plugin config.
```

**Root cause:** The requested `foregroundServiceType` is not in the `androidForegroundServiceTypes` allowlist configured in `tauri.conf.json`. The plugin validates the type before starting the native service to prevent undeclared types from causing runtime crashes.

**Solution:**

1. Add the type to the allowlist in your plugin config:

```json
{
  "plugins": {
    "background-service": {
      "androidForegroundServiceTypes": ["dataSync", "specialUse"]
    }
  }
}
```

2. Add the corresponding permission to your `AndroidManifest.xml`. See the [Manifest Checklist](./android.md#manifest-checklist) for all types.

3. To skip validation during development, set `androidValidateForegroundServiceType` to `false`:

```json
{
  "plugins": {
    "background-service": {
      "androidValidateForegroundServiceType": false
    }
  }
}
```

**See also:** [Android Platform Guide](./android.md#foreground-service-type-configuration)

---

### [Android] Service stopped by timeout

**Symptom:** The background service stops after approximately 6 hours of cumulative running time on Android 15 (API 35). The `onPluginEvent` handler receives `{ type: "stopped", reason: "timeout" }`.

**Root cause:** Android 15 enforces a 6-hour cumulative time limit for foreground services using the `dataSync` type. When this limit is exceeded, the system calls `onTimeout()` on the foreground service. The plugin stops the service and applies the configured timeout policy.

**Solution:**

1. **Choose a timeout policy** via `androidOnTimeout` in your plugin config:

   - `"notifyUser"` (default) — Posts a high-priority notification so the user can restart the service:
     ```json
     {
       "plugins": {
         "background-service": {
           "androidOnTimeout": "notifyUser"
         }
       }
     }
     ```

   - `"scheduleRecovery"` — Marks recovery as pending so the service can resume on next app launch or reboot:
     ```json
     {
       "plugins": {
         "background-service": {
           "androidOnTimeout": "scheduleRecovery"
         }
       }
     }
     ```

   - `"stop"` — Clean stop with no notification. Use this if your service doesn't need to recover from timeout.

2. **Switch to a different foreground service type** if you need longer uninterrupted execution. Types like `"specialUse"` are not subject to the 6-hour `dataSync` limit (but require Play Console justification).

3. **Listen for timeout events** to handle the stop gracefully in your app:
   ```typescript
   import { onPluginEvent } from 'tauri-plugin-background-service';

   await onPluginEvent((event) => {
     if (event.type === 'stopped' && event.reason === 'timeout') {
       // Optionally restart or notify the user
     }
   });
   ```

**See also:** [Android Platform Guide — Timeout Handling](./android.md#timeout-handling)

---

### [All] Enable debug logging

**Symptom:** You need more visibility into what the plugin is doing internally.

**Solution:**

Set the `RUST_LOG` environment variable to control log output from the plugin:

```bash
# Debug-level logging for the plugin only
RUST_LOG=tauri_plugin_background_service=debug your-app

# Trace-level (very verbose) for the plugin
RUST_LOG=tauri_plugin_background_service=trace your-app

# Debug for the plugin + Tauri framework
RUST_LOG=tauri_plugin_background_service=debug,tauri=debug your-app
```

On Android, view the Kotlin-side lifecycle logs via `adb logcat`:

```bash
adb logcat -s LifecycleService:V
```

For Rust-side debug logging on Android, set `RUST_LOG` before launching the app (e.g., via your IDE's run configuration or `adb shell am start` with `--ez` extras if your app reads it).

Key log messages to look for:
- `handle_start` / `handle_stop` — actor command processing
- `start_keepalive` / `stop_keepalive` — mobile lifecycle calls
- `PluginEvent::Started` / `PluginEvent::Stopped` — lifecycle events
- `Unrecognized foreground service type` (Android only, `Log.w` from `LifecycleService.mapServiceType()`) — invalid service type fallback

**See also:** [API Reference](./api-reference.md#pluginevent)

---

### [iOS] BGProcessingTask never fires

**Symptom:** `BGAppRefreshTask` works but `BGProcessingTask` is never scheduled or executed.

**Root cause:** iOS is selective about when it runs `BGProcessingTask`. It prefers conditions like device charging, connected to Wi-Fi, and idle. Unlike `BGAppRefreshTask`, processing tasks require more favorable system conditions.

**Solution:**

1. Verify both identifiers are in `Info.plist`:

```xml
<key>BGTaskSchedulerPermittedIdentifiers</key>
<array>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-refresh</string>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-processing</string>
</array>
```

2. Verify `UIBackgroundModes` includes `processing`:

```xml
<key>UIBackgroundModes</key>
<array>
    <string>fetch</string>
    <string>processing</string>
</array>
```

3. Test using the Xcode simulate-launch command:

```bash
e -l objc -- (void)[[BGTaskScheduler sharedScheduler] _simulateLaunchForTaskWithIdentifier:@"YOUR_BUNDLE_ID.bg-processing"]
```

4. For real device testing, plug the device into power and leave it idle. Processing tasks are more likely to execute overnight.

**See also:** [iOS Platform Guide](./ios.md#bgprocessingtask-support)

---

### [Desktop] IPC connection failures in OS service mode

**Symptom:** `startService()` returns an IPC error when `desktopServiceMode` is `"osService"`:

```
Ipc: connect failed: No such file or directory
```

**Root cause:** The sidecar process (headless binary) is not running or the socket path is incorrect.

**Solution:**

1. Verify the sidecar binary is installed and running:

```bash
# Linux
systemctl --user status com.example.myapp.background

# macOS
launchctl list | grep com.example.myapp.background
```

2. Check the socket file exists:

```bash
ls -la /tmp/com.example.myapp.background.sock
```

3. Ensure the sidecar was started with the correct `--service-label` argument.

4. Check the `desktopServiceLabel` config matches the label used when installing the service.

**See also:** [Desktop Platform Guide](./desktop.md#os-service-mode)

---

### [Desktop] OS service installed but IPC unavailable

**Symptom:** `getOsServiceStatus()` shows the service as `installed` or even `running`, but `ipcConnected` is `false`. Calling `startService()` returns an IPC error:

```
IPC error: connect failed: No such file or directory
```

**Root cause:** The sidecar process is not running or the IPC socket path is incorrect. The OS service can be in a "running" state according to the service manager without the sidecar actually being ready to accept IPC connections.

**Solution:**

1. Check the service status using `getOsServiceStatus()`:

```typescript
const status = await getOsServiceStatus();
console.log(status.installed);    // "notInstalled" | "installed" | "running"
console.log(status.ipcConnected); // true | false
console.log(status.socketPath);   // Check the socket path
console.log(status.lastError);    // Any error from the service
```

2. Restart the OS service and wait briefly:

```typescript
await restartOsService();
// Wait for the sidecar to initialize before calling startService()
```

3. If `desktopStartServiceIfMissing` is `true` in your config, the plugin attempts to start the OS service automatically when `startService()` is called with a disconnected IPC connection. Check the timeout setting (`desktopServiceStartTimeoutMs`) — the default is 5000 ms.

4. Verify the socket file exists at the reported path:

```bash
ls -la "$XDG_RUNTIME_DIR/com.example.myapp.background.sock"
```

5. Check the OS service logs for startup errors:

```bash
# Linux
journalctl --user -u com.example.myapp.background

# macOS
log show --predicate 'subsystem == "com.example.myapp.background"' --last 5m
```

**See also:** [Desktop Platform Guide — OS Service Management API](./desktop.md#os-service-management-api)

---

### [Desktop] startService returns ipcUnavailable after timeout

**Symptom:** `startService()` returns an IPC error containing `ipcUnavailable` and a socket path, even though `desktopStartServiceIfMissing` is `true`:

```
IPC error: ipcUnavailable: socket /run/user/1000/com.example.myapp.background.sock
```

**Root cause:** The OS service was started by the automatic sidecar recovery mechanism, but the IPC socket did not become ready within `desktopServiceStartTimeoutMs` (default: 5000 ms). This can happen if the sidecar binary takes longer than expected to initialize — for example, loading large WASM modules, setting up network connections, or waiting for external resources.

**Solution:**

1. Increase the timeout in your plugin config:

```json
{
  "plugins": {
    "background-service": {
      "desktopServiceStartTimeoutMs": 10000
    }
  }
}
```

2. Check the OS service logs to see why initialization is slow:

```bash
# Linux
journalctl --user -u com.example.myapp.background --since "5 minutes ago"

# macOS
log show --predicate 'subsystem == "com.example.myapp.background"' --last 5m
```

3. Verify the service is actually running after the timeout:

```typescript
import { getOsServiceStatus } from 'tauri-plugin-background-service';

const status = await getOsServiceStatus();
if (status.installed === 'running' && !status.ipcConnected) {
  // Service is running but IPC is not connecting — socket path issue
  console.log('Socket path:', status.socketPath);
}
```

4. If the socket file never appears, the sidecar binary may be crashing on startup. Check the service logs for panic messages or missing dependencies.

**See also:** [Desktop Platform Guide — Automatic Sidecar Recovery](./desktop.md#automatic-sidecar-recovery)

---

### [Desktop] Windows: OS-service mode is not supported

**Symptom:** Calling `startOsService()`, `stopOsService()`, `restartOsService()`, or `getOsServiceStatus()` on Windows returns:

```
Platform error: Windows OS-service mode is not yet supported
```

**Root cause:** The OS-service mode (`desktopServiceMode: "osService"`) currently only supports Linux (systemd) and macOS (launchd). Windows Service integration is not yet implemented.

**Solution:**

1. Use the default in-process mode (`desktopServiceMode: "inProcess"`) on Windows. The background service runs as a standard async task in the app process — no OS service registration is needed.

2. To run the service in the background on Windows, use the close-to-tray pattern: intercept `CloseRequested` on the main window, hide the window, and keep the process alive via a system tray icon.

3. `installService()` and `uninstallService()` still work on Windows for future compatibility — but `startOsService()`, `stopOsService()`, `restartOsService()`, and `getOsServiceStatus()` are Unix-only for now.

**See also:** [Desktop Platform Guide — Platform Support](./desktop.md#platform-support)

---

### [Desktop] Service install permission errors

**Symptom:** `installService()` fails with a permission error.

**Root cause:** Installing OS-level services requires appropriate permissions. On Linux, the user must have access to systemd --user (typically available without sudo). On macOS, launchd user agents should not require elevated permissions.

**Solution:**

1. On Linux, verify the user's systemd session is active:

```bash
systemctl --user status
```

2. On macOS, check that the launchd agent plist is in the correct directory (`~/Library/LaunchAgents/`).

3. If using a system-level service (not user-level), you may need elevated permissions. Consider using a user-level service instead.

4. Check logs for the specific error message from the `service-manager` crate.

**See also:** [Desktop Platform Guide](./desktop.md#os-service-mode)
