# Release Checklist

Pre-release verification for `tauri-plugin-background-service`. Complete all items before tagging a new version.

---

## Version Bump

- [ ] Update `version` in `tauri-plugin-background-service/Cargo.toml`
- [ ] Update `version` in `tauri-plugin-background-service/guest-js/package.json`
- [ ] Update `version` in `tauri-plugin-background-service/guest-js/package-lock.json` (`npm install` in `guest-js/`)
- [ ] Update crate version references in `README.md` and `docs/*.md` if major/minor bump
- [ ] Update `CHANGELOG.md` with the new version entry

---

## Automated Checks

These run in CI (`.github/workflows/ci.yml`) but should pass locally before pushing:

- [ ] `cargo test --features desktop-service` — all unit + integration tests pass
- [ ] `cargo clippy --features desktop-service -- -D warnings` — zero warnings
- [ ] `cargo fmt --check` — formatted
- [ ] `cargo doc --no-deps --features desktop-service` — zero warnings
- [ ] `cd guest-js && npm run build` — TypeScript compiles

---

## Manual Test Matrix

### Android

| # | Test Case | Steps | Expected |
|---|-----------|-------|----------|
| A1 | Start service | Call `startService({ serviceLabel: 'Test' })` | Persistent notification appears, `isServiceRunning()` returns `true`, `started` event fires |
| A2 | Stop service | Call `stopService()` | Notification removed, `isServiceRunning()` returns `false`, `stopped` event fires |
| A3 | FGS type validation (default) | Start with default config (`dataSync`) | Service starts with `FOREGROUND_SERVICE_TYPE_DATA_SYNC` |
| A4 | FGS type validation (custom) | Start with `foregroundServiceType: 'specialUse'` | Service starts with `FOREGROUND_SERVICE_TYPE_SPECIAL_USE`. Requires `FOREGROUND_SERVICE_SPECIAL_USE` permission and `PROPERTY_SPECIAL_USE_FGS_SUBTYPE` in manifest |
| A5 | FGS type validation (invalid) | Start with `foregroundServiceType: 'invalidType'` | Returns error, service does not start |
| A6 | Boot recovery | Enable auto-restart, reboot device | Service starts on boot (if type allows). If blocked (e.g. `dataSync` on API 35+), recovery notification appears |
| A7 | Timeout handling | Start with `dataSync` type, wait for 6-hour cumulative timeout (or mock `onTimeout`) | Service stops, state persists with `nativeState: 'timeout'`, `stopped` event with `reason: 'timeout'` |
| A8 | Notification customization | Configure `androidNotificationChannelName`, `androidNotificationSmallIcon`, `androidShowStopAction` | Notification uses custom channel name, icon, and shows stop action if enabled |
| A9 | Force quit survival | Start service, force-quit app from settings | Service stops. This is unsupported by design |
| A10 | `START_STICKY` restart | Start service, swipe app from recents | Service restarts via `START_STICKY` (best-effort, depends on OEM) |

### iOS

| # | Test Case | Steps | Expected |
|---|-----------|-------|----------|
| I1 | Start service (foreground) | Call `startService()` while app is foregrounded | `isServiceRunning()` returns `true`, `started` event fires, `run()` executes continuously |
| I2 | Stop service | Call `stopService()` | `isServiceRunning()` returns `false`, `stopped` event fires |
| I3 | BGTask scheduling | Start service, background app, wait for system to grant BGTask | BGAppRefreshTask or BGProcessingTask fires, `run()` executes in background window |
| I4 | Scheduling result | Call `getSchedulingStatus()` after start | Returns `{ refreshScheduled, processingScheduled }` booleans and any errors |
| I5 | Expiration handler | Background service running, wait for expiration | `setTaskCompleted` called exactly once, `stopped` event fires, next BGTask scheduled |
| I6 | Force quit | Start service, force-quit app from app switcher | Service stops. iOS will not relaunch the app. This is an iOS design limitation |
| I7 | Auto-start from BGTask | Enable auto-restart, force background, wait for BGTask launch | Rust auto-starts service from pending BGTask info, `desired_running` check passes |
| I8 | Safety timer | Mock expiration delay beyond safety threshold | Safety timer fires, calls `setTaskCompleted`, prevents double-completion |

### Desktop — In-Process Mode

| # | Test Case | Steps | Expected |
|---|-----------|-------|----------|
| D1 | Start service | Call `startService()` | `isServiceRunning()` returns `true`, `started` event fires |
| D2 | Stop service | Call `stopService()` | `isServiceRunning()` returns `false`, `stopped` event fires |
| D3 | Cancellation | Verify `run()` uses `tokio::select!` with `ctx.shutdown.cancelled()` | Service responds to `stopService()` promptly |
| D4 | App close | Close app window | Service stops (in-process mode). This is expected |

### Desktop — OS Service Mode (`desktop-service` feature)

| # | Test Case | Steps | Expected |
|---|-----------|-------|----------|
| D5 | Install service | Call `installService()` | Service unit/plist created. `getOsServiceStatus()` shows `installed: 'installed'` |
| D6 | Start OS service | Call `startOsService()` | Service starts. `getOsServiceStatus()` shows `installed: 'running'`, `ipcConnected: true` |
| D7 | Stop OS service | Call `stopOsService()` | Service stops. `getOsServiceStatus()` shows `installed: 'installed'`, `ipcConnected: false` |
| D8 | Restart OS service | Call `restartOsService()` | Service stops then starts. `ipcConnected` becomes `true` |
| D9 | Uninstall service | Call `uninstallService()` | Unit/plist removed. `getOsServiceStatus()` shows `installed: 'notInstalled'` |
| D10 | Autostart | Configure `desktopServiceAutostart: true`, install, reboot/logout+login | OS service starts automatically on login |
| D11 | IPC recovery | Stop OS service, call `startService()` with `desktopStartServiceIfMissing: true` | Plugin auto-starts OS service, waits for IPC, then sends start request |
| D12 | IPC timeout | Stop OS service, call `startService()` with `desktopStartServiceIfMissing: true` and short timeout | Returns IPC error after timeout with socket path |
| D13 | Windows unsupported | Run any OS service command on Windows | Returns `ServiceError::Platform("Windows OS-service mode is not yet supported")` |

### Cross-Platform API

| # | Test Case | Steps | Expected |
|---|-----------|-------|----------|
| X1 | `getPlatformCapabilities()` | Call on each platform | Returns correct `Platform`, `LifecycleMode`, and guarantee levels per platform table in README |
| X2 | `getServiceState()` (idle) | Call before starting service | Returns `{ state: 'idle', lastError: null }` |
| X3 | `getServiceState()` (running) | Call while service is running | Returns `{ state: 'running' }` with optional `desiredRunning`, `nativeState`, `platformMode` fields |
| X4 | `getServiceState()` (stopped) | Call after stopping service | Returns `{ state: 'stopped', lastError: null }` or with error if stopped due to failure |
| X5 | `enableAutoRestart()` | Call without starting service | `getDesiredServiceState()` returns `{ desiredRunning: true }`, service not started |
| X6 | `disableAutoRestart()` | Call after `enableAutoRestart()` | `getDesiredServiceState()` returns `{ desiredRunning: false }`, running service unaffected |
| X7 | `getDesiredServiceState()` | Call on platform without persistence | Returns `null` |
| X8 | `validateBackgroundServiceSetup()` (ok) | Call on correctly configured platform | Returns `{ ok: true, errors: [], warnings: [] }` |
| X9 | `validateBackgroundServiceSetup()` (errors) | Call on platform with missing permissions | Returns `{ ok: false, errors: [...], warnings: [...] }` with actionable `fix` suggestions |
| X10 | Typed errors — `alreadyRunning` | Start service twice | Second call rejects. `normalizeBackgroundServiceError(e).code` returns `'alreadyRunning'` |
| X11 | Typed errors — `notRunning` | Stop service when not running | Rejects. `normalizeBackgroundServiceError(e).code` returns `'notRunning'` |
| X12 | Typed errors — unknown | Cause an unexpected rejection | `normalizeBackgroundServiceError(e).code` returns `'unknown'` |
| X13 | `onPluginEvent()` | Start/stop service while listening | Receives `started`, `stopped`, and/or `error` events. Unsubscribe works cleanly |

---

## Documentation Review

- [ ] `README.md` — platform guarantee table accurate, no overpromises, version references correct
- [ ] `docs/api-reference.md` — all public APIs documented with TypeScript signatures and examples
- [ ] `docs/android.md` — FGS types, boot recovery, timeout, notification config documented
- [ ] `docs/ios.md` — "scheduled background execution" framing, limitations explicit, no "keepalive" language
- [ ] `docs/desktop.md` — in-process vs OS-service distinction clear, Windows unsupported, autostart documented
- [ ] `docs/getting-started.md` — prerequisites current for all platforms
- [ ] `docs/troubleshooting.md` — entries for all error conditions
- [ ] `docs/migration-guide.md` — version change section with backward-compatible notes
- [ ] Internal links valid — no broken relative paths between docs

---

## CI Verification

All CI jobs in `.github/workflows/ci.yml` should pass on the release branch:

- [ ] `check` — cargo check (default features)
- [ ] `check-android` — cargo check `--target aarch64-linux-android`
- [ ] `check-ios` — cargo check `--target aarch64-apple-ios`
- [ ] `check-desktop-service` — cargo check `--features desktop-service`
- [ ] `docs` — cargo doc `--no-deps --features desktop-service`
- [ ] `test` — Rust tests (default features)
- [ ] `clippy` — lint with `-D warnings`
- [ ] `fmt` — formatting check
- [ ] `ts` — TypeScript build
- [ ] `android-test` — Android Gradle unit tests
- [ ] `android-lint` — Android lint (pre-existing errors allowed via `continue-on-error`)
- [ ] `ios-build` — iOS Swift compilation

---

## Post-Release

- [ ] `git tag v{VERSION}` on the release commit
- [ ] `git push origin main --tags`
- [ ] Verify `crates.io` publish workflow triggered
- [ ] Verify `npm` publish workflow triggered
- [ ] Confirm new version appears on `crates.io` and `npmjs.com`
- [ ] Create GitHub Release with changelog summary
