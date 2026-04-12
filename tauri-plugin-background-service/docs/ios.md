# iOS Platform Guide

This guide covers iOS-specific behavior for the background service plugin, including the BGTaskScheduler architecture, dual task support (BGAppRefreshTask + BGProcessingTask), timeout configuration, limitations, and debugging.

## How It Works

On iOS, the plugin uses Apple's `BGTaskScheduler` API with **two task types** for background execution:

1. **`BGAppRefreshTask`** — Short periodic work (~30 seconds). Registered as `{bundleIdentifier}.bg-refresh`.
2. **`BGProcessingTask`** — Longer maintenance tasks (minutes to hours). Registered as `{bundleIdentifier}.bg-processing`.

iOS background execution is fundamentally different from Android: the OS controls when and for how long your code runs. The plugin registers handlers for both task types and automatically schedules the next task after each completion.

### Architecture

```
JS: startService()
  → Tauri Command (start)
    → Actor: handle_start()
      → MobileLifecycle.start_keepalive()
        → BackgroundServicePlugin.startKeepalive()
          → BGTaskScheduler.shared.register() for both identifiers
          → scheduleNext() submits both BGAppRefreshTaskRequest + BGProcessingTaskRequest

iOS calls handleBackgroundTask() or handleProcessingTask():
  → Sets expiration handler
  → Starts safety timer (BGAppRefresh: 28.0s default, BGProcessing: configurable)
  → Stores BGTask reference
  → Rust runs service.run() in background

On expiration:
  → expirationHandler fires
  → Resolves pending waitForCancel invoke
  → Rust receives cancel signal → stop()
  → on_complete → completeBgTask()
  → BGTask.setTaskCompleted(success: false)
  → scheduleNext() for next window
```

## Foreground vs Background Behavior

### Foreground (App Active)

When the app is in the foreground, the service runs **continuously** with no time limits. The `BGTaskScheduler` registration still occurs, but the service task runs as a normal async task.

### Background (App Suspended)

When the app moves to the background, iOS gives you **short execution windows** (typically ~30 seconds) controlled by `BGAppRefreshTask`. Between these windows, your app is suspended and receives no CPU time.

Key constraints:
- **Execution window**: ~30 seconds per background task (the plugin uses a 28.0s safety timeout by default)
- **Minimum interval**: 15 minutes between scheduled task executions (`earliestBeginDate`)
- **No guarantee**: iOS decides whether to launch your task based on system conditions (battery, usage patterns, time of day)

## Required Info.plist Entries

Add the following entries to your app's `Info.plist`:

### 1. Background Modes

```xml
<key>UIBackgroundModes</key>
<array>
    <string>fetch</string>
    <string>processing</string>
</array>
```

### 2. Permitted Task Identifiers

The plugin uses two task identifiers based on your bundle identifier. You must declare both:

```xml
<key>BGTaskSchedulerPermittedIdentifiers</key>
<array>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-refresh</string>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-processing</string>
</array>
```

For example, if your bundle identifier is `com.example.myapp`, the task identifier will be `com.example.myapp.bg-refresh`.

## BGProcessingTask Support

Starting from version 0.2, the plugin registers a `BGProcessingTask` handler alongside the existing `BGAppRefreshTask`. This provides access to longer execution windows under specific system conditions.

### How BGProcessingTask differs from BGAppRefreshTask

| Aspect | BGAppRefreshTask | BGProcessingTask |
|--------|-----------------|-----------------|
| Duration | ~30 seconds | Minutes to hours |
| Requires charging | No | Recommended |
| Requires network | No | Optional |
| System conditions | Any | Preferably idle, charging |
| Use case | Quick sync, data refresh | ML training, database maintenance, large downloads |

### Automatic orchestration

The plugin registers both task handlers and submits scheduling requests for both types after each task completion. iOS guarantees at most one BGTask is active at a time — the plugin uses a single `pendingCancelInvoke` and single safety timer for whichever task type is currently running.

### Processing safety timeout

By default, `BGProcessingTask` has **no safety timeout** (the plugin does not impose a cap). You can configure one via `iosProcessingSafetyTimeoutSecs`:

```json
{
    "plugins": {
        "background-service": {
            "iosProcessingSafetyTimeoutSecs": 600
        }
    }
}
```

Set to `0` (default) for no timeout, or a positive value in seconds to cap processing task execution time.

## Timeout Configuration

The plugin has several configurable values set via `PluginConfig` in your Tauri plugin configuration:

### All iOS PluginConfig Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `iosSafetyTimeoutSecs` | `f64` | `28.0` | Safety timeout for `BGAppRefreshTask` expiration handler |
| `iosCancelListenerTimeoutSecs` | `u64` | `14400` | Cancel listener max wait in seconds (4 hours) |
| `iosProcessingSafetyTimeoutSecs` | `f64` | `0.0` | Safety timeout for `BGProcessingTask` (`0.0` = no cap) |
| `iosEarliestRefreshBeginMinutes` | `f64` | `15.0` | Min delay (minutes) before `BGAppRefreshTask` is scheduled |
| `iosEarliestProcessingBeginMinutes` | `f64` | `15.0` | Min delay (minutes) before `BGProcessingTask` is scheduled |
| `iosRequiresExternalPower` | `bool` | `false` | Whether `BGProcessingTask` requires charging |
| `iosRequiresNetworkConnectivity` | `bool` | `false` | Whether `BGProcessingTask` requires network |

### `iosSafetyTimeoutSecs`

- **Default**: `28.0` seconds
- **Purpose**: Safety timer that fires if the Rust service doesn't complete within the expected BGTask window. Prevents iOS from killing the app for exceeding the background execution limit.
- **When it fires**: The expiration handler is called and the BGTask is completed with `success: false`.
- **Recommendation**: Keep at or below 28.0. Apple recommends finishing BG tasks before the ~30 second system limit.

### `iosCancelListenerTimeoutSecs`

- **Default**: `14400` seconds (4 hours)
- **Purpose**: Maximum time the cancel listener thread will wait for an iOS expiration signal. Prevents indefinite thread leaks if iOS kills the app without firing the expiration handler.
- **When it fires**: The `waitForCancel` pending invoke is rejected and the cancel listener exits.
- **Recommendation**: Leave at the default unless you have specific requirements.

### `iosProcessingSafetyTimeoutSecs`

- **Default**: `0.0` (no cap)
- **Purpose**: Safety timeout for `BGProcessingTask` execution. When set to a positive value, the plugin caps processing task runtime. When `0.0`, the processing task has no plugin-imposed timeout (iOS manages the lifetime).
- **Recommendation**: Leave at `0.0` for processing tasks that benefit from long runtimes. Set a positive value if you need bounded execution.

### Setting Custom Values

In your Tauri plugin configuration (`tauri.conf.json` or equivalent):

```json
{
    "plugins": {
        "background-service": {
            "iosSafetyTimeoutSecs": 20.0,
            "iosCancelListenerTimeoutSecs": 7200,
            "iosProcessingSafetyTimeoutSecs": 600,
            "iosEarliestRefreshBeginMinutes": 15.0,
            "iosEarliestProcessingBeginMinutes": 30.0,
            "iosRequiresExternalPower": true,
            "iosRequiresNetworkConnectivity": false
        }
    }
}
```

## Cancellation Flow

iOS cancellation uses the **Pending Invoke pattern**:

1. When a `BGAppRefreshTask` starts, the plugin stores the task reference and sets an expiration handler.
2. The Rust side spawns a `spawn_blocking` thread that calls `waitForCancel()`. This stores an `Invoke` object without resolving it, which blocks the thread.
3. When iOS fires the expiration handler (system is about to suspend the task):
   - The stored invoke is **resolved** (unblocking the Rust thread)
   - Rust receives the signal and calls `stop()`
   - The `on_complete` callback fires `completeBgTask(success: false)` on the Swift side
   - `BGTask.setTaskCompleted(success: false)` is called
   - `scheduleNext()` queues the next background task
4. If the safety timer fires first (Rust didn't complete in time):
   - The stored invoke is **rejected** (unblocking the Rust thread)
   - The BGTask is completed with `success: false`
   - Next task is scheduled

## Limitations

### No Guaranteed Execution

iOS decides when (or if) your background task runs. Factors that reduce execution frequency:
- Low battery or Power Saver mode
- App not recently used by the user
- System under heavy load
- Device in low-power state overnight

**Do not** rely on iOS background execution for time-critical operations. It is suitable for opportunistic sync, data refresh, and maintenance tasks.

### No Auto-Restart

Unlike Android, iOS does **not** automatically restart your service after the app is killed. The plugin schedules the next `BGAppRefreshTask` after each completion, but iOS may never invoke it.

### Simulator vs Device

- The **simulator** runs background tasks more frequently than real devices. Behavior on the simulator is not representative of production.
- To test on device, use the Xcode debugger to trigger background tasks:

```bash
# Trigger a background app refresh immediately (device connected to Xcode)
e -l objc -- (void)[[BGTaskScheduler sharedScheduler] _simulateLaunchForTaskWithIdentifier:@"YOUR_BUNDLE_ID.bg-refresh"]
```

### ~30 Second Window

Each `BGAppRefreshTask` gives you approximately 30 seconds of execution. The plugin's safety timeout defaults to 28.0 seconds to provide a 2-second buffer for cleanup. Your `run()` method should:

1. Check `ctx.shutdown.cancelled()` frequently (via `tokio::select!`)
2. Complete work incrementally rather than in one long operation
3. Use the `Notifier` to inform the user of progress if needed

## Notification Permission

The plugin requests notification authorization in `BackgroundServicePlugin.load()` with `.alert`, `.sound`, and `.badge` options. This enables the `Notifier` API to display local notifications from your background service.

No additional code is needed. If the user denies the permission, notifications won't appear but the service will still function.

## Debugging

### Check Background Task Registration

In Xcode, check that your task identifier is registered:

```swift
// In the debugger console:
po BGTaskScheduler.shared.registeredTaskIdentifiers
```

### Force a Background Task (Simulator)

```bash
# Simulate BGAppRefreshTask
e -l objc -- (void)[[BGTaskScheduler sharedScheduler] _simulateLaunchForTaskWithIdentifier:@"com.example.myapp.bg-refresh"]

# Simulate BGProcessingTask
e -l objc -- (void)[[BGTaskScheduler sharedScheduler] _simulateLaunchForTaskWithIdentifier:@"com.example.myapp.bg-processing"]
```

### Force a Background Task (Xcode Scheme)

1. Edit your scheme in Xcode (**Product → Scheme → Edit Scheme**)
2. Under **Run → Options**, check **Background Fetch**
3. Launch the app from Xcode — it will launch directly into background mode

### Check Task Scheduling

```swift
po BGTaskScheduler.shared.pendingTaskRequests()
```

### Common Issues

**Background task never executes on device:**
- Verify `BGTaskSchedulerPermittedIdentifiers` in Info.plist includes **both** `{bundleIdentifier}.bg-refresh` and `{bundleIdentifier}.bg-processing`
- Ensure `UIBackgroundModes` includes both `fetch` and `processing`
- Background tasks are rate-limited by iOS — they may not run for hours
- Test using the Xcode simulate-launch command above

**Service runs in foreground but not in background:**
- This is expected behavior. iOS limits background execution to ~30 seconds
- The expiration handler fires, service is cancelled, and next task is scheduled
- Check that `iosSafetyTimeoutSecs` is set appropriately (default 28.0)

**Thread leak warnings:**
- Verify `iosCancelListenerTimeoutSecs` is set (default 14400)
- The cancel listener will timeout and clean up after the configured duration
- This timeout prevents indefinite blocking if iOS kills the app without signaling
