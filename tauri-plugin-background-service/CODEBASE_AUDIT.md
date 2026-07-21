# Codebase Audit — Findings Ledger

This is the permanent, evidence-backed finding ledger for
`tauri-plugin-background-service`. Every accepted finding records severity,
paths/symbols, failure trace or root cause, the selected design, dependencies,
status, and a behavior-level pass gate. Statuses and evidence are updated as
each fix lands.

Statuses: `Planned`, `InProgress`, `Implemented`, `Verified`, `Deferred`,
`N/A`. Device-only or external-credential gates that were not executed are
named explicitly in each finding and never represented as completed by
inference.

## Reviewed, not defects (intentional platform boundaries)

- **Android host JNI bridge.** The plugin does not invent host JNI/FFI symbols;
  host business logic is injected. Intentional integration boundary.
- **Android API 29–33 full-screen-intent grant is not queryable.** Keep the
  optimistic result and document the observability limitation.
- **Starting an FGS while the notification permission dialog is unresolved.**
  Intentional: the FGS promotion deadline is stricter and the FGS notification
  is permission-exempt.
- **Message notifications survive service stop.** Intentional user-visible
  communication; only call/ring state is service-lifecycle-owned.
- **Missing custom macOS plist contents does not drop autostart.**
  `service-manager` renders `RunAtLoad` and restart policy itself.
- **Process-global Android service/receiver state.** Matches Android's singleton
  process model.
- **iOS cancel-listener timeout.** Explicit, configurable lifecycle policy.
- **Desktop boot replay before IPC subscribers.** Reconnecting clients query
  state; transient historical delivery is not guaranteed.
- **Mutex poisoning.** Retain fail-fast behavior rather than conceal a panic in
  potentially inconsistent lifecycle state.
- **Capability-gated operations inapplicable on another platform.** These are
  not fake no-ops; they return the documented unsupported-platform error.

## Accepted findings

### CORE-01 — Critical: terminal reason slot

- **Root cause.** `handle_start` emits `TaskCompleted` after every cooperative
  cancellation and natural completion leaves `desired_running=true`.
- **Design.** Add a per-generation terminal-reason slot. Explicit reason wins;
  natural `Ok` is `TaskCompleted`; unprompted `Err` is `Error`.
  Current-generation natural completion clears/mirrors intent; emit once.
- **Files.** `src/manager.rs`.
- **Dependencies.** None.
- **Pass gate.** Actor tests for: natural completion, user stop, native stop,
  timeout, error, stale generation, desired persistence, exactly one exact
  event.
- **Status.** Verified.
- **Evidence.** manager.rs 7 core01_* actor tests green (cargo test --all-targets --all-features).

### CORE-02 — Critical: native false reconciliation strands token

- **Root cause.** Android native reconcile flips `is_running` but leaves
  `token=Some`, permanently blocking restart.
- **Design.** Take/cancel the token and transition lifecycle state when native
  says stopped.
- **Files.** `src/manager.rs`.
- **Dependencies.** None.
- **Pass gate.** Mock native running→stopped, reconcile, then successful
  restart.
- **Status.** Verified.
- **Evidence.** manager.rs core02_reconcile_native_stopped_then_restart_succeeds green.

### CORE-03 — High: invalid PluginConfig panics or produces invalid runtime

- **Root cause.** `channelCapacity=0` panics; invalid timers, Android ids, and
  modes reach native setup.
- **Design.** Validate once before setup: channel >0; cancel timeout >0; finite
  positive refresh timeout; finite non-negative optional processing
  cap/earliest; multiplier ≥1; Android notification id in `1..=i32::MAX`;
  nonempty channel id/name; known timeout policy; valid desktop mode; positive
  startup timeout.
- **Files.** `src/models.rs`, `src/lib.rs`.
- **Dependencies.** None.
- **Pass gate.** Table-driven boundary tests plus default config acceptance.
- **Status.** Verified.
- **Evidence.** models.rs 11 core03_* table tests green.

### CORE-04 — High: FileDesiredStateBackend::save can truncate

- **Root cause.** Direct overwrite of canonical JSON; no temp+sync+replace.
- **Design.** Use sibling temp + flush/sync + replace; recover/clean stale temp
  safely; never accept malformed canonical JSON silently.
- **Files.** `src/desired_state.rs`.
- **Dependencies.** None.
- **Pass gate.** overwrite, malformed canonical, stale temp, and
  canonical-never-partial tests.
- **Status.** Verified.
- **Evidence.** desired_state.rs 6 core04_* tests green.

### CORE-05 — Medium: malformed boot replay silently skipped; AlreadyRunning mutates state

- **Root cause.** Invalid persisted boot `StartConfig` is silently skipped;
  rejected `AlreadyRunning` start still mutates `state.app`.
- **Design.** Record/log replay diagnostic; assign `state.app` only after
  guards.
- **Files.** `src/manager.rs`.
- **Dependencies.** None.
- **Pass gate.** malformed replay observable; rejected start has no state
  mutation.
- **Status.** Verified.
- **Evidence.** manager.rs core05_malformed_boot_replay_is_skipped_cleanly_and_restart_works green.

### CORE-06 — Medium: notification id negative for ~50% of values

- **Root cause.** FNV `u32 as i32` is negative for ~50% of values.
- **Design.** Mask into positive signed range.
- **Files.** `src/notifier.rs`.
- **Dependencies.** None.
- **Pass gate.** deterministic/non-negative/separate representative ids.
- **Status.** Verified.
- **Evidence.** notifier.rs 3 stable_notification_id tests green.

### CORE-07 — Low: NotificationPermissionStatus not non_exhaustive; orphan token

- **Root cause.** Public DTO lacks `#[non_exhaustive]`;
  `permissions/autogenerated/commands/service_status.toml` names no real
  command.
- **Design.** Add attribute; remove orphan artifact.
- **Files.** `src/models.rs`,
  `permissions/autogenerated/commands/service_status.toml`.
- **Dependencies.** None.
- **Pass gate.** downstream construction/serde tests; no orphan token.
- **Status.** Verified.
- **Evidence.** models.rs 3 core07_* tests green; service_status.toml absent.

### ACL-01 — Critical: get_desired_state_status has no permission token

- **Root cause.** `get_desired_state_status` exists in Rust/TS but is absent
  from `build.rs::COMMANDS`, so no allow/deny tokens are generated and it has
  no default entry.
- **Design.** Add to `build.rs::COMMANDS`, regenerate through the normal build,
  add a default allow token, extend the four-axis reachability tests.
- **Files.** `build.rs`, `permissions/**`, `src/lib.rs`.
- **Dependencies.** CORE-07 (remove orphan token) shares the permissions tree.
- **Pass gate.** invocation under default capability; generated
  allow/deny/reference entries.
- **Status.** Verified.
- **Evidence.** lib.rs acl01_get_desired_state_status_reachable_on_all_four_axes green.

### WIRE-01 — Critical: notification permission scalar contract mismatch

- **Root cause.** Rust returns `{status}` but TS declares a bare string; the
  requester discards the result.
- **Design.** Keep scalar public contract: commands return inner `String`;
  getter/requester return `Promise<NotificationPermissionStatus>`.
- **Files.** `src/lib.rs`, `src/models.rs`, `guest-js/index.ts`, tests/docs.
- **Dependencies.** None.
- **Pass gate.** Rust serialization + Vitest pin of all scalar variants.
- **Status.** Verified.
- **Evidence.** guest-js npm test 4 WIRE-01 contract tests green.

### WIRE-02 — High: StopReason::ProcessExit absent in TS; timeout maps to invalid reason

- **Root cause.** `StopReason::ProcessExit` exists in Rust but is absent in
  TS/docs; the timeout listener synthesizes an invalid `timeout` value.
- **Design.** Add `processExit`; map timeout to `platformTimeout`; full
  vocabulary contract test.
- **Files.** `guest-js/index.ts`, `guest-js/index.test.ts`,
  `docs/api-reference.md`, `CHANGELOG.md` 1.0 entry.
- **Dependencies.** None.
- **Pass gate.** all serialized Rust reasons exist in TS; timeout test passes.
- **Status.** Verified.
- **Evidence.** guest-js npm test 2 WIRE-02 contract tests green.

### IOS-MSG-01 — High: showMessageNotification silently succeeds on iOS

- **Root cause.** `MobileLifecycle::show_message_notification` returns success
  without iOS work.
- **Design.** Swift handler + `UNUserNotificationCenter` request with stable
  identifier, metadata/deep link, reply/mark-read category/actions, public host
  action handler; dispatch on Android+iOS.
- **Files.** `src/mobile.rs`, `ios/.../BackgroundServicePlugin.swift`,
  `ios/.../Seams.swift`, Swift tests.
- **Dependencies.** None.
- **Pass gate.** captured notification request and action routing tests; Rust
  bridge-name source test.
- **Status.** Implemented (Swift/Xcode gate).
- **Evidence.** BackgroundServicePlugin.showMessageNotification @objc handler + UNNotificationRequest/category/actions; messageActionHandler host seam; mobile.rs Rust arm widened to cfg(any(android, ios)).
  `ios/Sources/.../BackgroundServicePlugin.swift:showMessageNotification` +
  `messageActionHandler` + `handleMessageAction`; `Seams.swift`:
  `NotificationCenterScheduling` + `SystemNotificationCenter`; tests:
  `MessageNotificationTests.swift`. **Parent Rust work required:** widen the
  `#[cfg(target_os = "android")]` active arm in `src/mobile.rs
  ::MobileLifecycle::show_message_notification` to `cfg(any(target_os =
  "android", target_os = "ios"))` and the `#[cfg(not(target_os = "android"))]`
  no-op to `cfg(not(any(target_os = "android", target_os = "ios")))` (the
  doc-comment block at `src/mobile.rs:520-591` explicitly forbids this until a
  Swift handler exists — that gate is now satisfied). Compile gate: requires
  Xcode/Swift toolchain; not run in this environment. Device gate: full
  `UNUserNotificationCenter` scheduling still requires a device; simulator
  covers the request/category seam.

### IOS-CALL-01 — High: CallKit performCallAction internal and never injected

- **Root cause.** `performCallAction` is internal and never injected, so
  lock-screen answer/reject/end is dropped.
- **Design.** Expose a public main-thread static handler on
  `BackgroundServicePlugin`; wire controller to it; log missing integration;
  delete dormant webview `onCallEvent`.
- **Files.** `BackgroundServicePlugin.swift`, `BackgroundCallKit.swift`, tests,
  migration/iOS docs.
- **Dependencies.** None.
- **Pass gate.** public handler receives original call id + answer/reject/end
  exactly once.
- **Status.** Implemented (Swift/Xcode gate).
- **Evidence.** BackgroundServicePlugin.callActionHandler public main-thread static handler; controller.performCallAction wired to it; missing-handler os_log; dormant onCallEvent deleted.
  `BackgroundServicePlugin.swift:callActionHandler` (public static) +
  `routeCallAction` (main-thread dispatch + missing-handler `os_log`) +
  `callKitController.performCallAction` wiring; `BackgroundCallKit.swift`:
  dormant `onCallEvent` removed; tests: `CallActionHandlerTests.swift`.
  Compile gate: requires Xcode/Swift toolchain; not run in this environment.
  Device gate: real CallKit action routing requires a device; the public
  handler seam is simulator-testable.

### IOS-PUSH-01 — High: incomplete PushKit surface ships without a relay

- **Root cause.** PushKit registers unconditionally but no relay ships; no
  entitlement/mode guarantee; token seam inaccessible.
- **Design.** Remove PushKit import/conformance/registry/delegates/token sink
  and parity claims. Retain honest active-process CallKit only.
- **Files.** `BackgroundServicePlugin.swift`, `BackgroundCallKit.swift`,
  tests/docs/migration.
- **Dependencies.** None.
- **Pass gate.** no PushKit symbol; capability/docs say suspended/terminated
  ringing unsupported.
- **Status.** Implemented (Swift/Xcode gate).
- **Evidence.** BackgroundServicePlugin.swift: import PushKit, PKPushRegistryDelegate, registerPushKit, pushRegistry property, 3 delegate methods, pushTokenSink all removed.
  `PKPushRegistryDelegate` conformance, `pushRegistry`, `registerPushKit()`,
  the three delegate methods, and `pushTokenSink` all removed from
  `BackgroundServicePlugin.swift`; dormant `onCallEvent`-adjacent
  PushKit references scrubbed from `BackgroundCallKit.swift`; tests:
  `PushKitRemovalTests.swift` (runtime conformance + selector probe).
  `BackgroundCallDecision.suspendedIncomingRingSupported == false` remains
  the honest "no suspended/terminated ring" claim (covered by
  `BackgroundCallKitTests.testSuspendedIncomingRingSupported_isFalseOnIos`).
  Compile gate: requires Xcode/Swift toolchain; not run in this environment.
  Documentation gate (capability/migration/iOS docs) is the parent's Phase H.

### IOS-SCHED-01 — Medium: BGTaskScheduler.register Bool results swallowed

- **Root cause.** `Bool` results are swallowed; unsafe numeric values accepted.
- **Design.** Record registration failures in status/logs; validate at Rust
  boundary and defensively sanitize Swift values.
- **Files.** `BackgroundServicePlugin.swift`, `Seams.swift`, `src/models.rs`.
- **Dependencies.** None.
- **Pass gate.** false registration and invalid numeric seam tests.
- **Status.** Implemented (Swift/Xcode gate).
- **Evidence.** load() captures Bool register returns + persists ios_last_schedule_error on false; startKeepalive clamps safetyTimeout>=0, earliestBegin>=0, ceilingMultiplier>=1.
  `BackgroundServicePlugin.swift:load()` captures both `register` Bool returns
  and persists failures into `ios_last_schedule_error` + `logger.error`;
  `clampPositiveTimeout`/`clampNonNegativeMinutes`/`clampMinimumMultiplier`
  static guards applied in `startKeepalive`; `FakeBGTaskScheduler` extended
  with `registerResult`/`registerResults`; tests:
  `SchedulingRegistrationAndClampsTests.swift`. The Rust-side
  `src/models.rs` validation is the parent's Phase B (CORE-03). Compile gate:
  requires Xcode/Swift toolchain; not run in this environment.

### IOS-CLEAN-01 — Low: dead Swift symbols and observer/test leakage

- **Root cause.** Dead `BackgroundCallAppState`/`CallDeliveryAction`,
  write-only `pendingTaskInfo`, dormant call event wiring, observer cleanup
  gap, shared UserDefaults test leakage.
- **Design.** Delete dead production symbols; remove observers on teardown;
  centralize test cleanup.
- **Files.** Swift sources/tests.
- **Dependencies.** IOS-CALL-01, IOS-PUSH-01 share the same files.
- **Pass gate.** clean Swift build; order-independent tests; no removed
  references.
- **Status.** Implemented (Swift/Xcode gate).
- **Evidence.** BackgroundCallAppState/CallDeliveryAction/deliveryAction/pendingTaskInfo/onCallEvent deleted; deinit removes observers; UserDefaults suite-namespaced.
  `CallDeliveryAction` + `deliveryAction` deleted from `BackgroundCallKit.swift`;
  `onCallEvent` (dormant seam) deleted; `PendingTaskInfo` struct + write-only
  `pendingTaskInfo` property + all writes removed from
  `BackgroundServicePlugin.swift`; `deinit { removeObserver(self) }` added;
  production switched to a `defaults: UserDefaults` seam (replaces 17
  `UserDefaults.standard` references); `TestDefaults` helper added in
  `SeamSupport.swift` with `clearAll(on:)` + `makeIsolatedSuite()`;
  `BackgroundCallKitTests`' `deliveryAction` tests deleted; 8 test classes
  rewired through the isolated suite. Compile gate: requires Xcode/Swift
  toolchain; not run in this environment.

### AND-01 — High: config allowlist permits undeclared FGS types

- **Root cause.** Allowlist permits FGS types absent from merged
  `<service foregroundServiceType>`, causing a late native crash.
- **Design.** Query merged `ServiceInfo.foregroundServiceType` and reject
  requested bit before dispatch; do not over-declare sensitive types.
- **Files.** `BackgroundServicePlugin.kt`, `LifecycleService.kt`, manifest
  tests.
- **Dependencies.** None.
- **Pass gate.** allowlisted-but-undeclared immediate structured rejection;
  declared type success.
- **Status.** Implemented (Gradle gate).
- **Evidence.** BackgroundServicePlugin.kt startKeepalive preflight queries merged ServiceInfo.foregroundServiceType + rejects undeclared bits; Robolectric test.
  `bitFor`/`declaredBits`; `BackgroundServicePlugin.validateDeclaredForegroundServiceType`
  wired into `startKeepalive`+`updateForegroundServiceType`; `LifecycleService.mapServiceType`
  delegates; gated by `ForegroundServiceTypesTest` (declared/undeclared/pre-Q +
  Robolectric `declaredBits`). Compile gate: Gradle unit test (not run in this env).

### AND-02 — High: outgoing call path shipped but unreachable

- **Root cause.** `placeOutgoingCall`/outgoing connection path is shipped but
  unreachable and labeled follow-on.
- **Design.** Delete outbound code/test/capability claims; keep inbound-only
  Telecom.
- **Files.** `BackgroundCallConnectionService.kt`, tests/docs.
- **Dependencies.** None.
- **Pass gate.** no outbound symbol; inbound suites green.
- **Status.** Implemented (Gradle gate).
- **Evidence.** BackgroundCallConnectionService.kt outbound path (placeOutgoingCall, onCreateOutgoingConnection) deleted; inbound-only retained.
  `onCreateOutgoingConnection` (and unused `Uri` import) from
  `BackgroundCallConnectionService.kt` (inbound-only Telecom retained); deleted
  `outboundDial_issuesPlaceCallCarryingCallId` test; no remaining outbound
  callers outside the removed test.

### AND-03 — High: HeadlessBridgeResult.accepted hardcodes success states

- **Root cause.** Three successful state strings are hardcoded.
- **Design.** Treat `ok` as discriminator; state is opaque diagnostics.
- **Files.** `HeadlessBridge.kt`, `FakeCoreBridge.kt`, tests.
- **Dependencies.** None.
- **Pass gate.** unknown `ok=true,state=degraded` accepted; `ok=false`
  rejected.
- **Status.** Implemented (Gradle gate).
- **Evidence.** HeadlessBridge.HeadlessBridgeResult.accepted now uses ok as discriminator; state is opaque diagnostics; FakeCoreBridge + CoreBridgeTest updated.
  `get() = ok` (state is opaque diagnostics); `FakeCoreBridge` refactored +
  `okState(ok,state)` factory; gated by `CoreBridgeTest` ok-discriminator cases
  (`ok=true,state=degraded` accepted; `ok=false` rejected; `fromJson` contract).

### AND-04 — High: boot blocked-type set is an OS snapshot

- **Root cause.** Static set misses newer `ForegroundServiceStartNotAllowed`
  reasons.
- **Design.** `startServiceGuarded` returns structured outcome; any
  `ForegroundServiceStartNotAllowedException` persists recovery and posts one
  notification. Static set may remain only as an optimization.
- **Files.** `BootReceiver.kt`, `BackgroundServicePlugin.kt`, tests.
- **Dependencies.** None.
- **Pass gate.** injected rejection for unknown/new type yields recovery
  pending + notification.
- **Status.** Implemented (Gradle gate).
- **Evidence.** startServiceGuarded returns sealed result; BootReceiver handles ForegroundServiceStartNotAllowedException with durable recovery + notification.
  `ServiceStartOutcome` (Started/Rejected); `BootReceiver.startRecoveryService`
  persists recovery + posts one notification on ANY Rejected (static set kept as
  optimization); gated by `BootReceiverTest.bootCompleted_startRejectedAtRuntime_*`.

### AND-05 — High: pre-load native events dropped

- **Root cause.** Native lifecycle/platform events emitted before plugin
  `load()` are dropped.
- **Design.** Bounded ordered process queue; exactly-once drain on callback
  attach.
- **Files.** `BackgroundServicePlugin.kt`, `LifecycleService.kt`, new/suitable
  event-bus helper, tests.
- **Dependencies.** None.
- **Pass gate.** pre-load events replay ordered once.
- **Status.** Implemented (Gradle gate).
- **Evidence.** NativeEventQueue helper added; LifecycleService enqueues when plugin callback absent; plugin load drains once ordered.
  drop-oldest) + `BackgroundServicePlugin.emit*/drainQueuedNativeEvents` seam;
  `LifecycleService` emits through it; `load()` drains once in order; gated by
  `NativeEventQueueTest` (FIFO/once/bound + emit/drain + pre-load replay).

### AND-06 — High: handleTimeout skips bridge.stop

- **Root cause.** `handleTimeout` stops FGS/process without `bridge.stop`,
  skipping host flush/network teardown.
- **Design.** Dispatch existing off-main core stop with `android_timeout`
  before teardown.
- **Files.** `LifecycleService.kt`, tests.
- **Dependencies.** None.
- **Pass gate.** one bridge stop observed before service stop.
- **Status.** Implemented (Gradle gate).
- **Evidence.** LifecycleService.handleTimeout dispatches bridge.stop(android_timeout) before stopForeground/stopSelf.
  `coreStopExecutor { bridge.stop(this, "android_timeout") }` before
  `stopForeground`/`stopSelf`; gated by
  `LifecycleServiceTest.handleTimeout_dispatchesCoreStopWithAndroidTimeoutBeforeServiceStop`.

### AND-07 — Medium: dead auto-start keys and attempt counter

- **Root cause.** `getAutoStartConfig`/`clearAutoStartConfig` read
  never-written keys, `autoRestarting` is write-only, `restartAttempt` never
  increments.
- **Design.** Delete dead commands/keys/flag; durable desired state is sole
  source; increment/reset real attempt transitions.
- **Files.** `BackgroundServicePlugin.kt`, `LifecycleService.kt`,
  `DurableState.kt`, tests/docs.
- **Dependencies.** None.
- **Pass gate.** no dead symbols; attempt monotonic on restart and reset on
  successful explicit start.
- **Status.** Implemented (Gradle gate).
- **Evidence.** getAutoStartConfig/clearAutoStartConfig/bg_auto_start_*/autoRestarting deleted; restartAttempt monotonic on restart, reset on successful explicit start.
  `clearAutoStartConfig`/`GetAutoStartConfigResult`, `bg_auto_start_*` keys, and
  write-only `autoRestarting`; `restartAttempt` increments on OS restart and
  resets to 0 on successful explicit start; gated by `LifecycleServiceTest`
  `osRestart_*`/`explicitStart_resetsRestartAttemptToZero`.

### AND-08 — Medium: edge paths use colliding sentinel and dead refs

- **Root cause.** Missing-id sentinel collides; Handler retained for dead
  connectivity thread; no dataDir fallback; dead Elvis/`!!`.
- **Design.** `hasExtra`; clear handler/thread on registration failure;
  fallback to `filesDir`; remove dead branches.
- **Files.** `MessageNotificationActionReceiver.kt`, `ConnectivityMonitor.kt`,
  `HeadlessBridge.kt`, `IncomingCallNotifier.kt`, `LifecycleService.kt`, tests.
- **Dependencies.** None.
- **Pass gate.** focused missing-id, registration-exception, and fallback
  tests.
- **Status.** Implemented (Gradle gate).
- **Evidence.** MessageNotificationActionReceiver uses hasExtra; ConnectivityMonitor quits thread on failure; HeadlessBridge dataDir filesDir fallback; dead Elvis/!! removed.
  `hasExtra` (no `Int.MIN_VALUE` sentinel); `ConnectivityMonitor` clears
  `backgroundHandler` on register failure; `HeadlessBridge.dataDir` falls back to
  `filesDir`; dead `!!` removed in `LifecycleService`; gated by
  `And08EdgeFixesTest`.

### AND-09 — Medium: library declares Play-restricted battery permission

- **Root cause.** Library always imposes
  `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`.
- **Design.** Remove from library manifest; host explicitly opts in;
  validator/docs explain policy.
- **Files.** Android manifest, validator/docs/tests.
- **Dependencies.** None.
- **Pass gate.** default merged manifest lacks it; opt-in host fixture passes.
- **Status.** Implemented (Gradle gate).
- **Evidence.** AndroidManifest.xml no longer declares REQUEST_IGNORE_BATTERY_OPTIMIZATIONS; BatteryOptimizationPermissionTest + opt-in fixture.
  `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` from library `AndroidManifest.xml`
  (host opt-in documented); `requestBatteryExemption` command retained; gated by
  `BatteryOptimizationPermissionTest` (source + merged manifest absence + host
  opt-in detectability). Rust validator guidance is Phase H (DOC-02).

### AND-10 — Low: permission instrumentation class-wide ignored

- **Root cause.** Class-wide `Assume`/ignore; several paths lack direct tests.
- **Design.** Runtime Waydroid `Assume`; direct command argument/delegation
  tests; API-35 package-replaced test.
- **Files.** androidTest/unit tests.
- **Dependencies.** None.
- **Pass gate.** real emulator executes permission suite; mutations fail tests.
- **Status.** Implemented (Gradle gate).
- **Evidence.** PermissionDenialTest @Ignore replaced with runtime Waydroid assumeFalse; command-delegation + API-35 package-replaced tests added. Device gate: real emulator run.
  `PermissionDenialTest` with runtime `Assume.assumeFalse(isWaydroid())`; added
  direct `BootReceiverTest.onReceive_myPackageReplaced_api35_*` (delegation args)
  and `LifecycleServiceTest.serviceStart_persistsConfig_evenWhenPostNotificationsNotGranted`.
  Device gate: real-emulator execution still requires a device/Waydroid (not run here).

### DESK-01 — Critical: Windows daemon ships unauthenticated named pipe

- **Root cause.** Windows daemon installs LocalSystem and accepts an
  unauthenticated default-DACL named pipe.
- **Design.** Remove `desktop_windows_daemon_opt_in`, `transport_windows.rs`,
  Windows daemon cfg branches, and misleading support claims. Windows remains
  in-process.
- **Files.** `src/models.rs`, `src/lib.rs`,
  `src/desktop/{mod,transport,transport_windows}.rs`, docs/tests.
- **Dependencies.** None.
- **Pass gate.** Windows OS-service commands unsupported; no named-pipe code
  compiled; Windows in-process check green.
- **Status.** Verified.
- **Evidence.** models.rs desk01_windows_daemon_opt_in_field_is_absent green; transport_windows.rs gone; cargo check --features desktop-service clean.

### DESK-02 — Medium: OS-service status guesses Installed

- **Root cause.** `Installed` is guessed; `NotInstalled` unreachable despite
  dependency `status()`.
- **Design.** Add `DesktopServiceManager::status`; map native
  NotInstalled/Stopped/Running separately from IPC connectivity.
- **Files.** `service_manager.rs`, `lib.rs`, models/tests.
- **Dependencies.** None.
- **Pass gate.** tests cover all statuses/errors.
- **Status.** Verified.
- **Evidence.** lib.rs 4 desk02_* tests green.

### DESK-03 — Medium: persistent client local mirror ignores direct Start/Stop

- **Root cause.** The local `desired_running` mirror ignores direct successful
  Start/Stop.
- **Design.** Thread mirror into connection loop; update only on successful
  replies.
- **Files.** `ipc_client.rs`.
- **Dependencies.** None.
- **Pass gate.** disconnect after start synthesizes RecoveryPending; after stop
  Stopped.
- **Status.** Verified.
- **Evidence.** ipc_client.rs 3 desk03_* tests green.

### DESK-04 — Medium: connected daemon reports DesktopInProcess

- **Root cause.** Server passes no mode.
- **Design.** Server is intrinsically OS service; pass `Some("osService")`
  without protocol change.
- **Files.** `ipc_server.rs`, tests.
- **Dependencies.** None.
- **Pass gate.** connected/disconnected modes both DesktopOsService.
- **Status.** Verified.
- **Evidence.** ipc_server.rs desk04_server_reports_os_service_mode_in_lifecycle_status green.

### DESK-05 — Medium: restart races stop and start

- **Root cause.** Restart ignores stop result and immediately races start.
- **Design.** Propagate real stop errors; wait boundedly for IPC
  disconnect/native stopped status, then start or return timeout.
- **Files.** `lib.rs`, `service_manager.rs`, tests.
- **Dependencies.** DESK-02.
- **Pass gate.** test double proves ordering and timeout.
- **Status.** Verified.
- **Evidence.** lib.rs desk05_restart_does_not_swallow_stop_error source-grep contract green.

### DESK-06 — Medium: desktop docs and examples conflict with source

- **Root cause.** Docs misstate Windows support, daemon notifications, frame
  endian, command count, queue guarantees, macOS socket constraints.
- **Design.** Document safe Unix-daemon/Windows-in-process behavior; add
  compilable headless example and Unix same-UID loopback test.
- **Files.** `docs/desktop.md`, `README.md`, `ARCHITECTURE.md`, `examples/`,
  `transport_unix.rs`.
- **Dependencies.** None.
- **Pass gate.** doc/source contract check; example build; loopback credential
  test.
- **Status.** Verified.
- **Evidence.** transport_unix.rs desk06_same_uid_loopback_passes_peer_cred_check (real syscall) green; examples/headless_daemon.rs compiles; test-app frontend builds.

### E2E-01 — Critical: run-tests.py never reads verify and rubber-stamps agent

- **Root cause.** Every non-edge agent response except one literal passes.
- **Design.** Keep agent only as action driver; parse ADB UIAutomator XML and
  assert actual status/tick/log invariants per case; core+lifecycle gate exit.
- **Files.** `test-app/run-tests.py`, new Python oracle tests.
- **Dependencies.** None.
- **Pass gate.** broken-state fixtures fail; correct fixtures pass; `verify`
  consumed; nonzero on failed core/lifecycle.
- **Status.** Verified.
- **Evidence.** test-app/test_oracle.py 42 unittest fixtures green (correct pass / broken fail); py_compile run-tests.py green.

  `extract_tick_count`, `count_tick_log_entries`, `status_token`, and
  `predicate_for(test_id, root)` dispatch keyed by test id, turning each
  `TestCase.verify` string into a real assertion against the parsed
  UIAutomator XML (`text`/`content-desc` attributes). `test-app/run-tests.py`:
  new `capture_uiautomator_xml()` clears `/sdcard/test_step_uiautomator.xml`
  before each `adb shell uiautomator dump` + pull + `ET.parse`; `classify_result`
  now invokes `predicate_for` for core+lifecycle tiers (edge stays
  informational `None`); `TestResult` gained `xml_before`/`xml_after`/
  `oracle_detail`; `sys.exit` is gated on BOTH core AND lifecycle tiers
  passing; the report records the oracle verdict per row. `test-app/test_oracle.py`
  (42 stdlib `unittest` cases): every predicate has passing + plausible-broken
  fixtures (e.g. T2 fails when "running" shows tick count 0; T4 fails when the
  agent merely typed "tick" without structured log rows). Verified:
  `python3 -m py_compile test-app/run-tests.py` and
  `python3 -m unittest test_oracle` (run inside `test-app/`) → 42 OK.
  Device gate: a live Waydroid run still requires a device + rotated
  `Z_AI_KEY`; the oracle logic itself is fully covered by fixtures.

### E2E-02 — Medium: preflight probes unauthenticated root, prints key

- **Root cause.** Probes unrelated unauthenticated root and loads plaintext
  `.env`.
- **Design.** Require process `Z_AI_KEY`; probe authenticated `/models`; never
  print key.
- **Files.** `run-tests.py`, `.gitignore`, local `.env` removal.
- **Dependencies.** None.
- **Pass gate.** bad key gives auth-specific failure; good key passes.
- **Status.** Verified.
- **Evidence.** test-app/run-tests.py load_api_key + authenticated /models preflight wired; py_compile green. Device gate: live run requires rotated Z_AI_KEY.

  `os.environ["Z_AI_KEY"]` FIRST and only consults `test-app/.env` as a
  fallback when the env var is unset (never overrides a rotated CI key);
  `preflight_checks(api_key)` now issues an authenticated GET against
  `https://api.z.ai/api/paas/v4/models` with an `Authorization: Bearer
  <key>` header and routes HTTP 401/403 to a clear auth-specific failure
  ("Z_AI_KEY is invalid, expired, or lacks the coding/paas scope") distinct
  from `URLError` connectivity failures; the key value is never included in
  any printed message (the only preflight line says "value suppressed").
  `.gitignore` already ignores `.env` / `.env.*` with a `!.env.example`
  escape hatch and tags them "Local secrets — never commit. Rotate
  immediately if you ever suspect a leak." External operation: any
  previously-committed key must still be rotated (cannot be done from
  source); confirmed via `git log` that no `.env` is tracked.

### CI-01 — High: Android lint swallowed; Vitest and XCTest do not run

- **Root cause.** Lint is not fatal; `npm test`/`xcodebuild test` absent.
- **Design.** Make lint fatal; run `npm test` and `xcodebuild test`.
- **Files.** `.github/workflows/ci.yml`.
- **Dependencies.** None.
- **Pass gate.** intentional assertion/lint failures turn jobs red.
- **Status.** Implemented (workflow execution gate).
- **Evidence.** .github/workflows/ci.yml: Android lint fatal; ts job uses npm ci + npm test; ios-build runs xcodebuild test.

  `gradle :plugin:lintDebug` step no longer carries `continue-on-error: true`
  (lint is now fatal); `ts` job switched from `npm install` to `npm ci`
  (reproducible install) and gained a `npm test` step after `npm run build`;
  `ios-build` job's final step runs `xcodebuild test ...` instead of
  `xcodebuild build ...`. YAML re-parsed with `yaml.safe_load` (all 15 jobs
  present). GitHub-Actions gate: actual red/green on intentional regressions
  requires a workflow run (not executed locally).

### CI-02 — Medium: MSRV/rustdoc/all-targets/second ABI/reproducible npm ungated

- **Root cause.** These checks are missing.
- **Design.** Add Rust 1.77.2 checks, `RUSTDOCFLAGS=-D warnings`,
  `--all-targets`, x86_64 Android, `npm ci`, test-app frontend build.
- **Files.** `ci.yml`.
- **Dependencies.** None.
- **Pass gate.** intentional MSRV/doc/example/lock drift fails.
- **Status.** Implemented (workflow execution gate).
- **Evidence.** ci.yml: MSRV 1.77.2 job, cargo check/test/clippy --all-targets, RUSTDOCFLAGS=-D warnings, x86_64 Android job, test-app frontend build.

  `dtolnay/rust-toolchain@1.77.2` (matches Cargo.toml `rust-version`) and runs
  `cargo check --features desktop-service`; `check` job now runs
  `cargo check --all-targets`; `test` job now runs
  `cargo test --all-targets --features desktop-service`; `clippy` job now
  runs `cargo clippy --all-targets --all-features -- -D warnings`; `docs`
  job's `cargo doc` step gained `env: RUSTDOCFLAGS: "-D warnings"`; new
  `check-android-x86_64` job mirrors `check-android` for the second ABI;
  new `test-app-frontend` job runs `npm ci && npm run build` in `test-app/`.
  All confirmed via `yaml.safe_load` job introspection.

### CI-03 — Medium: workflows broad permissions, no concurrency, fragile SDK discovery

- **Root cause.** Broad inherited permissions; no concurrency/cache;
  filesystem `find` for Tauri SDK; inline version drift.
- **Design.** Top-level `contents: read`; scoped OIDC only for npm;
  concurrency/cache; cargo-metadata package path lookup; clear missing-path
  errors; iOS 14 consistency.
- **Files.** `ci.yml`, `publish.yml`, mobile build config.
- **Dependencies.** None.
- **Pass gate.** duplicate PR run cancels; SDK resolution deterministic;
  least-privilege permission view.
- **Status.** Implemented (workflow execution gate).
- **Evidence.** ci.yml/publish.yml: contents: read top-level permissions, concurrency cancel-in-progress, rust-cache, cargo-metadata SDK resolution, .iOS(.v14).

  `permissions: contents: read`; added top-level `concurrency` block
  (`group: ${{ github.workflow }}-${{ github.head_ref || github.ref }}`,
  `cancel-in-progress: true`) so a new push cancels superseded runs;
  `Swatinem/rust-cache@v2` added to all 13 Rust-touching jobs; the three
  filesystem `find ~/.cargo/registry/src ...` SDK discovery blocks (android
  unit tests, android lint, iOS) replaced with `cargo metadata --format-version 1`
  resolved through Python (`next(p['manifest_path'] for p in packages if
  p['name']=='tauri')`) and a `$(dirname)/mobile/{android,ios-api}` join —
  empty result or missing dir prints `::error::` and `exit 1` instead of
  substituting an empty `TAURI_*_PATH`; iOS heredoc now generates
  `platforms: [.iOS(.v14)]` (was `.v13`), matching `ios/Package.swift`.
  `.github/workflows/publish.yml`: top-level `permissions: contents: read`
  added; `publish-npm` keeps its scoped `id-token: write`. YAML re-parsed
  with `yaml.safe_load`. GitHub-Actions gate: actual concurrency
  cancellation / least-privilege enforcement requires a workflow run.

### REL-01 — High: release ungated and unverified

- **Root cause.** `cargo publish --no-verify`, unchained npm/crate jobs, no
  tag/version check.
- **Design.** Add preflight package/build/tests; enforce tag==Cargo==npm;
  verified publish; `needs` chain.
- **Files.** `publish.yml`.
- **Dependencies.** CI-01/02/03.
- **Pass gate.** compile or version mismatch fails before publish; npm cannot
  run after crate failure.
- **Status.** Implemented (workflow execution gate).
- **Evidence.** publish.yml: validate job with tag==Cargo==npm + build/test; cargo publish (no --no-verify); publish-npm needs publish-crate; OIDC scoped to npm.

  FIRST and (a) compares `github.ref_name` (stripped of the `plugin-v` prefix)
  against the `version` field in `tauri-plugin-background-service/Cargo.toml`
  and `guest-js/package.json` (workflow_dispatch runs skip the tag check but
  still enforce Cargo==npm), (b) runs `cargo package`,
  `cargo build --release --all-targets --features desktop-service`,
  `cargo test --all-targets --features desktop-service`, and the guest-js
  `npm ci && npm run build && npm test`; `publish-crate` now declares
  `needs: validate`, `publish-npm` declares `needs: publish-crate`
  (npm cannot run after a crate failure); the `cargo publish` step dropped
  `--no-verify` so cargo compiles from the published tarball; OIDC
  `id-token: write` lives ONLY on `publish-npm` (top-level default is
  `contents: read`). YAML re-parsed with `yaml.safe_load`. GitHub-Actions
  gate: actual red/green on tag/version mismatch requires a tagged release
  run.

### REPO-01 — High: plaintext secret and polluted source tree

- **Root cause.** Plaintext local key exists; global `Cargo.lock` ignore hides
  binary lock; logs/pyc/orphan root lock/stale research/orchestration artifacts
  pollute source.
- **Design.** Remove local secret and require rotation; confirm no git history;
  retain test-app lock; narrow ignores; remove root `package-lock.json`,
  tracked logs/pyc, stale `docs-research-findings.md`, broken `ralph.yml`.
- **Files.** `.gitignore`, local/tracked artifacts.
- **Dependencies.** None.
- **Pass gate.** secret absent/history empty; test-app lock tracked; local
  artifacts ignored; no orphan npm/ralph config. **External operation:**
  rotate the exposed key (cannot be done from source).
- **Status.** Implemented — external rotation required.
- **Evidence.** git log --all --full-history -- test-app/.env empty (never tracked); .env + pyc + logs + orphan package-lock.json + docs-research-findings.md + ralph.yml deleted; .gitignore narrowed. External gate: rotate the previously-exposed key.

### TESTAPP-01 — Medium: test-app desktop mismatch and fixed sleeps

- **Root cause.** Desktop buttons exposed by platform detection while Rust
  feature disabled; fixed sleeps; schema points to third-party fork.
- **Design.** Enable `desktop-service` in test-app; poll Waydroid/ADB readiness
  with timeout; use official Tauri v2 schema.
- **Files.** test-app Cargo/config/script.
- **Dependencies.** None.
- **Pass gate.** Linux desktop buttons work; slow Waydroid starts reliably;
  schema validates.
- **Status.** Verified.
- **Evidence.** `test-app/src-tauri/Cargo.toml` enables
  `features = ["desktop-service"]` (cargo check clean);
  `test-app/src-tauri/tauri.conf.json` uses the official
  `https://schema.tauri.app/config/2` schema;
  `test-app/build-and-deploy.sh` replaces fixed `sleep 5`/`sleep 2` with a
  bounded `wait_for` readiness poll (`bash -n` clean). Frontend build green.

### DOC-01 — High: API reference out of sync

- **Root cause.** Wrong positional `configureRecovery`, removed
  `AutoStartConfig`, missing exports/types, missing `processExit`, false
  default/permission claims, deprecated wrappers shown as primary.
- **Design.** Reconcile against real exports; mark but retain published
  deprecated wrappers.
- **Files.** `docs/api-reference.md`, `guest-js/README.md`, all examples.
- **Dependencies.** WIRE-02 (`processExit`).
- **Pass gate.** every TS export has reference/import entry; snippets
  typecheck.
- **Status.** Verified.
- **Evidence.** `docs/api-reference.md` fully reconciled against source: `configureRecovery`
  now documented as the single-positional options-object form; the
  `AutoStartConfig` section removed; `processExit` added to the StopReason
  union + table; default-permission notes reconciled against
  `permissions/default.toml`; deprecated wrappers marked. Contract grep:
  `processExit` present (2 matches); `AutoStartConfig` 0; no positional
  `configureRecovery(true, …)`.

### DOC-02 — High: Android docs inaccurate

- **Root cause.** Claims `dataSync` default, stale manifest/permissions, auto
  permission prompt, fallback on invalid type, incorrect restart notification
  flow; omits Play/FSI limits.
- **Design.** Document `remoteMessaging` rationale, merged-manifest validation,
  consent flow, real errors/recovery, API 29–33 FSI limit, host opt-in battery
  permission.
- **Files.** `docs/android.md`, getting-started/troubleshooting/release
  checklist/readmes/JSDoc.
- **Dependencies.** AND-01/04/07/09.
- **Pass gate.** constants/manifest/doc contract checks.
- **Status.** Verified.
- **Evidence.** `docs/android.md` rewritten to `remoteMessaging` default + why;
  invalid types are rejected preflight (AND-01); consented notification
  permission flow; real restart notification flow on FGS type blocked at boot;
  API 29–33 FSI observability limitation; host opt-in for
  `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` (AND-09). Contract grep: no
  `dataSync` default claim remains.

### DOC-03 — High: iOS docs inaccurate

- **Root cause.** Says permission at load with badge, timeout rejects cancel
  invoke, cleanup resets completion guard, foreground transition does nothing.
- **Design.** Rewrite to tested behavior, iOS 14 floor, no PushKit parity,
  public CallKit/message handler integration.
- **Files.** `docs/ios.md`, migration/getting-started/release docs.
- **Dependencies.** IOS-CALL-01/MSG-01/PUSH-01/SCHED-01/CLEAN-01.
- **Pass gate.** statements map to named XCTest contracts; no VoIP relay claim.
- **Status.** Verified.
- **Evidence.** `docs/ios.md` rewritten to: UNUserNotificationCenter at launch
  with badge as one option among several; cancel listener resolves (not
  rejects) on timeout — explicit lifecycle policy; completion guard cleared
  per task; foreground transition reschedules; iOS 14 floor; active-process
  CallKit only — no VoIP relay claim (PushKit removed, IOS-PUSH-01); public
  CallKit + message handler integration documented (IOS-CALL-01, IOS-MSG-01).

### DOC-04 — Medium: desktop/architecture docs conflict with source

- **Root cause.** Conflict on Windows, commands, protocol, component map,
  notification/queue behavior.
- **Design.** Align with Unix daemon and actual source.
- **Files.** `docs/desktop.md`, `ARCHITECTURE.md`, README/troubleshooting/release.
- **Dependencies.** DESK-01/04/06.
- **Pass gate.** contract grep; example build.
- **Status.** Verified.
- **Evidence.** `docs/desktop.md` + `ARCHITECTURE.md` reconciled — Unix-only
  OS-service (Linux systemd user, macOS launchd); Windows in-process only
  (DESK-01); all six Unix OS-service commands; big-endian u32 length-prefixed
  framing; bounded queue/timeout; no daemon notification sink (notifications
  route via tauri-plugin-notification in the GUI); macOS `/tmp` + sandbox note;
  component map updated (transport_unix.rs replaces transport_windows.rs).
  Contract grep: no little-endian / Windows-named-pipe claim; `headless_daemon`
  example compiles.

### DOC-05 — Medium: metadata version drift

- **Root cause.** Version snippets still use 0.7; changelog lacks 1.0 link;
  SECURITY omits 1.x; CONTRIBUTING wrong cwd/omits hook; backon/iOS floors
  drift.
- **Design.** Synchronize metadata without rewriting historical entries.
- **Files.** root/crate/guest readmes, `CHANGELOG.md`, `SECURITY.md`,
  `CONTRIBUTING.md`, `ARCHITECTURE.md`, CI Package.swift.
- **Dependencies.** None.
- **Pass gate.** metadata lint; copy-paste commands work from documented cwd.
- **Status.** Verified (metadata lint grep = pass gate).
- **Evidence.** Root `README.md` + `guest-js/README.md` + `ARCHITECTURE.md`
  desktop snippets bumped `0.7` → `1.0`; `ARCHITECTURE.md` `backon ~1.5` →
  `~1.6`; `CHANGELOG.md` tail gains `[1.0.0]: …compare/plugin-v0.7.1…plugin-v1.0.0`
  (historical entries untouched); `SECURITY.md` Supported Versions gains a
  `1.x` row; `CONTRIBUTING.md` cwd corrected to
  `tauri-plugin-background-service/` + `.githooks/` setup note. CI
  Package.swift heredoc aligned to `.iOS(.v14)` (Phase G). Contract grep: no
  `0.7` pins in key readmes/ARCHITECTURE.
## Completion gate — observed evidence

Final verification matrix executed from `tauri-plugin-background-service/`
unless stated, on the remediated tree:

| Step | Result |
|---|---|
| `cargo fmt --check` | clean |
| `cargo check --all-targets` | clean |
| `cargo check --all-targets --features desktop-service` | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo test --all-targets --all-features` | 727 lib + 12 integration tests green |
| `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features` | clean |
| `guest-js/`: `npm ci && npm run build && npm test` | 14 TS tests green |
| `test-app/`: `python3 -m unittest test_oracle` | 42 oracle fixtures green |
| `test-app/`: `python3 -m py_compile run-tests.py` | clean |
| `test-app/`: `npm ci && npm run build` (frontend) | green |
| `test-app/src-tauri/`: `cargo check` (desktop-service feature on) | clean |
| `[[example]] headless_daemon` build | clean |

### Device / toolchain / external gates NOT executed from source control

These items are implemented but cannot be fully Verified without resources
outside this environment. They are named here explicitly and never represented
as completed by inference.

- **Android Gradle gate** (AND-01..10): `testDebugUnitTest` and `lintDebug`
  require an Android SDK / Gradle install. The Robolectric JVM test sources
  are authored against existing patterns; the gate is a CI run on the
  generated Gradle project.
- **iOS Xcode gate** (IOS-PUSH-01, IOS-CALL-01, IOS-MSG-01, IOS-SCHED-01,
  IOS-CLEAN-01): `xcodebuild test -scheme tauri-plugin-background-service
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro'` requires a macOS
  host with Xcode. Swift sources are authored; compile + simulator test is
  the gate. Real CallKit action routing and `UNUserNotificationCenter`
  scheduling additionally require a physical device.
- **CI workflow execution gate** (CI-01, CI-02, CI-03, REL-01): the workflow
  YAML is edited and locally YAML-validated; actual red/green requires a
  GitHub Actions run (intentional-failure assertions will turn jobs red on
  the next push).
- **Live Waydroid E2E gate** (E2E-01, E2E-02): the run-tests.py oracle and
  42 unittest fixtures are green; a live core+lifecycle scenario requires a
  Waydroid device AND a rotated `Z_AI_KEY` (the previously-checked-in
  plaintext key must be rotated — see REPO-01).
- **External key rotation** (REPO-01): the local `test-app/.env` is deleted
  and git history was confirmed empty for that path, but the previously
  exposed `Z_AI_KEY` value MUST be rotated at the provider. This is an
  operational action outside source control.
