# iOS Platform Guide

This guide covers iOS-specific behavior for the background service plugin, including scheduled background execution via BGTaskScheduler, dual task support (BGAppRefreshTask + BGProcessingTask), scheduling results, desired-state persistence, timeout configuration, limitations, and debugging.

## How It Works

On iOS, the plugin uses Apple's `BGTaskScheduler` API with **two task types** for **opportunistic, scheduled background execution**:

1. **`BGAppRefreshTask`** — Short periodic work (~30 seconds). Registered as `{bundleIdentifier}.bg-refresh`.
2. **`BGProcessingTask`** — Longer maintenance tasks (minutes to hours). Registered as `{bundleIdentifier}.bg-processing`.

iOS background execution is fundamentally different from Android: the OS controls when and for how long your code runs. The plugin registers handlers for both task types and automatically schedules the next task after each completion. **iOS cannot guarantee continuous background execution** — your service runs in short, opportunistic windows controlled entirely by the system.

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

## Background Task Lifecycle

The following diagram shows the complete lifecycle from background transition through BGTask execution:

```
┌─────────────────────────────────────────────────────────┐
│  App in Foreground                                       │
│  Service runs continuously (no time limits)              │
└─────────────────┬───────────────────────────────────────┘
                  │ User backgrounds app
                  ▼
┌─────────────────────────────────────────────────────────┐
│  appDidEnterBackground                                   │
│  Plugin detects desired_running=true, no active BGTask   │
│  → scheduleNext() submits BGAppRefreshTaskRequest        │
│                    + BGProcessingTaskRequest              │
└─────────────────┬───────────────────────────────────────┘
                  │ iOS decides to launch (minutes/hours later)
                  ▼
┌─────────────────────────────────────────────────────────┐
│  iOS launches app for BGTask                             │
│  handleBackgroundTask() or handleProcessingTask() fires  │
│  → Stores PendingTaskInfo (taskKind, identifier, time)   │
│  → Sets expirationHandler                               │
│  → Starts safety timer (refresh: 28s, processing: opt)   │
└─────────────────┬───────────────────────────────────────┘
                  │
                  ▼
┌─────────────────────────────────────────────────────────┐
│  Rust Auto-Start                                         │
│  Plugin setup calls getPendingBgTask()                   │
│  → PendingTaskInfo found                                │
│  → Checks ios_desired_running in UserDefaults            │
│  → If true: sends ManagerCommand::Start with stored cfg  │
│  → Sets on_complete callback for completeBgTask()        │
│  → Spawns cancel listener (waitForCancel)                │
│  → Calls clearPendingBgTask()                            │
└─────────────────┬───────────────────────────────────────┘
                  │
                  ▼
┌─────────────────────────────────────────────────────────┐
│  Service Execution                                       │
│  BackgroundService::run() executes with CancellationToken│
│  Must check ctx.shutdown.cancelled() via tokio::select!  │
└────┬────────────────────────┬───────────────────────────┘
     │                        │
     ▼                        ▼
┌──────────────┐   ┌──────────────────────┐
│ Natural      │   │ Expiration / Timeout │
│ Completion   │   │ iOS fires expiration │
│ run() returns│   │ handler or safety    │
│ on_complete  │   │ timer fires          │
│ → setTask    │   │ → resolves waitFor   │
│ Completed    │   │   Cancel             │
│ (success:    │   │ → Rust cancels via   │
│  true)       │   │   CancellationToken  │
│              │   │ → on_complete        │
│              │   │ → setTaskCompleted   │
│              │   │   (success: false)   │
└──────┬───────┘   └──────────┬───────────┘
       │                      │
       └──────────┬───────────┘
                  ▼
┌─────────────────────────────────────────────────────────┐
│  Post-Task Cleanup                                       │
│  scheduleNext() queues next BGAppRefreshTaskRequest      │
│                    + BGProcessingTaskRequest              │
│  Cleanup resets: task refs, cancel invoke, safety timer  │
└─────────────────────────────────────────────────────────┘
```

### Foreground/Background Transitions

The plugin observes `UIApplication.didEnterBackgroundNotification` and `UIApplication.willEnterForegroundNotification` to manage the scheduling lifecycle:

| Transition | Behavior |
|------------|----------|
| **Foreground → Background** | If `ios_desired_running == true` and no BGTask is currently active, the plugin calls `scheduleNext()` to submit both `BGAppRefreshTaskRequest` and `BGProcessingTaskRequest`. This ensures iOS has scheduled tasks that may relaunch the app. |
| **Background → Foreground** | No special action. The service continues running uninterrupted while the app is active. |

When the app transitions to the background with `desired_running=true`, the scheduling ensures iOS can potentially relaunch the app for a background task. When iOS does relaunch the app, the pending task bridge (described above) auto-starts the Rust service.

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

## Completion Safety

iOS requires `BGTask.setTaskCompleted(success:)` to be called **exactly once** per active background task. Calling it zero times causes iOS to kill the app process. Calling it twice causes undefined behavior.

The plugin uses a `completeActiveTask(success:)` guard that ensures exactly-once completion across all terminal paths:

| Terminal Path | What Triggers | `setTaskCompleted` Called |
|---------------|---------------|---------------------------|
| **Expiration** | iOS fires `expirationHandler` (~30s for refresh, system-decided for processing) | `completeActiveTask(success: false)` |
| **Safety timer** | Plugin's internal timer fires before expiration (28s default for refresh, configurable for processing) | `completeActiveTask(success: false)` |
| **Natural completion** | Rust `run()` returns normally | `completeActiveTask(success: true)` via `on_complete` callback |
| **Explicit stop** | `stopService()` called from JS/Rust | `completeActiveTask(success: false)` via `stopKeepalive()` |

The guard works by:

1. Checking a `taskCompleted` flag before calling `setTaskCompleted`
2. Setting the flag to `true` on first call and nil-ing the task reference
3. Returning `false` (no-op) on subsequent calls
4. Resetting the flag when a new BGTask handler fires or during cleanup

This prevents double-completion in edge cases like:
- Expiration handler fires while `completeBgTask` is in flight
- Safety timer fires simultaneously with natural Rust completion
- `stopService()` called after expiration already completed the task
- `completeBgTask` called after `stopKeepalive` already cleaned up

## Scheduling Results

When you call `startService()` on iOS, the plugin submits both a `BGAppRefreshTaskRequest` and a `BGProcessingTaskRequest` to `BGTaskScheduler`. The `startKeepalive` call returns a structured result indicating which tasks were successfully scheduled:

```typescript
const status = await getSchedulingStatus();
// {
//   refreshScheduled: true,
//   processingScheduled: true,
//   refreshError: undefined,
//   processingError: undefined
// }
```

### Partial Success

It is possible for one task type to be scheduled while the other fails. For example, `BGAppRefreshTask` may succeed while `BGProcessingTask` fails if the system conditions aren't favorable. The plugin logs partial failures as warnings and continues — at least one scheduled task type is enough for the service to run.

When both scheduling attempts fail, the Swift layer rejects the invoke with `"schedulerUnavailable"`, which surfaces as `ServiceError::Platform("schedulerUnavailable")` in Rust. This typically means:

- `BGTaskSchedulerPermittedIdentifiers` is missing from `Info.plist`
- `UIBackgroundModes` doesn't include `fetch` and/or `processing`
- The app is running in a context where BGTaskScheduler is unavailable (e.g. App Extension)

### Querying Scheduling Status

Use `getSchedulingStatus()` to check the current scheduling state at any time:

```typescript
import { getSchedulingStatus } from 'tauri-plugin-background-service';

const status = await getSchedulingStatus();
if (!status.refreshScheduled && !status.processingScheduled) {
  console.warn('No background tasks scheduled:', status.refreshError, status.processingError);
}
```

On non-iOS platforms, `getSchedulingStatus()` returns a default status with all fields set to `false`/`undefined`.

## Desired State

The plugin persists scheduling intent to `UserDefaults` so it can detect whether the service should be running across app launches. This is the iOS equivalent of Android's `SharedPreferences`-based durable state.

### Persisted Keys

| Key | Type | Set When |
|-----|------|----------|
| `ios_desired_running` | `Bool` | `startService()` sets `true`; `stopService()` sets `false` |
| `ios_last_start_config` | `String` (JSON) | `startService()` with the start configuration |
| `ios_last_schedule_error` | `String` | `startService()` if either scheduling attempt fails |
| `ios_last_task_kind` | `String` | BGTask handler fires (`"refresh"` or `"processing"`) |
| `ios_last_task_started_at` | `Double` (epoch) | BGTask handler fires |
| `ios_last_task_completed_at` | `Double` (epoch) | Expiration handler, safety timer, or `stopService()` |

### Lifecycle

- `startService()` sets `ios_desired_running = true`, stores the start config, and clears `ios_last_task_completed_at`.
- Each time iOS launches a BGTask, the plugin records the task kind and start time.
- When the task completes (expiration, safety timer, or explicit stop), `ios_last_task_completed_at` is set.
- `stopService()` sets `ios_desired_running = false` and records the completion time.

### Limitations of Desired State

- `UserDefaults` is **not** written when the user force-quits the app. Force-quit kills all background tasks immediately — iOS will not relaunch force-killed apps.
- Desired state persists across normal app launches, but iOS provides no mechanism to auto-start your app based on it. It is informational only, useful for displaying UI state (e.g. "Background service was running before — restart?").
- The values are only as recent as the last successful persistence call. If the app crashes between writes, some keys may be stale.

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

### Force-Quit Kills Everything

When the user force-quits the app (swipe-up in app switcher), iOS:
1. Immediately terminates all background tasks (`setTaskCompleted` is never called).
2. Removes the app from BGTaskScheduler's eligible pool until the user manually launches it again.
3. Does **not** deliver any callback or notification — the app process is simply killed.

This is an iOS design limitation. There is no workaround. Only `location`, `audio`, and `VoIP` background modes can relaunch after force-quit, and App Store review requires legitimate use of these modes.

### No Continuous Background Execution

iOS does not support continuous background execution for general-purpose tasks. Each `BGAppRefreshTask` gives you ~30 seconds. Each `BGProcessingTask` can run longer but requires specific system conditions (device idle, charging). The OS may revoke execution at any time via the expiration handler.

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
