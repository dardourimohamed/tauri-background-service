# Code Review: Runtime Verification via AutoGLM/Waydroid

## Files Reviewed
- [x] tauri-plugin-background-service/src/runner.rs
- [x] tauri-plugin-background-service/src/models.rs
- [x] tauri-plugin-background-service/src/lib.rs
- [x] tauri-plugin-background-service/android/src/main/kotlin/app/tauri/backgroundservice/LifecycleService.kt
- [x] tauri-plugin-background-service/android/src/main/kotlin/app/tauri/backgroundservice/BackgroundServicePlugin.kt
- [x] test-app/run-tests.py
- [x] test-app/index.html
- [x] test-app/src-tauri/src/lib.rs

## Summary
APPROVE with one significant concern requiring deep analysis.

## Test Results (2026-04-02T18:04:35Z)

| Tier | Passed | Total |
|------|--------|-------|
| Core (must pass) | 5/5 | 5 |
| Lifecycle (should pass) | 2/2 | 2 |
| Edge (informational) | -- | 3 |

### Per-Test Breakdown
| ID | Tier | Result | Notes |
|----|------|--------|-------|
| T1 | core | PASS | App opens, shows Stopped state |
| T2 | core | PASS | Start Service works, status Running, tick count > 0 |
| T3 | core | PASS | Check Status confirms Running |
| T4 | core | PASS | Event log shows tick events with timestamps |
| T5 | core | PASS | Stop Service works, status Stopped |
| T6 | lifecycle | PASS | Double-stop: no crash, remains Stopped |
| T7 | lifecycle | PASS | Double-start: no crash, remains Running |
| T8 | edge | INFO | Force-stop + reopen: verification inconclusive |
| T9 | edge | INFO | Max steps reached (KDE Connect dialogs interfere) |
| T10 | edge | INFO | 5 ticks in 15s run, clean stop confirmed |

### Manual Verification
- Rapid stop->start race condition: PASS (generation counter fix works)
- Foreground service notification: NOT VERIFIED (see Critical Finding #1)

## Critical Issues (Must Investigate)

### 1. LifecycleService not visible in dumpsys activity services
**Severity:** High
**Evidence:** After starting the service (green Running status, tick count incrementing), `dumpsys activity services` does NOT list `app.tauri.backgroundservice.LifecycleService`. Only the WebView sandboxed process is listed.
**Impact:** If the foreground service is not actually running, the Android OS can kill the process at any time, and the auto-restart (START_STICKY) mechanism will not work. The Rust Tokio task works fine, but the OS-level protection is missing.
**Possible Causes:**
- Waydroid does not fully track foreground services in dumpsys
- The service starts but immediately stops (startForeground() not called in time)
- The Intent is malformed or the service crashes silently
**Confidence:** 50 (could be a Waydroid artifact)

## Suggestions (Should Consider)

### 1. Add logging to LifecycleService
The LifecycleService has almost no logging (only a Log.w in mapServiceType for unrecognized types). Adding Log.d calls in onStartCommand, handleOsRestart, and startForegroundTyped would make debugging much easier.

### 2. T9 test harness needs Waydroid-specific handling
The KDE Connect dialog keeps popping up in Waydroid and confuses the AutoGLM agent.

### 3. Test coverage gap: actual foreground service behavior
No test verifies that the foreground notification actually appears or that the OS keeps the service alive when the app is backgrounded.

## Deep Analysis: Foreground Service Lifecycle (2026-04-02T19:30Z)

### Verdict: FALSE ALARM — Foreground service works correctly in Waydroid

The primary review finding that "LifecycleService not visible in dumpsys" was a **timing artifact**: the service had already been stopped before the dumpsys check was performed. Deep analysis with live testing confirms the foreground service lifecycle is fully functional.

### Evidence

#### 1. Service IS visible in dumpsys when running
After starting the service via the app UI and checking dumpsys immediately:
```
ServiceRecord{48d8217 u0 com.test.backgroundservice/app.tauri.backgroundservice.LifecycleService}
  app=ProcessRecord{fc7963e 10236:com.test.backgroundservice/u0a159}
  startForegroundCount=1
  isForeground=true foregroundId=9001
  foregroundNoti=Notification(channel=bg_keepalive ...)
  createTime=-2s39ms
```

#### 2. Historical FOREGROUND_SERVICE_START/STOP events confirm lifecycle
```
time="2026-04-02 18:52:47" type=FOREGROUND_SERVICE_START ... LifecycleService
time="2026-04-02 18:54:12" type=FOREGROUND_SERVICE_STOP  ... LifecycleService
time="2026-04-02 18:55:01" type=FOREGROUND_SERVICE_START ... LifecycleService
time="2026-04-02 18:55:28" type=FOREGROUND_SERVICE_STOP  ... LifecycleService
time="2026-04-02 19:04:00" type=FOREGROUND_SERVICE_START ... LifecycleService
time="2026-04-02 19:04:25" type=FOREGROUND_SERVICE_STOP  ... LifecycleService
```

#### 3. Notification channel confirms foreground notification was posted
```
NotificationChannel{mId='bg_keepalive', ..., mFgServiceShown=true, ...}
```

#### 4. Service stats show consistent running across time windows
```
Svc app.tauri.backgroundservice.LifecycleService:
  Running count 3 / time 5.9%   (last 3 hours)
  Running count 9 / time 43%    (last 24 hours)
  Running count 4 / time 81%    (since boot)
```

### Adversarial Tests — All Passed

| Test | Result | Evidence |
|------|--------|----------|
| Fresh start → dumpsys check | PASS | `isForeground=true`, `startForegroundCount=1`, prompt `createTime` |
| Background app → service survival | PASS | Tick count continued 6→22 after HOME key press |
| Rapid stop→start race | PASS | Service restarted cleanly, tick count reset to 3, no errors |
| OOM priority | PASS | `oom_adj=0`, `oom_score_adj=0` (foreground priority) |
| START_STICKY flag | PASS | `stopIfKilled=false`, `callStart=true` in ServiceRecord |

### Code Review: No Issues Found

- `onStartCommand`: Correctly handles ACTION_STOP, null intent (OS restart), and normal start
- `startForegroundTyped`: Properly handles API level Q+ vs older
- `handleOsRestart`: Saves auto-start prefs, calls startForeground within 5s, re-launches Activity
- `onDestroy`: Resets `isRunning` and `autoRestarting` flags
- `onTimeout` (Android 14+): Stops foreground gracefully
- `buildNotification`: Creates proper ongoing notification with PendingIntent

### Root Cause of Original Finding
The primary reviewer checked `dumpsys activity services app.tauri.backgroundservice` **after the service had already been stopped**. When the service IS running, it appears correctly in dumpsys with all expected foreground service attributes. This is not a Waydroid limitation — it's expected behavior (stopped services don't appear in active services).

## Positive Notes
- Generation counter in runner.rs correctly prevents stop->start race conditions
- Token cleanup is generation-guarded to avoid clearing the new service token
- on_complete callback is captured at spawn time, preventing stale callback issues
- init() failure path correctly fires callback with false and clears token
- Serde models have comprehensive unit test coverage
- AutoStartConfig correctly handles null/missing fields with Option<String>
