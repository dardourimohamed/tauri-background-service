# Code Review: tauri-plugin-background-service

## Files Reviewed
- [x] tauri-plugin-background-service/src/runner.rs
- [x] tauri-plugin-background-service/src/lib.rs
- [x] tauri-plugin-background-service/src/models.rs
- [x] tauri-plugin-background-service/src/mobile.rs
- [x] tauri-plugin-background-service/src/service_trait.rs
- [x] tauri-plugin-background-service/src/notifier.rs
- [x] tauri-plugin-background-service/src/error.rs
- [x] tauri-plugin-background-service/Cargo.toml
- [x] tauri-plugin-background-service/build.rs
- [x] tauri-plugin-background-service/permissions/default.toml
- [x] tauri-plugin-background-service/guest-js/index.ts
- [x] tauri-plugin-background-service/android/src/main/kotlin/app/tauri/backgroundservice/LifecycleService.kt
- [x] tauri-plugin-background-service/android/src/main/kotlin/app/tauri/backgroundservice/BackgroundServicePlugin.kt
- [x] tauri-plugin-background-service/ios/Sources/TauriPluginBackgroundService/BackgroundServicePlugin.swift
- [x] test-app/src-tauri/src/lib.rs
- [x] test-app/index.html

## Summary
COMMENT — one significant architectural concern requiring deep analysis. Overall code quality is high: generation counter pattern, callback capture, and lifecycle management are well-designed. All 57 tests pass.

## Test Results
- Unit tests: 45/45 PASS
- Integration tests: 12/12 PASS
- Doc tests: 0 passed, 1 ignored (expected — requires Tauri context)

## Critical Issues (Must Investigate)

### 1. start command calls native layer BEFORE AlreadyRunning check
**Severity:** High
**Location:** `lib.rs:120-135`
**Description:** The `start` Tauri command executes in this order:
1. `start_keepalive()` — starts Android foreground service / schedules iOS BGTask
2. `ios_set_on_complete_callback()` — overwrites the on_complete callback slot
3. `holder.start()` — checks AlreadyRunning, returns error if running
4. `ios_spawn_cancel_listener()` — skipped if step 3 errors

If the service is already running, step 3 returns `AlreadyRunning` but steps 1-2 already executed. This means:
- **Android:** A new START intent is sent to LifecycleService, which calls `startForeground()` again and updates the notification. The user gets an error but the foreground notification flickers.
- **iOS:** `scheduleNext()` is called (harmless — same identifier), and the on_complete callback is overwritten (orphaned — the running task captured the old one).
- **State desync:** The mobile native layer believes a new service was started, but the Rust runner rejected it.

**Confidence:** 85 (confirmed ordering issue with concrete failure paths)

### Deep Analysis: start command ordering (adversarial pass)

**Root cause:** `lib.rs:121-126` execute native side-effects before `holder.start()` performs the authoritative `AlreadyRunning` check under the token Mutex (`runner.rs:77-81`).

**Failure path 1 — Double start from JS:**
1. User calls `start()` → succeeds
2. User calls `start()` again → `start_keepalive()` succeeds (Android: `startForeground()` fires again; iOS: `scheduleNext()` fires), `ios_set_on_complete_callback()` writes orphaned callback into Mutex slot, then `holder.start()` returns `AlreadyRunning`
3. JS receives error, but native state was mutated
4. Android: notification flickers; iOS: orphaned callback (leaked closure holding `PluginHandle`)

**Failure path 2 — Concurrent stop+start race (most severe):**
1. Thread A (stop): `runner.stop()` acquires token Mutex, takes+cancel token, drops lock
2. Thread B (start): `start_keepalive()` starts new native keepalive
3. Thread A (stop): `stop_keepalive()` stops native keepalive — **kills B's keepalive**
4. Thread B (start): `holder.start()` acquires token Mutex, sees `None` (A cleared it), succeeds — service runs
5. **Result: Service running without native keepalive. Android may kill process; iOS may suspend app.**

This race exists because keepalive operations are not serialized with token operations. Tauri commands are independent async tasks — they CAN interleave.

**Failure path 3 — Auto-start same ordering issue:**
`lib.rs:199-201`: auto-start path calls `start_keepalive()` before `holder.start()`, and ignores all results (`let _`). Same desync potential, though less likely at setup time.

**Secondary concern — Fix introduces new risk:**
Moving `start_keepalive()` after `holder.start()` creates a window where the service runs without native keepalive. If `start_keepalive()` fails, the service MUST be rolled back (call `runner.stop()`) to avoid running unprotected.

**Recommended fix structure:**
```rust
async fn start<R: Runtime>(app: AppHandle<R>, config: StartConfig) -> Result<(), String> {
    ios_set_on_complete_callback(&app);   // Must precede start_boxed (take() at spawn)

    app.state::<Arc<ServiceRunnerHolder<R>>>()
        .start(app.clone(), config.clone())
        .map_err(|e| e.to_string())?;

    #[cfg(mobile)]
    if let Err(e) = app.state::<MobileLifecycle<R>>()
        .start_keepalive(&config.service_label, &config.foreground_service_type)
    {
        let holder = app.state::<Arc<ServiceRunnerHolder<R>>>();
        let _ = holder.runner.stop();  // Rollback
        return Err(e.to_string());
    }

    ios_spawn_cancel_listener(&app);
    Ok(())
}
```
Note: `StartConfig` derives `Clone` (`models.rs:21`). The `config` value is unused by `start_boxed()` (`runner.rs:93`: `let _config = config`) — it's only needed for keepalive labels. Future refactor could avoid the clone.

**Note on stop→start race:** The fix above addresses the `AlreadyRunning` ordering but does NOT fully resolve the concurrent stop+start race (failure path 2). A complete fix requires serializing stop and start commands (e.g., an async Mutex wrapping both the runner stop and the keepalive stop/start). This is an architectural change beyond a single-command fix.

## Suggestions (Should Consider)

### 1. Forward StartConfig to service init()
`runner.rs:91-93` suppresses the `StartConfig` with `let _config = config;`. Services currently cannot access the label or service type. Consider extending `ServiceContext` or passing config to `init()` so services can use these values.

### 2. Add logging to LifecycleService
`LifecycleService.kt` has almost no logging. Only one `Log.w` for unrecognized service types (line 115). Adding `Log.d` calls in `onStartCommand`, `handleOsRestart`, and `startForegroundTyped` would significantly help production debugging.

### 3. iOS safety timer is 25s — consider 28-29s
`BackgroundServicePlugin.swift:73` uses a 25-second safety timer. iOS typically gives ~30 seconds for BGAppRefreshTask. A task completing at 28s would be force-killed by the safety timer. Consider using 28-29 seconds or making it configurable.

## Positive Notes
- Generation counter pattern is correctly implemented for stop→start race conditions
- Token cleanup is generation-guarded (won't clear new service's token)
- on_complete callback captured at spawn time prevents stale callback issues
- init() failure path correctly fires callback with false and clears token
- Comprehensive unit tests for serde models (33 model tests)
- Integration tests cover real async lifecycle (start/stop/init-failure/on-complete/generation-guard)
- iOS Pending Invoke pattern is well-documented in comments
- `tauri::async_runtime::spawn()` correctly used for Android auto-start compatibility
- Serde `#[non_exhaustive]` on PluginEvent prevents API breakage
- Swift code uses `[weak self]` correctly to prevent retain cycles
- Android companion object `@Volatile` vars are appropriate for cross-thread visibility
