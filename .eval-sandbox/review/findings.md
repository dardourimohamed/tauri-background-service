# Code Review: tauri-plugin-background-service

## Files Reviewed
- [x] src/lib.rs
- [x] src/error.rs
- [x] src/models.rs
- [x] src/notifier.rs
- [x] src/service_trait.rs
- [x] src/runner.rs
- [x] src/mobile.rs
- [x] build.rs
- [x] Cargo.toml
- [x] tests/integration.rs
- [x] examples/basic_service.rs
- [x] guest-js/index.ts
- [x] guest-js/package.json
- [x] guest-js/tsconfig.json
- [x] permissions/default.toml
- [x] android/src/main/AndroidManifest.xml
- [x] android/src/main/kotlin/.../BackgroundServicePlugin.kt
- [x] android/src/main/kotlin/.../LifecycleService.kt
- [x] ios/Sources/.../BackgroundServicePlugin.swift
- [x] README.md

## Summary
**APPROVE with suggestions**

The codebase is well-structured, idiomatic Rust following Tauri v2 plugin conventions. All 37 tests pass, `cargo clippy` is clean, and the architecture cleanly separates concerns. The code reads like a production-quality Tauri plugin.

## Critical Issues (Must Fix)

### C1. iOS BGAppRefreshTask completes immediately — background execution is entirely non-functional

**File:** `BackgroundServicePlugin.swift:21-28`
**Severity:** HIGH — the iOS background execution feature does not work at all.

The registered `BGAppRefreshTask` handler calls `task.setTaskCompleted(success: true)` synchronously in the first line of the callback. This immediately tells iOS the task is finished, causing iOS to reclaim the background execution window (~30 seconds) before any Rust work can happen.

```swift
BGTaskScheduler.shared.register(forTaskWithIdentifier: taskId, using: .main) {
    [weak self] task in
    task.setTaskCompleted(success: true)  // ← fires immediately
    self?.scheduleNext()
}
```

**Three distinct problems identified:**

1. **Wasted execution window.** iOS grants a `BGAppRefreshTask` to let the app do background work. The handler completes it instantly, so the OS immediately marks the process as idle. The Tokio runtime's `service.run()` future continues executing only until iOS decides to suspend the process — which may be within seconds because the declared task is "complete."

2. **No expiration handler.** iOS requires `BGAppRefreshTask` handlers to set `task.expirationHandler` for graceful shutdown when the OS revokes the remaining time. Without it, iOS will kill the process if the task hasn't completed when the deadline passes. The Rust `CancellationToken` never gets triggered, meaning `service.run()` may be frozen mid-operation with no cooperative cancellation.

3. **No Swift→Rust signaling.** There is no communication channel between the Swift BGTask handler and the Rust Tokio runtime. When the task fires, Rust has no way to know a background window opened. When iOS is about to suspend, Rust has no way to know it should flush state and wind down. The `MobileLifecycle` bridge only has `start_keepalive` (schedule a task) and `stop_keepalive` (cancel the schedule) — there are no "background-task-started" or "background-task-expiring" events.

**Additional adversarial paths found:**

- **Silent scheduling failure:** `scheduleNext()` uses `try?` to swallow `BGTaskScheduler.submit()` errors. If iOS rejects the request (e.g., quota exceeded, identifier not in Info.plist), the error is silently lost and background processing stops permanently with no feedback to the Rust layer.
- **Bundle identifier fallback:** `Bundle.main.bundleIdentifier ?? "app"` uses `"app"` as fallback. If the bundle identifier is nil (misconfigured builds), all tasks use the identifier `"app.bg-refresh"`, which won't match `BGTaskSchedulerPermittedIdentifiers` in Info.plist, and iOS will never schedule the task.
- **Main queue handler:** The handler runs on `.main` queue. While the current handler is trivial, any future work done in the handler (e.g., signaling Rust) would block the UI thread.

**What a correct implementation would need:**

The fix requires bidirectional Swift↔Rust signaling:
1. When the BGAppRefreshTask fires, Swift should signal Rust that a background window is open.
2. Rust should perform its background work (or let the existing Tokio task run).
3. An `expirationHandler` should signal Rust to cancel gracefully via `CancellationToken`.
4. `setTaskCompleted` should only be called after Rust acknowledges completion or the expiration handler fires.
5. Only then should `scheduleNext()` be called.

This is a significant architectural gap — it requires adding a new cross-language signaling mechanism (e.g., a Tauri event or FFI callback) that doesn't exist in the current codebase.

## Suggestions (Should Consider)

### 1. Race condition window in `ServiceRunner::stop()` (runner.rs:145-154)
The `stop()` method takes the lock, removes the token, and cancels it. But `is_running()` (line 32) also takes the same lock. Between `stop()` dropping the token and the spawned task cleaning up, there's a brief window where `is_running()` returns `false` even though the service's `run()` future hasn't actually finished yet. This is a *semantic* concern — the service task is still executing when `is_running` says it's not. The generation counter correctly prevents state corruption for the stop→start race, but consumers relying on `is_running()` to know "has the service fully wound down?" will get a false negative.

**Severity:** Medium — depends on whether downstream code relies on `is_running()` for more than "can I call start()".

### 2. `tokio::spawn` without `Handle` parameter (runner.rs:91)
`tokio::spawn` is called directly inside `start_boxed`. If this code is ever invoked from outside a Tokio runtime context (e.g., during Tauri setup before the runtime is fully initialized), it will panic. Tauri v2 normally provides a runtime, but the `ServiceRunner` is `Send + Sync` with no runtime affinity, so a user could theoretically call `start()` from a non-Tokio thread.

**Severity:** Low — extremely unlikely in normal Tauri usage, but a footgun for advanced users.

### 3. `guest-js/` missing rollup.config.js — JS API is completely non-functional (CRITICAL JS BUILD ISSUE)

**Deep analysis — JS build tooling:**

The `package.json` specifies `"build": "rollup -c"` but no `rollup.config.js` or `rollup.config.mjs` exists. Verified: running `npx rollup -c` fails with `Cannot find module rollup.config.js`. The JS API **cannot be built at all**.

**Five distinct problems identified:**

1. **Missing rollup.config.js (blocker).** `rollup -c` requires a config file. Official Tauri v2 plugins (e.g., `tauri-plugin-app`) provide `rollup.config.mjs` that configures ESM output, CJS output, TypeScript compilation via `@rollup/plugin-typescript`, and externalizes `@tauri-apps/api`. This plugin has none of that. A minimal config needs: input `guest-js/index.ts`, two output targets (`esm` to `dist-js/index.js`, `cjs` to `dist-js/index.cjs`), typescript plugin, and `@tauri-apps/api` as external.

2. **No `dist-js/` output directory.** `package.json` exports map to `./dist-js/index.js`, `./dist-js/index.cjs`, `./dist-js/index.d.ts` — none exist. The package is completely non-functional as a published npm module. Anyone `npm install`-ing this plugin gets an empty package with no entry points.

3. **Missing `package-lock.json`.** No lockfile exists for reproducibility. Different `npm install` runs may resolve different dependency versions. The devDependencies (`rollup ^4.0.0`, `typescript ^5.0.0`, `@rollup/plugin-typescript ^11.0.0`) use caret ranges that float within majors.

4. **`.gitignore` gap: `dist-js/` not covered.** The root `.gitignore` has `dist/` but not `dist-js/`. If someone adds a rollup config and builds, the output would not be gitignored and could accidentally be committed.

5. **`tsconfig.json` include is overly broad.** `"include": ["./**/*.ts"]` matches all `.ts` files recursively. While currently there's only `index.ts`, this would pick up any future test files or utilities that shouldn't be in the published bundle. Should be `"include": ["index.ts"]` or `"include": ["*.ts"]`.

**Severity:** HIGH — the JS API cannot be built, tested, or consumed. This is the #1 blocker for anyone using the plugin from JavaScript/TypeScript.

### 4. Android `stopKeepalive` uses `startService` with STOP action (BackgroundServicePlugin.kt:45-46)
`stopKeepalive()` calls `activity.startService(...)` with `ACTION_STOP`. This works, but the method name `startService` to stop a service is semantically confusing. More importantly, on Android 12+ (API 31+), there are strict foreground service launch restrictions — calling `startService` from the background may throw `ForegroundServiceStartNotAllowedException`. Since `stopKeepalive` should always be called from the foreground (it's triggered by JS), this is fine in practice, but using `stopService()` or `Intent(action).also { it.action = ACTION_STOP }` → `LifecycleService.stopSelf()` would be more idiomatic.

**Severity:** Low — works in practice but non-idiomatic.

### 5. iOS `BGTaskScheduler` task completes immediately (BackgroundServicePlugin.swift:26-27)
The `BGAppRefreshTask` handler calls `task.setTaskCompleted(success: true)` immediately, then schedules the next task. This means the OS grants a background execution window and the plugin immediately surrenders it. The actual background work is done by the Tokio runtime in the Rust process, which isn't coordinated with the BGTask lifecycle. If iOS suspends the process after the task completes, the Tokio work gets frozen mid-execution.

The `earliestBeginDate` of 15 minutes means iOS won't even attempt to run again for 15 minutes after each brief window. This is architecturally limited by iOS, but the current design doesn't actually *use* the background execution window at all — it just keeps scheduling tasks that immediately complete.

**Severity:** Medium — the iOS background execution story doesn't actually work as intended. The BGTask completes before the Rust code can do meaningful work.

### 6. No `#[non_exhaustive]` on `ServiceError` or `PluginEvent` (error.rs, models.rs)
These are public enums that could grow in future versions. Without `#[non_exhaustive]`, downstream `match` statements will break when new variants are added. Since this is a v0.1.0 plugin, adding it now avoids a semver break later.

**Severity:** Low — easy to add before 1.0.

### 7. `ServiceFactory` type alias not publicly exported (lib.rs:44)
Users who want to store the factory or pass it around can't name the type. Consider re-exporting it if advanced usage is expected.

**Severity:** Nitpick.

### 8. `config` parameter silently dropped in `start_boxed` (runner.rs:80)
The `StartConfig` is received but immediately dropped with `let _config = config;`. The comment explains it's "used by the command handler for mobile keepalive labels," but this means a consumer calling `ServiceRunner::start()` directly (not through the Tauri command) passes a config that does nothing. The mobile keepalive is handled at the command level, not the runner level, which is correct architecture — but the runner accepting a parameter it ignores is a mild API smell.

**Severity:** Nitpick.

## Positive Notes
- Clean separation of concerns: trait, runner, notifier, models, mobile bridge
- Generation counter in `ServiceRunner` correctly handles the stop→start race condition
- `CancellationToken` for cooperative cancellation is the right primitive
- Comprehensive unit tests (31) and integration tests (6) covering lifecycle, edge cases, and serde roundtrips
- Proper Tauri v2 plugin conventions: `links` attribute, `tauri_plugin::Builder` in build.rs, auto-generated permissions
- iOS `ios_plugin_binding!` macro correctly placed at module level
- Mobile lifecycle correctly kept behind `#[cfg(mobile)]`
- Good documentation in README with platform-specific notes
- Clippy-clean with zero warnings
- Type-safe factory pattern with `ServiceFactory<R>` avoids runtime downcasting
