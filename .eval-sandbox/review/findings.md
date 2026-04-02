# Code Review: tauri-plugin-background-service (Fresh Full Review)

## Files Reviewed
- [x] tauri-plugin-background-service/src/lib.rs
- [x] tauri-plugin-background-service/src/runner.rs
- [x] tauri-plugin-background-service/src/models.rs
- [x] tauri-plugin-background-service/src/error.rs
- [x] tauri-plugin-background-service/src/mobile.rs
- [x] tauri-plugin-background-service/src/notifier.rs
- [x] tauri-plugin-background-service/src/service_trait.rs
- [x] tauri-plugin-background-service/build.rs
- [x] tauri-plugin-background-service/Cargo.toml
- [x] tauri-plugin-background-service/ios/Sources/TauriPluginBackgroundService/BackgroundServicePlugin.swift
- [x] tauri-plugin-background-service/android/src/main/kotlin/app/tauri/backgroundservice/BackgroundServicePlugin.kt
- [x] tauri-plugin-background-service/android/src/main/kotlin/app/tauri/backgroundservice/LifecycleService.kt
- [x] tauri-plugin-background-service/android/src/main/AndroidManifest.xml
- [x] tauri-plugin-background-service/permissions/default.toml
- [x] tauri-plugin-background-service/tests/integration.rs
- [x] tauri-plugin-background-service/examples/basic_service.rs
- [x] tauri-plugin-background-service/guest-js/index.ts
- [x] tauri-plugin-background-service/guest-js/dist-js/index.d.ts
- [x] test-app/src-tauri/src/lib.rs

## Test Results
- **Unit tests:** 42/42 passed
- **Integration tests:** 12/12 passed
- **Clippy:** Clean (only example dead_code warnings)

## Summary
APPROVE with suggestions. All previously identified critical/high bugs have been fixed. The codebase is well-structured with comprehensive test coverage. Two medium-severity issues remain.

## Previously Fixed Issues (Verified)
- ✅ iOS `waitForCancel` now stores invoke unconditionally (was: resolved immediately when no BGTask)
- ✅ `onTimeout()` override added for Android 15+ (was: missing, caused ANR)
- ✅ `startForegroundTyped()` with API-level branching (was: 2-param overload)
- ✅ `stopForeground(STOP_FOREGROUND_REMOVE)` before `stopSelf()` in ACTION_STOP (was: notification linger)
- ✅ Swift force-cast replaced with `if let` guard (was: potential crash)
- ✅ `isRunning` set during OS restart (was: false during restart)
- ✅ `Acquire/Release` ordering on generation counter (was: SeqCst)

## Remaining Issues

### MEDIUM: Guest JS `StartConfig` missing `foregroundServiceType`
**Location:** `guest-js/index.ts:5-8` and `guest-js/dist-js/index.d.ts:2-5`

The TypeScript interface is:
```typescript
export interface StartConfig {
  serviceLabel?: string;
}
```

But Rust accepts `foregroundServiceType` and Kotlin uses it. JS users cannot configure the Android foreground service type from the frontend. The value silently defaults to `"dataSync"`.

### MEDIUM: Android OS restart loses foreground service type
**Location:** `BackgroundServicePlugin.kt:54-57` + `LifecycleService.kt:82-84`

`startKeepalive` only persists `bg_service_label` to SharedPreferences, NOT the foreground service type. After OS restart, `handleOsRestart()` calls `startForegroundTyped` with hardcoded `FOREGROUND_SERVICE_TYPE_DATA_SYNC`, and the Rust auto-start path also defaults to `"dataSync"`. Users who configured `"specialUse"` will lose their setting after OS restart.

### LOW: Example dead_code warnings
**Location:** `examples/basic_service.rs:19,24`

Clippy warns about unused `ExampleService` struct and `new()` method. Expected for an example binary.

## Deep Analysis: foregroundServiceType Gap (Step 2)

### MEDIUM: Android OS restart silently downgrades foreground service type (confirmed)

Full root-cause chain traced across 4 layers:

1. **Kotlin `startKeepalive()`** (`BackgroundServicePlugin.kt:54-56`): Persists `bg_service_label` to SharedPreferences but does NOT persist `foregroundServiceType`. The `EXTRA_SERVICE_TYPE` is passed via Intent to LifecycleService but never written to durable storage.

2. **Kotlin `handleOsRestart()`** (`LifecycleService.kt:84`): Hardcodes `ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC` for the initial `startForeground` call. No way to know what was originally configured.

3. **Kotlin `getAutoStartConfig()`** (`BackgroundServicePlugin.kt:73-79`): `GetAutoStartConfigResult` only carries `pending` and `label` — no service type field.

4. **Rust `AutoStartConfig::into_start_config()`** (`models.rs:78-80`): Always calls `default_foreground_service_type()` (returns `"dataSync"`), regardless of what was originally configured.

**Adversarial scenario:**
- App configures `specialUse` for Google Play policy compliance (e.g., for a feature that doesn't fit other service types).
- OS kills service due to memory pressure or battery optimization.
- `handleOsRestart` starts with `DATA_SYNC` immediately (Android 12+ requirement).
- Activity relaunches → Rust auto-start reads `AutoStartConfig` → always gets `"dataSync"`.
- `startKeepalive` sends `"dataSync"` to LifecycleService → `onStartCommand` uses `DATA_SYNC` again.
- Service now runs permanently with wrong type. User never notified.

**Impact:** Google Play policy violation if `specialUse` was required. The manifest declares both types so no crash, but functional and compliance risk.

**Fix:** Persist `bg_service_type` alongside `bg_service_label` in SharedPreferences. Thread it through `GetAutoStartConfigResult`, `AutoStartConfig`, and `handleOsRestart`.

### MEDIUM: TypeScript StartConfig missing foregroundServiceType (confirmed, lower runtime impact)

The Rust `StartConfig` (`models.rs:23-31`) accepts `foregroundServiceType` via `#[serde(rename_all = "camelCase")]`. The Kotlin bridge and mobile.rs pass it through correctly. But:

- `guest-js/index.ts:5-8`: Interface only has `serviceLabel`
- `guest-js/dist-js/index.d.ts:2-5`: Generated types mirror the gap

**Runtime behavior:** The field DOES work if passed manually:
```typescript
startService({ serviceLabel: "test", foregroundServiceType: "specialUse" } as any);
```
The `invoke` call passes the full object through; serde deserializes it correctly. The gap is purely TypeScript ergonomics (no autocomplete, type error without `as any`).

**iOS note:** Not affected — iOS uses `BGAppRefreshTask` which has no foreground service type concept.

### LOW (new): mapServiceType silently swallows invalid values

`LifecycleService.kt:105-110`:
```kotlin
private fun mapServiceType(type: String): Int {
    return when (type) {
        "specialUse" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
        else -> ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
    }
}
```
Typos like `"special_use"`, `"SPECIALUSE"`, or `"location"` silently default to `DATA_SYNC` instead of failing fast. No logging, no error. Since both types are declared in the manifest, this won't crash, but it masks configuration errors.

**Recommendation:** Consider logging a warning via `Log.w()` for unrecognized service type strings, or returning an error to the caller.

### Deep Analysis Conclusion

The two medium issues are confirmed and traced end-to-end. Neither causes crashes (manifest declares both types). The primary risk is configuration integrity after OS restart — a user's `specialUse` choice is silently lost. The JS API gap is a discoverability issue with a trivial runtime workaround. One new low-severity finding added (silent type fallback).

## Positive Notes
- Generation counter pattern is elegant and well-tested
- Callback capture-at-spawn-time with generation guard is a solid pattern
- Main-queue serialization in Swift eliminates data races
- All 4 BGTask completion paths are nil-guarded and call cleanup()
- Android manifest correctly declares both dataSync and specialUse types
- Comprehensive test coverage (54 tests) including adversarial cases
- Clean module separation: runner, models, notifier, service_trait, mobile
- Guest-js properly builds CJS+ESM+types with rollup
