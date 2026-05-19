# Implementation Plan: Lifecycle Hardening

## Checklist

- [x] Step 1: Fix Android compile blocker (StartConfig partial move)
- [x] Step 2: Fix clippy warnings (desktop-service feature)
- [x] Step 3: Define StopReason enum and extend PluginEvent
- [x] Step 4: Define LifecycleState and LifecycleStatus models
- [x] Step 5: Wire file-backed desired state into desktop paths
- [x] Step 6: Implement stop-reason-aware actor (StopWithReason command)
- [x] Step 7: Implement iOS expiration semantics (preserve desired state)
- [x] Step 8: Implement iOS pending BGTask persistence (UserDefaults)
- [x] Step 9: Implement iOS config shape alignment (serviceLabel migration)
- [x] Step 10: Implement Android native-to-Rust lifecycle bridge
- [x] Step 11: Implement Android foreground service type validation
- [x] Step 12: Implement Android notification permission explicit control
- [x] Step 13: Add Lifecycle API to guest-js
- [x] Step 14: Add compatibility wrappers to guest-js
- [x] Step 15: Upgrade validate_setup to structured reports
- [x] Step 16: Update demo app UI and lifecycle integration
- [x] Step 17: Fix demo build/deploy script portability
- [x] Step 18: Document native test harness execution
- [x] Step 19: Run full quality gates

---

## Step 1: Fix Android Compile Blocker (StartConfig Partial Move)

**Objective:** Make Android and iOS targets compile by fixing the partial move of `StartConfig` fields in `ServiceContext` construction.

**Implementation Guidance:**
- In `src/manager.rs` `handle_start()`, change `config.service_label` to `config.service_label.clone()` and `config.foreground_service_type` to `config.foreground_service_type.clone()` in the `#[cfg(mobile)]` `ServiceContext` construction block.
- Verify that `save_desired_running(state, true, Some(&config))` still compiles after the change — `config` must remain whole after `ServiceContext` construction.
- No changes to desktop code paths.

**Test Requirements:**
- `cargo check --target aarch64-linux-android` passes
- `cargo check --target x86_64-linux-android` passes
- `cargo test --all-targets` passes
- Existing `StartConfig` serialization tests still pass

**Integration:** No integration with previous steps (this is the first step).

**Demo:** After this step, Android targets compile. No visible demo change.

---

## Step 2: Fix Clippy Warnings (Desktop-Service Feature)

**Objective:** Make `cargo clippy --all-targets --features desktop-service -- -D warnings` pass.

**Implementation Guidance:**
- Fix `private_interfaces` warning in desktop test helper
- Fix boolean assert comparison warnings (use `assert!` instead of `assert_eq!(x, true)`)
- Fix needless borrow warnings
- Fix `while let` iterator warning
- Any other clippy warnings in the `desktop-service` feature path

**Test Requirements:**
- `cargo clippy --all-targets --features desktop-service -- -D warnings` exits 0
- `cargo test --all-targets --features desktop-service` passes

**Integration:** Independent of Step 1.

**Demo:** Clean clippy pass. No visible demo change.

---

## Step 3: Define StopReason Enum and Extend PluginEvent

**Objective:** Introduce the `StopReason` enum and extend `PluginEvent` to carry structured stop reasons instead of opaque strings.

**Implementation Guidance:**
- Add `StopReason` enum to `src/models.rs` with variants: `UserStop`, `AppStop`, `PlatformTimeout`, `PlatformExpiration`, `NativeNotificationStop`, `OsRestart`, `BootRecovery`, `TaskCompleted`, `Error`
- All `#[serde(rename_all = "camelCase")]`, `#[non_exhaustive]`
- Change `PluginEvent::Stopped { reason: String }` to `PluginEvent::Stopped { reason: StopReason }`
- Keep backward-compatible deserialization: if a string `"user"` is received, map to `StopReason::UserStop`
- Update `emit_event` calls in `manager.rs` to pass the correct `StopReason`

**Test Requirements:**
- Unit test: Each `StopReason` serializes to expected camelCase JSON
- Unit test: Deserialization of legacy string reasons maps to correct variants
- Unit test: `PluginEvent::Stopped { reason: StopReason::UserStop }` round-trips through serde

**Integration:** Extends existing models. All subsequent steps use `StopReason`.

**Demo:** Events now carry structured reasons. JS can distinguish stop types.

---

## Step 4: Define LifecycleState and LifecycleStatus Models

**Objective:** Add the unified `LifecycleState` and `LifecycleStatus` types that expose a complete view of service state.

**Implementation Guidance:**
- Add `LifecycleState` enum: `Idle`, `Starting`, `Running`, `Stopping`, `Stopped`, `Recovering`, `RecoveryPending`, `Expired`, `Blocked`, `Error`
- Add `LifecycleStatus` struct with: state, desired_running, recovery_enabled, recovery_pending, recovery_reason, last_start_config, last_platform_state, last_platform_error, last_error, platform, capabilities, issues
- Add a method on `ServiceState` (internal) to compute `LifecycleState` from current state fields
- Add `ValidationIssue` struct: severity, code, message, fix, platform

**Test Requirements:**
- Unit test: Each `LifecycleState` variant serializes to expected camelCase
- Unit test: `LifecycleStatus` computed from `ServiceState` returns correct state for: idle, running, stopped, recovery pending, expired, error

**Integration:** Models only. Steps 6-7 will populate these, Step 13 will expose them via API.

**Demo:** No visible change yet (models not wired to API).

---

## Step 5: Wire File-Backed Desired State into Desktop Paths

**Objective:** Connect the existing `FileDesiredStateBackend` to all desktop manager loop paths so desired state survives process restarts.

**Implementation Guidance:**
- In `init_with_service`, when creating the manager loop for desktop in-process mode, construct a `FileDesiredStateBackend` at `{app_data_dir}/background-service-state.json` and pass it to `manager_loop`
- In the desktop headless service mode, construct a similar backend at the configured state path
- Verify that `enable_auto_restart`, `disable_auto_restart`, `get_desired_service_state` now persist and retrieve state correctly
- Ensure the file backend handles concurrent access gracefully

**Test Requirements:**
- Unit test: Desktop desired-state backend wiring — `enable_auto_restart` persists, process restart (simulated with fresh manager) loads the same state
- Unit test: `get_desired_service_state` returns `None` without a backend (existing behavior preserved)
- Unit test: `disable_auto_restart` clears file-backed state

**Integration:** Builds on existing `FileDesiredStateBackend`. Uses models from Step 4 for extended state.

**Demo:** Desktop users can enable auto-restart, close the app, reopen, and see the service state persisted.

---

## Step 6: Implement Stop-Reason-Aware Actor (StopWithReason Command)

**Objective:** Refactor the actor's stop handling to accept explicit stop reasons and apply different policies (clear vs preserve desired state).

**Implementation Guidance:**
- Add `ManagerCommand::StopWithReason { reason: StopReason, reply }` to the command enum
- Create `handle_stop_with_reason()` that:
  - Always cancels the Rust task
  - Conditionally clears desired state based on reason:
    - `UserStop`, `AppStop`, `NativeNotificationStop` → clear desired state
    - `PlatformTimeout`, `PlatformExpiration` → preserve desired state if recovery enabled
    - `TaskCompleted` → configurable
    - `Error` → preserve desired state if recovery enabled
  - Emits `PluginEvent::Stopped { reason }` with the correct reason
- Modify existing `handle_stop` to call `handle_stop_with_reason(UserStop)`
- Update the cancel listener to distinguish resolved invoke (expiration) from timeout — send `StopWithReason::PlatformExpiration` or `StopWithReason::PlatformTimeout`
- Update task cleanup to send `StopWithReason::TaskCompleted` or `StopWithReason::Error`

**Test Requirements:**
- Unit test: `UserStop` clears desired state
- Unit test: `PlatformExpiration` preserves desired state when recovery enabled
- Unit test: `PlatformTimeout` preserves desired state when recovery enabled, clears when disabled
- Unit test: `NativeNotificationStop` clears desired state
- Unit test: `TaskCompleted` behavior follows configured recovery mode
- Unit test: `Error` preserves desired state when recovery enabled
- Unit test: Cancel listener resolves with `PlatformExpiration`, not generic `Stop`
- Unit test: Idempotent stop — second stop with any reason is a no-op

**Integration:** Uses `StopReason` from Step 3. Replaces existing stop logic.

**Demo:** Stop events now include structured reasons. JS consumers can distinguish them.

---

## Step 7: Implement iOS Expiration Semantics (Preserve Desired State)

**Objective:** Ensure iOS BGTask expiration does not clear desired state or cancel scheduled recovery.

**Implementation Guidance:**
- Modify `run_cancel_listener` to accept a stop reason parameter for the resolved case
- iOS expiration: `wait_for_cancel` resolves → Rust sends `StopWithReason(PlatformExpiration)`
- Explicit stop: `wait_for_cancel` rejected → Rust sends `StopWithReason(UserStop)` (via the explicit stop path)
- `handle_stop_with_reason(PlatformExpiration)` cancels current Rust task, completes BGTask, preserves desired state
- Swift `handleExpiration()` already schedules next tasks — ensure Rust stop does not call `stop_keepalive()` on expiration (which would cancel scheduled tasks)

**Test Requirements:**
- Rust test: `PlatformExpiration` does not clear desired state
- Rust test: `PlatformExpiration` does not call `stop_keepalive`
- Rust test: Explicit `UserStop` still clears desired state on iOS
- iOS Swift test (or mocked): Expiration does not clear `ios_desired_running` in UserDefaults

**Integration:** Uses `StopWithReason` from Step 6. Modifies cancel listener behavior.

**Demo:** On iOS, BGTask expiration preserves recovery. Service restarts on next OS execution window.

---

## Step 8: Implement iOS Pending BGTask Persistence (UserDefaults)

**Objective:** Persist pending BGTask info in UserDefaults so it survives timing gaps between BGTask handler and Rust setup.

**Implementation Guidance:**
- In Swift, when a BGTask launches the app, write to UserDefaults:
  - `ios_pending_task_kind` (string)
  - `ios_pending_task_identifier` (string)
  - `ios_pending_task_received_at` (number)
  - `ios_pending_task_consumed_at` (null initially)
- `get_pending_bg_task` reads from UserDefaults instead of in-memory property
- `clear_pending_bg_task` sets `consumed_at` timestamp
- If Rust setup fails to parse config, preserve pending task info for diagnostics

**Test Requirements:**
- Swift test: Pending task written to UserDefaults on BGTask launch
- Swift test: `get_pending_bg_task` reads from UserDefaults
- Swift test: `clear_pending_bg_task` sets consumed_at
- Swift test: Pending task survives app relaunch (simulated)

**Integration:** Modifies Swift `handleBackgroundTask` and Rust mobile commands.

**Demo:** iOS BGTask launch starts Rust service even if handler/setup timing varies.

---

## Step 9: Implement iOS Config Shape Alignment (serviceLabel Migration)

**Objective:** Ensure stored config uses the Rust `StartConfig` shape (`serviceLabel`, not `label`).

**Implementation Guidance:**
- In Swift `startKeepalive`, store config using the Rust field names: `serviceLabel`, `foregroundServiceType`
- In Rust iOS auto-start, when reading stored config, add migration: if `label` is present but `serviceLabel` is not, map `label` → `serviceLabel`
- Use `#[serde(alias = "label")]` on the `service_label` field in `StartConfig` for backward compatibility

**Test Requirements:**
- Rust test: Config with `serviceLabel` decodes correctly
- Rust test: Config with legacy `label` decodes to `serviceLabel`
- Rust test: Config with both fields uses `serviceLabel`
- Rust test: Unknown fields are ignored (serde default)

**Integration:** Modifies `StartConfig` serde attributes and Swift storage code.

**Demo:** Custom service label appears correctly after iOS BGTask recovery.

---

## Step 10: Implement Android Native-to-Rust Lifecycle Bridge

**Objective:** Make Android notification stop and timeout events reach the Rust actor.

**Implementation Guidance:**
- Add a `NativeLifecycleCallback` mechanism in the Android plugin:
  - `onNotificationStop()` → sends `NativeLifecycleEvent::AndroidNotificationStop` to Rust via Tauri command
  - `onTimeout(fgsType: String)` → sends `NativeLifecycleEvent::AndroidTimeout` to Rust
- In Rust, add a new `ManagerCommand::NativeLifecycleEvent { event }` handler that maps to `StopWithReason`:
  - `AndroidNotificationStop` → `StopWithReason(NativeNotificationStop)`
  - `AndroidTimeout` → `StopWithReason(PlatformTimeout)` (respects timeout policy)
- In `LifecycleService`, when handling `ACTION_STOP`, invoke the callback before stopping native service
- In `handleTimeout()`, invoke the callback before applying timeout policy

**Test Requirements:**
- Rust test: `NativeLifecycleEvent::AndroidNotificationStop` → clears desired state
- Rust test: `NativeLifecycleEvent::AndroidTimeout` with policy `scheduleRecovery` → preserves desired state
- Rust test: Native event while already stopped → no-op (idempotent)
- Android test: Notification stop invokes callback
- Android test: Timeout invokes callback

**Integration:** Uses `StopWithReason` from Step 6 and `NativeLifecycleEvent` command.

**Demo:** Android notification stop button cancels Rust work. Tick count stops. Status updates.

---

## Step 11: Implement Android Foreground Service Type Validation

**Objective:** Validate FGS types against plugin config and known manifest declarations before starting.

**Implementation Guidance:**
- In `BackgroundServicePlugin`, when `startKeepalive` is called, validate the requested type against `allowedFgsTypes` config
- If type is not in allowed list, return a structured error with the invalid type and valid options
- Catch native exceptions from `startForeground()` and map to structured errors
- Add validation logic to `validate_setup` that checks configured types against the manifest
- For boot-blocked types (API 35+), include a warning in validation

**Test Requirements:**
- Android test: Invalid type returns structured error
- Android test: Type not in allowed config returns structured error
- Android test: Boot-blocked type (API 35) returns warning in validation
- Android test: Missing FGS permission returns structured error

**Integration:** Extends existing `validate_setup` and `startKeepalive` validation.

**Demo:** Validation panel shows FGS type issues before service start.

---

## Step 12: Implement Android Notification Permission Explicit Control

**Objective:** Make notification permission request opt-in instead of automatic during plugin load.

**Implementation Guidance:**
- Add `androidRequestNotificationPermissionOnLoad` config option (default: `true` for backward compatibility)
- When `false`, skip the automatic `requestPermissions` call in `load()`
- Add `getNotificationPermissionStatus()` command that returns `granted`, `denied`, or `notDetermined`
- Add a helper API `requestNotificationPermission()` that developers can call at the right time
- Document recommended integration flow in API docs

**Test Requirements:**
- Android test: Config `false` does not request permission on load
- Android test: Config `true` requests permission on load (existing behavior)
- Android test: `getNotificationPermissionStatus` returns correct status
- Demo shows permission status in UI

**Integration:** Extends `BackgroundServicePlugin` config and adds new commands.

**Demo:** Demo shows notification permission status and explicit request button.

---

## Step 13: Add Lifecycle API to guest-js

**Objective:** Expose the new lifecycle-oriented API from the TypeScript package.

**Implementation Guidance:**
- Add `LifecycleState`, `LifecycleStatus`, `ValidationIssue` types
- Add `startService` (existing, unchanged), `stopService` with optional `{ reason }`, `configureRecovery(options)`, `getLifecycleStatus()`, `onLifecycleEvent(handler)`
- Add Rust command `get_lifecycle_status` that computes `LifecycleStatus` from `ServiceState`
- Add Rust command `configure_recovery` that wraps enable/disable auto-restart
- Update `build.rs` to register new commands
- Export all new types from `guest-js/index.ts`

**Test Requirements:**
- `npm run build` in `guest-js` passes
- Type-level test: `LifecycleStatus` type is complete and correct
- Rust test: `get_lifecycle_status` returns correct state for each lifecycle phase
- Rust test: `configure_recovery({ enabled: true })` persists desired state
- Rust test: `configure_recovery({ enabled: false })` clears desired state

**Integration:** Uses models from Step 4. Exposes actor behavior from Step 6.

**Demo:** Demo can call new lifecycle API and display full status.

---

## Step 14: Add Compatibility Wrappers to guest-js

**Objective:** Ensure existing API calls still work, mapping to the new lifecycle API internally.

**Implementation Guidance:**
- `enableAutoRestart(config?)` calls `configureRecovery({ enabled: true, config })`
- `disableAutoRestart()` calls `configureRecovery({ enabled: false })`
- `getDesiredServiceState()` maps from `getLifecycleStatus()`
- `getServiceState()` remains available but maps to the new internal state
- Add deprecation JSDoc annotations (not breaking changes)

**Test Requirements:**
- `npm run build` in `guest-js` passes
- Type-level test: Old and new API types coexist
- Rust test: Compatibility wrappers produce identical results to new API

**Integration:** Wraps Step 13 APIs.

**Demo:** Demo works with both old and new API calls.

---

## Step 15: Upgrade validate_setup to Structured Reports

**Objective:** Replace generic validation with platform-specific, actionable reports.

**Implementation Guidance:**
- Extend `SetupValidationReport` with structured `issues: Vec<ValidationIssue>` (from Step 4)
- Android validation checks: notification permission, FGS type, manifest alignment, boot limits, battery optimization
- iOS validation checks: BGTask identifiers, scheduler availability, background modes, desired state, pending task
- Desktop validation checks: service mode, backend path, install status, IPC socket
- Each issue has: severity, code, message, fix suggestion, platform
- Update Rust `validate_setup` command to return structured report

**Test Requirements:**
- Rust test: Each validation issue code produces correct severity and message
- Rust test: Empty validation (no issues) produces `valid: true`
- Android test (or mock): Missing permission produces `ANDROID_MISSING_NOTIFICATION_PERMISSION`
- iOS test (or mock): Missing identifier produces `IOS_BGTASK_IDENTIFIER_MISSING`

**Integration:** Uses `ValidationIssue` from Step 4. Extends existing `validate_setup`.

**Demo:** Validation panel shows structured issues with fix suggestions.

---

## Step 16: Update Demo App UI and Lifecycle Integration

**Objective:** Make the demo a first-class integration sample for the new lifecycle model.

**Implementation Guidance:**
- Use `getLifecycleStatus()` as the primary status API
- Display lifecycle state, desired running, recovery enabled, recovery pending, blocked reason, last platform event
- Add platform-specific panels for Android (notification permission, FGS type, boot recovery) and iOS (BGTask scheduling, expiration)
- Add controls for: start, stop, configure recovery, validate setup, refresh status, request notification permission
- Show validation issues in a dedicated panel
- Keep a small compatibility section showing legacy API calls still work
- Update Tauri config with representative plugin config for demo-safe defaults

**Test Requirements:**
- `npm install` and `npm run build` in `test-app` pass
- Manual verification: start/stop updates status correctly
- Manual verification: recovery pending shown distinctly from running
- Manual verification: validation panel shows actionable issues

**Integration:** Uses all APIs from Steps 13-15.

**Demo:** Full demo with lifecycle status, recovery state, validation, and platform panels.

---

## Step 17: Fix Demo Build/Deploy Script Portability

**Objective:** Remove machine-specific paths from build/deploy scripts.

**Implementation Guidance:**
- In `build-and-deploy.sh`, replace hardcoded `JAVA_HOME` with: check existing `JAVA_HOME`, if unset, print actionable error with install instructions
- Make the script detect Waydroid status and ADB connection with clear error messages
- Add a preflight check section that validates all prerequisites

**Test Requirements:**
- Script runs without hardcoded `JAVA_HOME` when env var is set externally
- Script prints clear error when `JAVA_HOME` is missing
- Script prints clear error when Waydroid or ADB is unavailable

**Integration:** Independent of other steps.

**Demo:** `build-and-deploy.sh` works on any machine with JDK 21+ and Waydroid.

---

## Step 18: Document Native Test Harness Execution

**Objective:** Make it easy for contributors to run native tests.

**Implementation Guidance:**
- Add a section to the README or contributing guide with:
  - Android: Command to run unit tests and instrumentation tests from a Tauri Android project
  - iOS: Command to run Swift tests from Xcode on macOS
  - CI: Recommended GitHub Actions workflow for native tests
- Add a release checklist item for native test gates

**Test Requirements:**
- Commands documented are verified to work on appropriate platforms
- Release checklist includes native test verification step

**Integration:** Documentation only. No code changes.

**Demo:** Contributors can run native tests from documented commands.

---

## Step 19: Run Full Quality Gates

**Objective:** Verify all global pass criteria are met.

**Implementation Guidance:**
Run all verification commands:
- `cargo test --all-targets`
- `cargo test --all-targets --features desktop-service`
- `cargo clippy --all-targets --features desktop-service -- -D warnings`
- `cargo check --target aarch64-linux-android`
- `cargo check --target x86_64-linux-android`
- `cd guest-js && npm install && npm run build`
- `cd test-app && npm install && npm run build`
- Desktop build in `test-app`
- Record results in release checklist

**Test Requirements:**
- All commands pass with zero errors
- No regressions in existing tests
- Demo builds and runs correctly

**Integration:** Final verification of all steps.

**Demo:** Full end-to-end demo pass.
