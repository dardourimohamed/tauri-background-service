import UIKit
import BackgroundTasks
import PushKit
import UserNotifications
import WebKit
import os.log
import Tauri

/**
 Manages background service lifecycle on iOS using `BGTaskScheduler`.

 ## Required Info.plist Entries

 Add the following entries to your app's `Info.plist` to enable background task scheduling:

 ### BGTaskSchedulerPermittedIdentifiers

 A string array listing the task identifiers this plugin registers. The plugin uses
 two identifiers derived from your bundle identifier:

 ```
 <key>BGTaskSchedulerPermittedIdentifiers</key>
 <array>
     <string>$(BUNDLE_ID).bg-refresh</string>
     <string>$(BUNDLE_ID).bg-processing</string>
 </array>
 ```

 Replace `$(BUNDLE_ID)` with your app's actual bundle identifier (e.g. `com.example.myapp`).
 Omitting this key causes `BGTaskScheduler.shared.submit(_:)` to throw an error at runtime.

 ### UIBackgroundModes

 Include both `processing` and `fetch` modes:

 ```
 <key>UIBackgroundModes</key>
 <array>
     <string>processing</string>
     <string>fetch</string>
 </array>
 ```

 - `fetch` enables `BGAppRefreshTask` scheduling (~30s budget).
 - `processing` enables `BGProcessingTask` scheduling (minutes/hours,
   requires device idle).

 ## Task Behavior

 | Task Type | Budget | Safety Timer | Use Case |
 |-----------|--------|-------------|----------|
 | BGAppRefreshTask | ~30s | 28s (default) | Short periodic work |
 | BGProcessingTask | Minutes/hours | Optional | Long maintenance tasks |

 - Note: Force-quitting the app kills **all** background tasks. iOS will not relaunch
   force-killed apps. Only location/audio/VoIP background modes can relaunch after kill
   (App Store validates legitimate use).
*/
@objc public class BackgroundServicePlugin: Plugin, PKPushRegistryDelegate {

    // MARK: - Task Identifiers

    private var refreshTaskId: String {
        "\(Bundle.main.bundleIdentifier ?? "app").bg-refresh"
    }

    private var processingTaskId: String {
        "\(Bundle.main.bundleIdentifier ?? "app").bg-processing"
    }

    // MARK: - Test Seams (Wave 0 / H12)

    /// BGTaskScheduler seam. Defaults to the real `BGTaskScheduler.shared` wrapper;
    /// XCTest injects a recording fake. See `Seams.swift`.
    var scheduler: BGTaskScheduling = SystemBGTaskScheduler()

    /// Clock seam — wall-clock seconds since 1970. Defaults to `Date()`; XCTest
    /// injects a fixed clock so persisted timestamps are deterministic.
    var now: () -> TimeInterval = { Date().timeIntervalSince1970 }

    /// Completion seam — invoked to mark the active BGTask done. Defaults to calling
    /// the real task's `setTaskCompleted`; XCTest injects a recorder to count calls
    /// and prove the exactly-once invariant (Step 18 / I4).
    var completeTask: (BGTaskCompleting, Bool) -> Void = { task, success in
        task.setTaskCompleted(success: success)
    }

    /// Notification-authorization seam (M4). Defaults to the real
    /// `UNUserNotificationCenter` wrapper; XCTest injects a recording fake so the
    /// deferral can be proven without a real system prompt. See `Seams.swift`.
    var notificationAuthorizer: NotificationAuthorizing = SystemNotificationAuthorizer()

    /// App-foreground seam (M-NATIVE-4 / NR-6). Defaults to the live UIKit app
    /// state (`.active` == the webview is visible/foreground); XCTest injects a
    /// fixed value to drive the foreground ring gate without a real app lifecycle.
    var appIsForeground: () -> Bool = { UIApplication.shared.applicationState == .active }

    /// At-most-once guard for the deferred notification-authorization request (M4).
    /// Set the first time `requestNotificationAuthorizationIfNeeded()` runs so a
    /// repeated service start doesn't re-prompt within the process lifetime.
    private var notificationAuthorizationRequested = false

    // MARK: - State for BGTask lifecycle management

    /// Currently active BGAppRefreshTask, if any.
    private var currentRefreshTask: BGAppRefreshTask?

    /// Currently active BGProcessingTask, if any.
    /// iOS guarantees at most one BGTask is active at a time, so only one of
    /// `currentRefreshTask` or `currentProcessingTask` will be non-nil.
    private var currentProcessingTask: BGProcessingTask?

    /// Test-only stand-in for an active BGTask. Production never sets this — the
    /// real `BGAppRefreshTask`/`BGProcessingTask` instances (which have no public
    /// initializer) flow through `currentRefreshTask`/`currentProcessingTask`.
    /// XCTest injects a `FakeBGTask` here so the four terminal-path methods
    /// (expiration, safety timer, manual complete, stop) can be driven against a
    /// completion recorder and the exactly-once `setTaskCompleted` invariant proven
    /// across paths and races (Step 18 / I4). It is the lowest-priority active task
    /// in `completeActiveTask` and is cleared on every terminal path like the real
    /// refs, so leaving it set never changes production behavior.
    var injectedActiveTask: BGTaskCompleting?

    /// Whether a BGTask run is active and awaiting completion — either a real iOS
    /// task ref or the test-injected stand-in. Every terminal path keys its
    /// "had active task" decisions (record outcome, reschedule, complete) off this.
    private var hasActiveTask: Bool {
        currentRefreshTask != nil || currentProcessingTask != nil || injectedActiveTask != nil
    }

    /// Pending cancel invoke — shared between both task types since iOS runs at most one.
    private var pendingCancelInvoke: Invoke?

    /// Pending warm-delivery invoke (H14). The Rust warm-BGTask listener blocks on
    /// `waitForBgTask`; this holds that Invoke until a BGTask is delivered to the
    /// warm process (resolved by `resolvePendingWarmInvoke`) or the listener is
    /// torn down (rejected by `cancelWarmListener`).
    private var pendingWarmInvoke: Invoke?

    /// Safety timer — shared between both task types.
    private var safetyTimer: Timer?

    /// iOS safety timeout for BGAppRefreshTask (default: 28.0s).
    /// Set via `startKeepalive` args from Rust (PluginConfig).
    private var safetyTimeout: TimeInterval = 28.0

    /// Optional safety timeout for BGProcessingTask.
    /// When `nil` or `0`, no safety timer is started for processing tasks — only the
    /// iOS expiration handler terminates them. Set via `startKeepalive` args from Rust.
    private var processingSafetyTimeoutSecs: Double?

    /// BGAppRefreshTask earliest begin date in minutes from now (default: 15.0).
    /// Controls how soon iOS can launch the refresh task.
    private var earliestRefreshBeginMinutes: Double = 15.0

    /// BGProcessingTask earliest begin date in minutes from now (default: 15.0).
    /// Controls how soon iOS can launch the processing task.
    private var earliestProcessingBeginMinutes: Double = 15.0

    /// BGProcessingTask requires external power (default: false).
    private var requiresExternalPower: Bool = false

    /// BGProcessingTask requires network connectivity (default: false).
    private var requiresNetworkConnectivity: Bool = false

    /// Ceiling multiplier for the adaptive BGProcessingTask schedule (default: 4.0).
    /// The adaptive earliest-begin value never exceeds
    /// `earliestProcessingBeginMinutes * processingCeilingMultiplier`.
    private var processingCeilingMultiplier: Double = 4.0

    // MARK: - Pending Task Info

    /// Information about a BGTask that launched the app in the background.
    /// Queried by Rust on iOS setup to implement auto-start.
    private struct PendingTaskInfo {
        let taskKind: String       // "refresh" or "processing"
        let identifier: String     // BGTask identifier
        let receivedAt: TimeInterval // Date().timeIntervalSince1970
    }

    /// Currently pending BGTask info, set when a BGTask launches the app.
    /// Cleared by Rust after processing the auto-start.
    private var pendingTaskInfo: PendingTaskInfo?

    /// Whether `setTaskCompleted` has been called for the current BGTask.
    /// Prevents double-completion across all terminal paths (expiration, safety
    /// timer, explicit stop, natural completion).
    private var taskCompleted: Bool = false

    // MARK: - Desired State Keys

    /// UserDefaults keys for iOS desired-state persistence.
    private enum DesiredStateKeys {
        static let desiredRunning = "ios_desired_running"
        static let lastStartConfig = "ios_last_start_config"
        static let lastScheduleError = "ios_last_schedule_error"
        static let lastTaskKind = "ios_last_task_kind"
        static let lastTaskStartedAt = "ios_last_task_started_at"
        static let lastTaskCompletedAt = "ios_last_task_completed_at"
        /// Submit-result snapshot of the most recent `scheduleNext()`, read back
        /// by `getSchedulingStatus` (the submit-result half of the C1 split).
        static let lastRefreshScheduled = "ios_last_refresh_scheduled"
        static let lastProcessingScheduled = "ios_last_processing_scheduled"
        static let lastRefreshError = "ios_last_refresh_error"
        static let lastProcessingError = "ios_last_processing_error"
        /// Outcome of the last BGTask run: "completed" | "expired".
        /// Consumed (removed) by `scheduleNext()` after one adaptation step.
        static let lastTaskOutcome = "ios_last_task_outcome"
        /// Durable reason the last BGTask run ended: "completed" | "expired".
        /// Written alongside `lastTaskOutcome` but **never consumed** by
        /// `scheduleNext()`, so the "why did the last run end?" status question
        /// (M7) is always answerable — surfaced by `getDesiredStateStatus`.
        static let lastCompletionReason = "ios_last_completion_reason"
        /// Whether notification authorization was granted (M4). Persisted when the
        /// deferred authorization request resolves, surfaced by
        /// `getDesiredStateStatus` so Rust's Notifier can degrade. Absent until the
        /// first notification-requiring intent (service start) requests it.
        static let notificationGranted = "ios_notification_granted"
        /// Adaptive BGProcessingTask earliest-begin value in minutes.
        /// Always within [configured floor, configured * ceiling multiplier].
        static let adaptiveProcessingBeginMinutes = "ios_adaptive_processing_begin_minutes"
    }

    // MARK: - Pending Task Keys

    /// UserDefaults keys for iOS pending BGTask persistence.
    /// Survives timing gaps between BGTask handler and Rust setup.
    private enum PendingTaskKeys {
        static let kind = "ios_pending_task_kind"
        static let identifier = "ios_pending_task_identifier"
        static let receivedAt = "ios_pending_task_received_at"
        static let consumedAt = "ios_pending_task_consumed_at"
        /// H3: timestamp of the last cold auto-start that failed. Stamped
        /// instead of clearing the pending record so the evidence survives.
        static let lastFailedAt = "ios_pending_task_last_failed_at"
    }

    // MARK: - Scheduling Result

    /// Result of submitting BGTaskScheduler requests.
    private struct SchedulingResult {
        let refreshScheduled: Bool
        let processingScheduled: Bool
        let refreshError: String?
        let processingError: String?
    }

    // MARK: - UserDefaults Helpers

    private func persistDesiredRunning(_ running: Bool) {
        UserDefaults.standard.set(running, forKey: DesiredStateKeys.desiredRunning)
    }

    private func persistStartConfig(_ args: [String: Any]) {
        if let data = try? JSONSerialization.data(withJSONObject: args, options: []),
           let json = String(data: data, encoding: .utf8) {
            UserDefaults.standard.set(json, forKey: DesiredStateKeys.lastStartConfig)
        }
    }

    /// Persist the submit-result snapshot of a `scheduleNext()` call so the
    /// `getSchedulingStatus` query reports real facts about the most recent
    /// scheduling attempt rather than re-submitting on read.
    private func persistSchedulingResult(_ result: SchedulingResult) {
        let defaults = UserDefaults.standard
        defaults.set(result.refreshScheduled, forKey: DesiredStateKeys.lastRefreshScheduled)
        defaults.set(result.processingScheduled, forKey: DesiredStateKeys.lastProcessingScheduled)
        if let error = result.refreshError {
            defaults.set(error, forKey: DesiredStateKeys.lastRefreshError)
        } else {
            defaults.removeObject(forKey: DesiredStateKeys.lastRefreshError)
        }
        if let error = result.processingError {
            defaults.set(error, forKey: DesiredStateKeys.lastProcessingError)
        } else {
            defaults.removeObject(forKey: DesiredStateKeys.lastProcessingError)
        }
    }

    private func persistScheduleError(_ error: String?) {
        if let error = error {
            UserDefaults.standard.set(error, forKey: DesiredStateKeys.lastScheduleError)
        } else {
            UserDefaults.standard.removeObject(forKey: DesiredStateKeys.lastScheduleError)
        }
    }

    private func persistTaskKind(_ kind: String) {
        UserDefaults.standard.set(kind, forKey: DesiredStateKeys.lastTaskKind)
    }

    private func persistTaskStartedAt() {
        UserDefaults.standard.set(now(), forKey: DesiredStateKeys.lastTaskStartedAt)
    }

    private func persistTaskCompletedAt() {
        UserDefaults.standard.set(now(), forKey: DesiredStateKeys.lastTaskCompletedAt)
    }

    /// Persist the outcome of the BGTask run that just ended
    /// ("completed" | "expired"). Writes two keys with the same value:
    /// - `lastTaskOutcome`: read once by `scheduleNext()` to adapt the
    ///   processing schedule, then consumed (removed).
    /// - `lastCompletionReason`: the **durable** copy that `scheduleNext()` never
    ///   removes, so `getDesiredStateStatus` can always report why the last run
    ///   ended (M7's "why?" question).
    ///
    /// `internal` (not `private`) so the XCTest target can assert both keys are
    /// written, mirroring the seam visibility chosen in Step 15.
    func persistTaskOutcome(_ outcome: String) {
        let defaults = UserDefaults.standard
        defaults.set(outcome, forKey: DesiredStateKeys.lastTaskOutcome)
        defaults.set(outcome, forKey: DesiredStateKeys.lastCompletionReason)
    }

    /// Persist pending BGTask info to UserDefaults.
    /// Called when a BGTask handler fires so the info survives timing gaps
    /// between the native handler and Rust setup.
    private func persistPendingTaskInfo(kind: String, identifier: String, receivedAt: TimeInterval) {
        let defaults = UserDefaults.standard
        defaults.set(kind, forKey: PendingTaskKeys.kind)
        defaults.set(identifier, forKey: PendingTaskKeys.identifier)
        defaults.set(receivedAt, forKey: PendingTaskKeys.receivedAt)
        defaults.set(nil, forKey: PendingTaskKeys.consumedAt)
    }

    // MARK: - Plugin Lifecycle

    public override func load(webview: WKWebView) {
        super.load(webview: webview)

        // M4: notification authorization is NOT requested here. Prompting at plugin
        // load — before any user intent — hurts App Store optics and couples the
        // permission to load rather than the feature that needs it. The request is
        // deferred to the first notification-requiring intent (service start) via
        // `requestNotificationAuthorizationIfNeeded()`.

        // Register both BGTask handlers before the app finishes launching.
        let refreshId = refreshTaskId
        let processingId = processingTaskId

        scheduler.register(forTaskWithIdentifier: refreshId, using: .main) {
            [weak self] task in
            if let bgTask = task as? BGAppRefreshTask {
                self?.handleBackgroundTask(bgTask)
            } else {
                task.setTaskCompleted(success: false)
            }
        }

        scheduler.register(forTaskWithIdentifier: processingId, using: .main) {
            [weak self] task in
            if let bgTask = task as? BGProcessingTask {
                self?.handleProcessingTask(bgTask)
            } else {
                task.setTaskCompleted(success: false)
            }
        }

        // Foreground/background transition observers.
        // When going to background with desired_running=true and no active BGTask,
        // ensure BGTasks are scheduled so iOS can manage the lifecycle.
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(appDidEnterBackground),
            name: UIApplication.didEnterBackgroundNotification,
            object: nil
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(appWillEnterForeground),
            name: UIApplication.willEnterForegroundNotification,
            object: nil
        )

        // BGS-08 (Step 18 Task B): register the VoIP-push registry. iOS 13+
        // REQUIRES an app declaring the `voip` UIBackgroundMode (Step 17) to
        // register a PKPushRegistry and report each VoIP push to CallKit, else
        // Apple rejects the binary + the OS terminates future pushes.
        registerPushKit()
    }

    // MARK: - Notification Authorization (M4)

    /// Request notification authorization at the first notification-requiring
    /// intent rather than at `load()` (M4). Service start (`startKeepalive`) is that
    /// intent: once running, the lifecycle Notifier posts timeout/recovery
    /// notifications, so this is the point the permission is actually needed.
    ///
    /// - Requests at most once per process (`notificationAuthorizationRequested`).
    /// - Requests only the types the lifecycle Notifier posts (`.alert`, `.sound`);
    ///   `.badge` is not used, so it is not over-requested.
    /// - Forwards `granted` into the Rust-readable durable store (persisted under
    ///   `ios_notification_granted`, surfaced by `getDesiredStateStatus`) so Rust's
    ///   Notifier can degrade when notifications are denied.
    ///
    /// Service start never depends on the outcome — denial only degrades
    /// notifications, it does not block the background service.
    private func requestNotificationAuthorizationIfNeeded() {
        guard !notificationAuthorizationRequested else { return }
        notificationAuthorizationRequested = true
        notificationAuthorizer.requestAuthorization(options: [.alert, .sound]) {
            [weak self] granted, _ in
            self?.persistNotificationGranted(granted)
        }
    }

    /// Persist the notification-authorization decision into the Rust-readable
    /// durable store. The completion handler may fire on an arbitrary queue;
    /// `UserDefaults` is thread-safe, so no marshalling is needed.
    private func persistNotificationGranted(_ granted: Bool) {
        UserDefaults.standard.set(granted, forKey: DesiredStateKeys.notificationGranted)
    }

    // MARK: - Main-Queue Serialization (H1)

    /// Run `body` on the main queue so every mutation of the five shared BGTask
    /// fields (`currentRefreshTask`, `currentProcessingTask`, `pendingCancelInvoke`,
    /// `safetyTimer`, `taskCompleted`) happens in one execution context.
    ///
    /// Tauri dispatches `@objc` command handlers on the `ipc` queue, while
    /// `BGTaskScheduler` launch/expiration handlers and the safety `Timer` fire on
    /// `.main`. Marshalling the command bodies here puts both on `.main`, removing
    /// the cross-queue data race (the exactly-once `setTaskCompleted` invariant and
    /// single-response invokes previously held only by luck).
    ///
    /// When already on the main thread (the BGTask/notification handlers, and
    /// XCTest), `body` runs inline so callers keep their synchronous semantics.
    private func onMain(_ body: @escaping () -> Void) {
        if Thread.isMainThread {
            body()
        } else {
            DispatchQueue.main.async(execute: body)
        }
    }

    // MARK: - Completion Safety

    /// Safely complete the active BGTask exactly once.
    ///
    /// iOS requires `setTaskCompleted` to be called exactly once per BGTask.
    /// This method guards against double-completion by checking the `taskCompleted`
    /// flag before calling `setTaskCompleted`. The flag is reset only when a new
    /// BGTask handler fires (`handleBackgroundTask`/`handleProcessingTask`) — not in
    /// `cleanup()`, so the exactly-once guard survives end-of-run teardown (M3).
    ///
    /// - Returns: `true` if a task was completed, `false` if already completed or no task.
    @discardableResult
    private func completeActiveTask(success: Bool) -> Bool {
        // H1: completion mutates the shared task refs + `taskCompleted` flag, so it
        // must run on the one serialized context. All callers reach here via a
        // `.main`-marshalled handler body or a `.main`-bound BGTask handler.
        dispatchPrecondition(condition: .onQueue(.main))
        guard !taskCompleted else { return false }

        if let task = currentRefreshTask {
            currentRefreshTask = nil
            return completeOnce(task, success: success)
        } else if let task = currentProcessingTask {
            currentProcessingTask = nil
            return completeOnce(task, success: success)
        } else if let task = injectedActiveTask {
            // Test-only path: a `FakeBGTask` injected so XCTest can drive the
            // terminal paths (Step 18 / I4). Never reached in production.
            injectedActiveTask = nil
            return completeOnce(task, success: success)
        }
        return false
    }

    /// Guarded one-shot completion — the single place `setTaskCompleted` is invoked
    /// (through the `completeTask` seam). Honors the `taskCompleted` flag so the call
    /// happens at most once per task. Internal so XCTest can drive it with a
    /// `FakeBGTask` and prove the exactly-once count (Step 18 / I4).
    @discardableResult
    func completeOnce(_ task: BGTaskCompleting, success: Bool) -> Bool {
        // H1: the sole `setTaskCompleted` call site reads-then-sets the shared
        // `taskCompleted` flag — only sound on the serialized main queue.
        dispatchPrecondition(condition: .onQueue(.main))
        guard !taskCompleted else { return false }
        taskCompleted = true
        completeTask(task, success)
        return true
    }

    // MARK: - Adaptive Processing Schedule (D2)

    /// Outcome of the most recent BGTask run, as persisted under
    /// `ios_last_task_outcome`.
    enum TaskOutcome: Equatable {
        /// The run finished before iOS expired it (budget was sufficient).
        case completedNaturally
        /// iOS (or the safety timer) terminated the run early.
        case expired
        /// No outcome recorded, or an unrecognized persisted value.
        case unknown

        /// Map a persisted UserDefaults string back to an outcome.
        init(persisted: String?) {
            switch persisted {
            case "completed": self = .completedNaturally
            case "expired": self = .expired
            default: self = .unknown
            }
        }
    }

    /// Compute the next BGProcessingTask earliest-begin offset in minutes,
    /// adapting to the outcome of the last run. Pure and static so XCTest
    /// covers the policy without BGTaskScheduler.
    ///
    /// Policy (processing-kind runs only — refresh runs never move the value):
    /// - `.expired` → back off: `min(previous * 1.5, configured * ceilingMultiplier)`
    /// - `.completedNaturally` → tighten: `max(previous / 1.5, configured)`
    /// - `.unknown` → hold `previous`
    ///
    /// Guards — no Rust-side validation exists, so bad config must not poison
    /// the persisted adaptive value:
    /// - `ceilingMultiplier` non-finite or <= 1 → effective ceiling is `configured`
    /// - `previous` non-finite or <= 0 → treated as "no previous" (`configured`)
    /// - `configured` non-finite or <= 0 → falls back to the 15-minute default
    /// - the result is always clamped into [floor, ceiling], never NaN
    ///
    /// `lastStartedAt`/`lastCompletedAt` are part of the persisted run record
    /// and reserved for duration-based policies; the current policy keys off
    /// the explicit outcome only.
    static func adaptiveProcessingBeginMinutes(
        configured: Double,
        ceilingMultiplier: Double,
        lastStartedAt: Date?,
        lastCompletedAt: Date?,
        lastTaskKind: String?,
        lastOutcome: TaskOutcome,
        previous: Double
    ) -> Double {
        let floor = (configured.isFinite && configured > 0) ? configured : 15.0
        let ceiling = (ceilingMultiplier.isFinite && ceilingMultiplier > 1)
            ? floor * ceilingMultiplier
            : floor
        let prev = (previous.isFinite && previous > 0) ? previous : floor

        // Only processing-kind runs carry information about the processing
        // budget; refresh runs (or no recorded run) hold the value.
        guard lastTaskKind == "processing" else {
            return min(max(prev, floor), ceiling)
        }

        let next: Double
        switch lastOutcome {
        case .expired:
            next = prev * 1.5
        case .completedNaturally:
            next = prev / 1.5
        case .unknown:
            next = prev
        }
        return min(max(next, floor), ceiling)
    }

    // MARK: - BGAppRefreshTask Handler

    private func handleBackgroundTask(_ task: BGAppRefreshTask) {
        self.currentRefreshTask = task
        self.taskCompleted = false

        let receivedAt = now()

        // Store pending task info for Rust auto-start on BGTask launch.
        self.pendingTaskInfo = PendingTaskInfo(
            taskKind: "refresh",
            identifier: refreshTaskId,
            receivedAt: receivedAt
        )

        // Persist to UserDefaults so info survives timing gaps.
        persistPendingTaskInfo(kind: "refresh", identifier: refreshTaskId, receivedAt: receivedAt)

        persistTaskKind("refresh")
        persistTaskStartedAt()

        // H14: wake the warm-BGTask listener so Rust starts the service now,
        // rather than only persisting pending and waiting.
        resolvePendingWarmInvoke()

        task.expirationHandler = { [weak self] in
            self?.handleExpiration()
        }

        // Always start safety timer for refresh tasks (default: 28s)
        startSafetyTimer(with: safetyTimeout)
    }

    // MARK: - BGProcessingTask Handler

    private func handleProcessingTask(_ task: BGProcessingTask) {
        self.currentProcessingTask = task
        self.taskCompleted = false

        let receivedAt = now()

        // Store pending task info for Rust auto-start on BGTask launch.
        self.pendingTaskInfo = PendingTaskInfo(
            taskKind: "processing",
            identifier: processingTaskId,
            receivedAt: receivedAt
        )

        // Persist to UserDefaults so info survives timing gaps.
        persistPendingTaskInfo(kind: "processing", identifier: processingTaskId, receivedAt: receivedAt)

        persistTaskKind("processing")
        persistTaskStartedAt()

        // H14: wake the warm-BGTask listener so Rust starts the service now,
        // rather than only persisting pending and waiting.
        resolvePendingWarmInvoke()

        task.expirationHandler = { [weak self] in
            self?.handleExpiration()
        }

        // Only start safety timer for processing tasks if an explicit timeout was configured
        if let timeout = processingSafetyTimeoutSecs, timeout > 0 {
            startSafetyTimer(with: timeout)
        }
    }

    // MARK: - Expiration Handler (signals Rust to cancel)

    func handleExpiration() {
        dispatchPrecondition(condition: .onQueue(.main))
        persistTaskCompletedAt()

        // L3: only an active run carries an outcome / cancel listener / reschedule.
        // A stray expiration with no task running records nothing and never
        // submits, mirroring `handleSafetyTimerExpiration`.
        guard hasActiveTask else { return }

        // iOS killed the run early — the next scheduleNext() backs off.
        persistTaskOutcome("expired")

        // Resolve pending cancel invoke (unblocks Rust thread)
        if let invoke = pendingCancelInvoke {
            invoke.resolve()
            pendingCancelInvoke = nil
        }

        // Complete + reschedule + cleanup via the shared guarded finish helper.
        finishRun(success: false)
    }

    // MARK: - Safety Timer

    private func startSafetyTimer(with interval: TimeInterval) {
        safetyTimer?.invalidate()
        safetyTimer = Timer.scheduledTimer(withTimeInterval: interval, repeats: false) { [weak self] _ in
            self?.handleSafetyTimerExpiration()
        }
    }

    func handleSafetyTimerExpiration() {
        dispatchPrecondition(condition: .onQueue(.main))
        persistTaskCompletedAt()

        // Force-complete the task only if Rust never called completeBgTask.
        guard hasActiveTask else { return }

        // Self-imposed budget expiry — same adaptation signal as an iOS
        // expiration: the run did not finish within its budget.
        persistTaskOutcome("expired")

        // Resolve pending cancel invoke (unblocks Rust thread)
        if let invoke = pendingCancelInvoke {
            invoke.resolve()
            pendingCancelInvoke = nil
        }

        // Complete + reschedule + cleanup via the shared guarded finish helper.
        finishRun(success: false)
    }

    // MARK: - Cleanup

    func cleanup() {
        currentRefreshTask = nil
        currentProcessingTask = nil
        injectedActiveTask = nil
        pendingCancelInvoke = nil
        safetyTimer?.invalidate()
        safetyTimer = nil
        // M3: do NOT reset `taskCompleted` here. The flag tracks one task's
        // lifetime and is reset only when a new BGTask handler begins
        // (`handleBackgroundTask`/`handleProcessingTask`). Resetting it on every
        // terminal path (cleanup runs after each) would re-open the one-shot
        // completion guard while a just-completed task's refs are torn down.
    }

    // MARK: - Shared Terminal Finish (L3 / M3 / I4)

    /// The one guarded finish helper shared by every path that ends a BGTask run
    /// (iOS expiration, the safety timer, and Rust-driven natural completion):
    /// complete the active task exactly once, reschedule **only** when a task was
    /// actually active, then clear the remaining lifecycle state.
    ///
    /// Capturing `hadActiveTask` before completing guards against a stray terminal
    /// path that fires with nothing running — it must not submit a spurious BGTask
    /// request (L3).
    ///
    /// - Returns: whether a task was active when this ran.
    @discardableResult
    private func finishRun(success: Bool) -> Bool {
        dispatchPrecondition(condition: .onQueue(.main))
        let hadActiveTask = hasActiveTask
        completeActiveTask(success: success)
        if hadActiveTask {
            scheduleNext()
        }
        cleanup()
        return hadActiveTask
    }

    // MARK: - waitForCancel (Pending Invoke pattern)

    @objc public func waitForCancel(_ invoke: Invoke) {
        onMain {
            // M1: reject any previously-held cancel invoke (a stale Rust thread)
            // with "superseded" before storing the new one, so a superseded
            // listener never leaks unanswered. Mirrors `waitForBgTask`.
            if let stale = self.pendingCancelInvoke {
                stale.reject("superseded")
            }
            // Store the invoke — it will be resolved by expiration/completion
            // or rejected by stopKeepalive, regardless of BGTask state.
            self.pendingCancelInvoke = invoke
        }
    }

    // MARK: - cancelCancelListener (timeout unblock)

    /// Reject the pending cancel invoke to unblock the Rust `spawn_blocking` thread.
    ///
    /// Called from Rust when the cancel listener timeout fires (default: 4h).
    /// This ensures the `wait_for_cancel` thread does not leak indefinitely
    /// when iOS never resolves the invoke (e.g., app killed in background).
    @objc public func cancelCancelListener(_ invoke: Invoke) {
        onMain {
            if let cancelInvoke = self.pendingCancelInvoke {
                cancelInvoke.reject("cancelled")
                self.pendingCancelInvoke = nil
            }
            invoke.resolve()
        }
    }

    // MARK: - waitForBgTask (warm-delivery Pending Invoke pattern, H14)

    /// Block the Rust warm-BGTask listener until iOS delivers a BGTask to the
    /// warm process. Mirrors `waitForCancel`: the invoke is stored without
    /// resolving, blocking the Rust `spawn_blocking` thread until a delivery
    /// resolves it (`resolvePendingWarmInvoke`) or teardown rejects it
    /// (`cancelWarmListener`).
    @objc public func waitForBgTask(_ invoke: Invoke) {
        onMain {
            // Reject any previously-held warm invoke (stale Rust thread) before
            // storing the new one, so only one warm listener is ever blocked.
            if let stale = self.pendingWarmInvoke {
                stale.reject("superseded")
            }
            self.pendingWarmInvoke = invoke
        }
    }

    /// Reject the pending warm invoke to unblock the Rust `spawn_blocking` thread
    /// on teardown. Mirrors `cancelCancelListener`.
    @objc public func cancelWarmListener(_ invoke: Invoke) {
        onMain {
            if let warmInvoke = self.pendingWarmInvoke {
                warmInvoke.reject("cancelled")
                self.pendingWarmInvoke = nil
            }
            invoke.resolve()
        }
    }

    /// Resolve the pending warm invoke, unblocking the Rust warm-BGTask listener
    /// so it can drive `run_warm_start`. Called by the BGTask handlers after the
    /// pending record is persisted. No-op when no warm listener is blocked.
    func resolvePendingWarmInvoke() {
        onMain {
            if let warmInvoke = self.pendingWarmInvoke {
                warmInvoke.resolve()
                self.pendingWarmInvoke = nil
            }
        }
    }

    // MARK: - completeBgTask (Rust→Swift completion signal)

    @objc public func completeBgTask(_ invoke: Invoke) {
        onMain {
            // Extract success value from invoke arguments
            let success = invoke.anyArgs?["success"] as? Bool ?? true

            // Track whether we had an active BGTask before completion.
            // Prevents spurious rescheduling when completeBgTask is called
            // after expiration or explicit stop already cleaned up the task.
            let hadActiveTask = self.hasActiveTask

            // Natural completion — the run ended before iOS expired it. Recorded
            // regardless of the success flag: adaptation keys off whether the
            // budget ran out, not whether the work itself succeeded.
            if hadActiveTask {
                self.persistTaskOutcome("completed")
            }

            // Reject pending cancel invoke (unblocks Rust thread)
            if let cancelInvoke = self.pendingCancelInvoke {
                cancelInvoke.reject("cancelled")
                self.pendingCancelInvoke = nil
            }

            // Complete + reschedule (only if a task was active) + cleanup, via the
            // shared guarded finish helper. Avoids scheduling when called after
            // expiration or stop already handled it.
            self.finishRun(success: success)

            // Resolve this invoke
            invoke.resolve()
        }
    }

    // MARK: - PushKit VoIP-push registry (BGS-08, doc-08 Step 18 Task B)

    /// The on-device VoIP-push registry. iOS 13+ REQUIRES an app declaring the
    /// `voip` UIBackgroundMode (Step 17) to register a `PKPushRegistry` and report
    /// each incoming VoIP push to CallKit — otherwise Apple rejects the binary at
    /// submission and the OS terminates subsequent pushes (App-Store gate, release
    /// condition #7). Retained for the process lifetime so it keeps delivering
    /// tokens + pushes. The APNs RELAY that produces the pushes is an external
    /// dependency (Fork 2); this registry tracks the on-device state only, and
    /// `push_relay_configured` (Task A's status surface) reflects "token
    /// registered", NOT "relay delivering".
    private var pushRegistry: PKPushRegistry?

    /// Register the VoIP-push registry. Callbacks arrive on `queue` (main): the
    /// device token via `didUpdate`, each VoIP push via `didReceiveIncomingPushWith`.
    /// Idempotent — the property guard prevents a second registration.
    private func registerPushKit() {
        guard pushRegistry == nil else { return }
        let registry = PKPushRegistry(queue: .main)
        registry.delegate = self
        registry.desiredPushTypes = [.voIP]
        pushRegistry = registry
    }

    // MARK: PKPushRegistryDelegate

    func pushRegistry(
        _ registry: PKPushRegistry,
        didUpdate pushCredentials: PKPushCredentials,
        for type: PKPushType
    ) {
        // Persist the VoIP device token into the doc-08 token store so
        // `MobilePackagingValidation.push_relay_configured` reflects the on-device
        // registry state (Task A's status surface). The token is re-issued on
        // every launch via PKPushRegistry, so an in-memory store is sufficient.
        let token = pushCredentials.token.map { String(format: "%02x", $0) }.joined()
        token.withCString { SilaNativeFFI.silaSetPushToken($0) }
    }

    func pushRegistry(
        _ registry: PKPushRegistry,
        didReceiveIncomingPushWith payload: [AnyHashable: Any],
        for type: PKPushType,
        completion: @escaping () -> Void
    ) {
        // iOS 13+: EVERY VoIP push MUST be reported to CallKit immediately (within
        // a few seconds), else iOS terminates the app and blocks future pushes.
        // The external APNs relay (Fork 2) is the producer; its payload carries the
        // call_id/callerName/hasVideo. Missing fields degrade (callerName
        // "Unknown"; a malformed call_id falls back to a random CallKit UUID inside
        // `reportIncomingCall`), but the system obligation to report is ALWAYS met.
        // The exact payload contract + malformed-call_id UX are device-verified in
        // Step 21 — no relay ships in v1, so this delegate does not fire yet.
        guard type == .voIP else { completion(); return }
        let callId = (payload["callId"] as? String) ?? ""
        let callerName = (payload["callerName"] as? String) ?? "Unknown"
        let hasVideo = (payload["hasVideo"] as? Bool) ?? false
        callKitController.reportIncomingCall(
            callId: callId, callerName: callerName, hasVideo: hasVideo)
        completion()
    }

    func pushRegistry(
        _ registry: PKPushRegistry,
        didInvalidatePushTokenCapabilitiesForType type: PKPushType
    ) {
        // Token revoked (user unregisters / capabilities invalidated) — clear the
        // store so `push_relay_configured` returns to false.
        SilaNativeFFI.silaClearPushToken()
    }

    // MARK: - CallKit incoming-call commands (spec 08 C6, Step 16)

    /// Lazily-initialized CallKit controller. Reports calls to the system and configures the
    /// VOIP audio session. Reached while foreground/background-active (F3 degraded mode) AND
    /// from the VoIP-push delegate (`pushRegistry(_:didReceiveIncomingPushWith:for:completion:)`),
    /// which iOS 13+ wakes a suspended app to deliver.
    lazy var callKitController: SilaCallKitController = {
        let controller = SilaCallKitController()
        // DEPRECATED FALLBACK (BGS-10, Step 18 Task B): the perform-handlers now
        // route Answer/Reject/End DIRECTLY to Rust via `sila_call_action` (the
        // webview is suspended when CallKit rings), so this `onCallEvent` →
        // `trigger` wiring is NO LONGER the active path for those actions.
        // Retained as the dormant fallback seam for a future webview-foreground
        // path; the original 32-hex `call_id` still rides the payload.
        controller.onCallEvent = { [weak self] event, callId in
            self?.trigger(event, data: ["callId": callId])
        }
        return controller
    }()

    /// Native handler for the Tauri mobile-plugin invoke `showIncomingCall` (routed by
    /// `MobileLifecycle::show_incoming_call`). Reports the call to CallKit → system ring +
    /// in-call UI. Mirrors the Android `BackgroundServicePlugin.showIncomingCall` handler
    /// (Step 15); the Rust dispatch (`run_mobile_plugin`) is platform-agnostic, so this is
    /// the symmetric iOS half.
    @objc public func showIncomingCall(_ invoke: Invoke) {
        onMain {
            let args = invoke.anyArgs
            let callId = (args?["callId"] as? String) ?? ""
            let callerName = (args?["callerName"] as? String) ?? ""
            let isVideo = (args?["isVideo"] as? Bool) ?? false
            // L5: a malformed call_id would strand a random-UUID call CallKit can
            // never dismiss; an empty callerName has nothing to show. Reject so the
            // call never rings rather than ringing a broken call.
            guard SilaCallKitController.isValidCallId(callId) else {
                invoke.reject("invalidCallId")
                return
            }
            guard !callerName.isEmpty else {
                invoke.reject("invalidCallerName")
                return
            }
            // M-NATIVE-4 / NR-6 (DEC-060): one ring owner per app-state. While the
            // webview is foreground/visible the in-app IncomingCallScreen owns the
            // ring, so suppress the CallKit ring (no double-ring). In production the
            // Rust `should_ring_native` gate already suppresses this path when
            // foreground; this is the independent iOS-side check reading the real OS
            // app state — defense-in-depth behind that gate.
            guard SilaCallDecision.shouldRingCallKit(appForeground: self.appIsForeground()) else {
                invoke.resolve()
                return
            }
            self.callKitController.reportIncomingCall(callId: callId, callerName: callerName, hasVideo: isVideo)
            invoke.resolve()
        }
    }

    /// Native handler for the Tauri mobile-plugin invoke `cancelIncomingCall`. Ends the
    /// CallKit call (caller hung up / answered-elsewhere / rejected).
    @objc public func cancelIncomingCall(_ invoke: Invoke) {
        onMain {
            let args = invoke.anyArgs
            let callId = (args?["callId"] as? String) ?? ""
            // L5: reject a malformed id (it could never have been reported, so
            // there is nothing to dismiss) rather than ending a random-UUID call.
            guard SilaCallKitController.isValidCallId(callId) else {
                invoke.reject("invalidCallId")
                return
            }
            self.callKitController.endCall(callId: callId, reason: .remoteEnded)
            invoke.resolve()
        }
    }

    /// Native handler for the Tauri mobile-plugin invoke `setCallAudioRoute`
    /// (M-NATIVE-3 / CCF-11, Step 11; routed by `MobileLifecycle::set_call_audio_route`).
    /// Applies the device audio route via `AVAudioSession.overrideOutputAudioPort`.
    /// An unknown route falls back to `.system` (platform-managed, no override).
    @objc public func setCallAudioRoute(_ invoke: Invoke) {
        onMain {
            let routeRaw = (invoke.anyArgs?["route"] as? String) ?? "system"
            let route = SilaAudioRoute(rawValue: routeRaw) ?? .system
            self.callKitController.setAudioRoute(route)
            invoke.resolve()
        }
    }

    /// Native handler for the Tauri mobile-plugin invoke `openAppSettings`
    /// (M-DIAG-2 / CCF-12, Step 17; routed by `MobileLifecycle::open_app_settings`).
    /// Opens this app's iOS Settings page (`UIApplication.openSettingsURLString`)
    /// so the user can grant a previously-denied camera/mic permission.
    @objc public func openAppSettings(_ invoke: Invoke) {
        onMain {
            guard let url = URL(string: UIApplication.openSettingsURLString) else {
                invoke.reject("app settings URL unavailable")
                return
            }
            UIApplication.shared.open(url, options: [:], completionHandler: nil)
            invoke.resolve()
        }
    }

    // MARK: - startKeepalive (configurable iOS safety timers)

    @objc public func startKeepalive(_ invoke: Invoke) {
        onMain {
            // M4: service start is the first notification-requiring intent — request
            // notification authorization here (deferred out of `load()`), at most
            // once. The outcome never gates the start below.
            self.requestNotificationAuthorizationIfNeeded()

            let args = invoke.anyArgs
            if let args = args {
                // BGAppRefreshTask safety timeout (default: 28.0s via PluginConfig)
                if let timeout = args["iosSafetyTimeoutSecs"] as? Double {
                    self.safetyTimeout = timeout
                }
                // BGProcessingTask safety timeout (default: nil = no cap)
                if let processingTimeout = args["iosProcessingSafetyTimeoutSecs"] as? Double {
                    self.processingSafetyTimeoutSecs = processingTimeout
                }
                // BGAppRefreshTask earliest begin date in minutes
                if let minutes = args["iosEarliestRefreshBeginMinutes"] as? Double {
                    self.earliestRefreshBeginMinutes = minutes
                }
                // BGProcessingTask earliest begin date in minutes
                if let minutes = args["iosEarliestProcessingBeginMinutes"] as? Double {
                    self.earliestProcessingBeginMinutes = minutes
                }
                // BGProcessingTask requires external power
                if let power = args["iosRequiresExternalPower"] as? Bool {
                    self.requiresExternalPower = power
                }
                // BGProcessingTask requires network connectivity
                if let network = args["iosRequiresNetworkConnectivity"] as? Bool {
                    self.requiresNetworkConnectivity = network
                }
                // Adaptive processing schedule ceiling multiplier (default: 4.0)
                if let multiplier = args["iosProcessingCeilingMultiplier"] as? Double {
                    self.processingCeilingMultiplier = multiplier
                }
            }

            let result = self.scheduleNext()

            // Persist desired state
            self.persistDesiredRunning(true)
            if let args = args {
                self.persistStartConfig(args)
            }
            // M2: `lastScheduleError` was already written by `scheduleNext` (single
            // source of truth) above; no separate persist here.
            UserDefaults.standard.removeObject(forKey: DesiredStateKeys.lastTaskCompletedAt)

            // If both scheduling attempts failed, reject with schedulerUnavailable
            if !result.refreshScheduled && !result.processingScheduled {
                invoke.reject("schedulerUnavailable")
                return
            }

            // Return structured scheduling result
            invoke.resolve([
                "refreshScheduled": result.refreshScheduled,
                "processingScheduled": result.processingScheduled,
                "refreshError": result.refreshError ?? NSNull(),
                "processingError": result.processingError ?? NSNull()
            ] as JsonObject)
        }
    }

    // MARK: - stopKeepalive (clean up active task)

    @objc public func stopKeepalive(_ invoke: Invoke) {
        onMain {
            // Persist desired state
            self.persistDesiredRunning(false)
            self.persistTaskCompletedAt()

            // Cancel any pending schedules for both task types
            self.scheduler.cancel(taskRequestWithIdentifier: self.refreshTaskId)
            self.scheduler.cancel(taskRequestWithIdentifier: self.processingTaskId)

            // Reject pending cancel invoke unconditionally (unblocks Rust thread)
            // This must happen even when no BGTask is active (foreground stop).
            if let cancelInvoke = self.pendingCancelInvoke {
                cancelInvoke.reject("cancelled")
                self.pendingCancelInvoke = nil
            }

            // Complete the active task exactly once
            self.completeActiveTask(success: false)

            // Clear remaining state
            self.cleanup()

            invoke.resolve()
        }
    }

    // MARK: - setDesiredRunning (H4 desired-state mirror)

    /// Mirror the Rust-authoritative desired state into iOS persistence (H4/D1).
    ///
    /// Called by Rust from the intent-only recovery commands
    /// (`enableAutoRestart`/`disableAutoRestart`/`setDesiredRunning`/
    /// `configureRecovery`) so they take a real, observable effect on iOS rather
    /// than silently no-op'ing: `desiredRunning` (+ optional `lastStartConfig`)
    /// is written to `UserDefaults`, and BGTasks are (re)scheduled when desired
    /// or cancelled when not. Unlike `startKeepalive`, this never starts the
    /// in-process service — it only sets recovery intent.
    @objc public func setDesiredRunning(_ invoke: Invoke) {
        onMain {
            let args = invoke.anyArgs
            let desired = (args?["desiredRunning"] as? Bool) ?? false

            self.persistDesiredRunning(desired)
            // `lastStartConfig` is the JSON-serialized StartConfig string the
            // Rust mirror passes; persist it verbatim so auto-start can parse it.
            if let config = args?["lastStartConfig"] as? String {
                UserDefaults.standard.set(config, forKey: DesiredStateKeys.lastStartConfig)
            }

            if desired {
                // Ensure the OS can relaunch us for background work even if the
                // service was never foreground-started this session.
                // M2: `scheduleNext` persists `lastScheduleError` itself.
                self.scheduleNext()
            } else {
                // No longer desired: cancel pending BGTask submissions.
                self.scheduler.cancel(taskRequestWithIdentifier: self.refreshTaskId)
                self.scheduler.cancel(taskRequestWithIdentifier: self.processingTaskId)
            }

            invoke.resolve()
        }
    }

    // MARK: - Status queries (C1 split: submit-result vs persisted desired state)

    /// Resolve the *submit-result* facts of the most recent scheduling attempt:
    /// `{refreshScheduled, processingScheduled, refreshError, processingError}`.
    /// Read from the snapshot `scheduleNext()` persists — never re-submits. Maps
    /// to the Rust `IOSSchedulingStatus` DTO. Defaults to "nothing scheduled"
    /// before any `startKeepalive`.
    @objc public func getSchedulingStatus(_ invoke: Invoke) {
        onMain {
            let defaults = UserDefaults.standard
            invoke.resolve([
                "refreshScheduled": defaults.bool(forKey: DesiredStateKeys.lastRefreshScheduled),
                "processingScheduled": defaults.bool(forKey: DesiredStateKeys.lastProcessingScheduled),
                "refreshError": defaults.string(forKey: DesiredStateKeys.lastRefreshError) ?? NSNull(),
                "processingError": defaults.string(forKey: DesiredStateKeys.lastProcessingError) ?? NSNull()
            ] as JsonObject)
        }
    }

    /// Resolve the *persisted desired-state* facts: `{desiredRunning,
    /// lastStartConfig, lastScheduleError, lastTaskKind, lastTaskStartedAt,
    /// lastTaskCompletedAt, lastCompletionReason, notificationGranted}`. Maps to the
    /// Rust `IOSDesiredStateStatus` DTO; the iOS auto-start reads `desiredRunning` +
    /// `lastStartConfig` from it. `lastCompletionReason` is the durable "why did
    /// the last run end?" fact (M7), distinct from the consumed adaptation
    /// outcome. `notificationGranted` forwards the deferred notification-authorization
    /// decision to Rust so the Notifier can degrade (M4); it is `NSNull()` until the
    /// first notification-requiring intent (service start) requests authorization.
    @objc public func getDesiredStateStatus(_ invoke: Invoke) {
        onMain {
            let defaults = UserDefaults.standard
            invoke.resolve([
                "desiredRunning": defaults.object(forKey: DesiredStateKeys.desiredRunning) as? Bool ?? false,
                "lastStartConfig": defaults.string(forKey: DesiredStateKeys.lastStartConfig) ?? NSNull(),
                "lastScheduleError": defaults.string(forKey: DesiredStateKeys.lastScheduleError) ?? NSNull(),
                "lastTaskKind": defaults.string(forKey: DesiredStateKeys.lastTaskKind) ?? NSNull(),
                "lastTaskStartedAt": defaults.object(forKey: DesiredStateKeys.lastTaskStartedAt) ?? NSNull(),
                "lastTaskCompletedAt": defaults.object(forKey: DesiredStateKeys.lastTaskCompletedAt) ?? NSNull(),
                "lastCompletionReason": defaults.string(forKey: DesiredStateKeys.lastCompletionReason) ?? NSNull(),
                "notificationGranted": defaults.object(forKey: DesiredStateKeys.notificationGranted) as? Bool ?? NSNull()
            ] as JsonObject)
        }
    }

    // MARK: - Pending BGTask Query (for Rust auto-start)

    /// Return the pending BGTask info that launched the app in the background.
    ///
    /// Called by Rust during iOS plugin setup to detect whether the app was
    /// launched by iOS for a background task. If a pending task exists and
    /// `desired_running` is true in UserDefaults, Rust auto-starts the service.
    ///
    /// Reads from UserDefaults as the source of truth so the info survives
    /// timing gaps between the BGTask handler and Rust setup.
    @objc public func getPendingBgTask(_ invoke: Invoke) {
        onMain {
            let defaults = UserDefaults.standard
            let kind = defaults.string(forKey: PendingTaskKeys.kind)
            let identifier = defaults.string(forKey: PendingTaskKeys.identifier)
            let receivedAt = defaults.object(forKey: PendingTaskKeys.receivedAt) as? TimeInterval
            let consumedAt = defaults.object(forKey: PendingTaskKeys.consumedAt) as? TimeInterval

            // H5/M14: a pending task is visible only while unconsumed
            // (`consumedAt == nil`). A consumed/stale record must not re-arm a
            // cold auto-start, so report "no pending task" once it's consumed.
            if let kind = kind, let identifier = identifier, consumedAt == nil {
                invoke.resolve([
                    "taskKind": kind,
                    "identifier": identifier,
                    "receivedAt": receivedAt ?? 0,
                    "consumedAt": NSNull()
                ] as JsonObject)
            } else {
                invoke.resolve([
                    "taskKind": NSNull(),
                    "identifier": NSNull(),
                    "receivedAt": NSNull(),
                    "consumedAt": NSNull()
                ] as JsonObject)
            }
        }
    }

    /// Clear the pending BGTask info by deleting **all** pending keys
    /// (`kind`/`identifier`/`receivedAt`/`consumedAt`) from UserDefaults and
    /// resetting the in-memory property. Stamping `consumedAt` alone left the
    /// other keys behind, so a stale record could re-arm a cold auto-start
    /// (H5); deleting every key guarantees a subsequent `getPendingBgTask`
    /// reports no pending task.
    @objc public func clearPendingBgTask(_ invoke: Invoke) {
        onMain {
            let defaults = UserDefaults.standard
            defaults.removeObject(forKey: PendingTaskKeys.kind)
            defaults.removeObject(forKey: PendingTaskKeys.identifier)
            defaults.removeObject(forKey: PendingTaskKeys.receivedAt)
            defaults.removeObject(forKey: PendingTaskKeys.consumedAt)
            self.pendingTaskInfo = nil
            invoke.resolve()
        }
    }

    /// Record a failure marker for the pending BGTask after a cold auto-start
    /// fails (H3).
    ///
    /// Crucially this does **not** clear any pending key — the pending record is
    /// the evidence that iOS launched us for a task, and a failed start must
    /// preserve it so `getPendingBgTask` still reports the task. It only stamps
    /// `lastFailedAt` so the failure is observable for diagnostics.
    @objc public func recordFailedPending(_ invoke: Invoke) {
        onMain {
            UserDefaults.standard.set(self.now(), forKey: PendingTaskKeys.lastFailedAt)
            invoke.resolve()
        }
    }

    // MARK: - Foreground/Background Transitions

    /// When the app transitions to background, ensure BGTasks are scheduled
    /// if desired_running is true and no BGTask is currently active.
    /// This covers the case where the user started the service in the foreground
    /// and then backgrounds the app — iOS needs scheduled BGTasks to potentially
    /// relaunch the app later.
    @objc func appDidEnterBackground() {
        let desired = UserDefaults.standard.bool(forKey: DesiredStateKeys.desiredRunning)
        if desired && !hasActiveTask {
            scheduleNext()
        }
    }

    /// On foreground transition, reconcile recovery state — mirroring
    /// `appDidEnterBackground` (L2). While the app is active the in-process service
    /// runs continuously, but if recovery is still desired and no BGTask is
    /// currently running, (re)schedule so iOS can relaunch us later. Any stale
    /// safety timer left by a suspended-then-expired run is cleared first.
    @objc func appWillEnterForeground() {
        let desired = UserDefaults.standard.bool(forKey: DesiredStateKeys.desiredRunning)
        guard !hasActiveTask else { return }
        // Clear stale refs/timer from a suspended-then-expired run.
        safetyTimer?.invalidate()
        safetyTimer = nil
        if desired {
            scheduleNext()
        }
    }

    // MARK: - Scheduling

    private let logger = Logger(subsystem: Bundle.main.bundleIdentifier ?? "app.tauri.backgroundservice", category: "BGTaskScheduler")

    @discardableResult
    private func scheduleNext() -> SchedulingResult {
        var refreshScheduled = false
        var refreshError: String?
        var processingScheduled = false
        var processingError: String?

        // L1: enforce the one-pending-request-per-identifier invariant — cancel the
        // prior pending request for each identifier before (re)submitting. iOS
        // already replaces a pending request when the same identifier is
        // resubmitted; cancelling first makes the invariant explicit and prevents
        // any backlog from accumulating across repeated reschedules.
        scheduler.cancel(taskRequestWithIdentifier: refreshTaskId)
        scheduler.cancel(taskRequestWithIdentifier: processingTaskId)

        // BGAppRefreshTask — runs opportunistically, ~30s budget
        let refreshReq = BGAppRefreshTaskRequest(identifier: refreshTaskId)
        refreshReq.earliestBeginDate = Date(timeIntervalSinceNow: earliestRefreshBeginMinutes * 60)
        do {
            try scheduler.submit(refreshReq)
            refreshScheduled = true
        } catch {
            refreshError = error.localizedDescription
            logger.error("Failed to submit BGAppRefreshTask '\(self.refreshTaskId)': \(error.localizedDescription)")
        }

        // BGProcessingTask — runs when device idle, minutes budget.
        // Its earliestBeginDate adapts to the outcome of the last run
        // (the refresh request above stays on the static config value).
        let defaults = UserDefaults.standard
        let previousAdaptive = defaults.object(
            forKey: DesiredStateKeys.adaptiveProcessingBeginMinutes
        ) as? Double ?? earliestProcessingBeginMinutes
        let adaptiveMinutes = Self.adaptiveProcessingBeginMinutes(
            configured: earliestProcessingBeginMinutes,
            ceilingMultiplier: processingCeilingMultiplier,
            lastStartedAt: (defaults.object(forKey: DesiredStateKeys.lastTaskStartedAt) as? TimeInterval)
                .map { Date(timeIntervalSince1970: $0) },
            lastCompletedAt: (defaults.object(forKey: DesiredStateKeys.lastTaskCompletedAt) as? TimeInterval)
                .map { Date(timeIntervalSince1970: $0) },
            lastTaskKind: defaults.string(forKey: DesiredStateKeys.lastTaskKind),
            lastOutcome: TaskOutcome(persisted: defaults.string(forKey: DesiredStateKeys.lastTaskOutcome)),
            previous: previousAdaptive
        )
        defaults.set(adaptiveMinutes, forKey: DesiredStateKeys.adaptiveProcessingBeginMinutes)
        // Consume the outcome: one observed run adapts the value exactly once.
        // Later scheduleNext() calls (foreground start, background transition)
        // see .unknown and hold.
        defaults.removeObject(forKey: DesiredStateKeys.lastTaskOutcome)

        let processingReq = BGProcessingTaskRequest(identifier: processingTaskId)
        processingReq.earliestBeginDate = Date(timeIntervalSinceNow: adaptiveMinutes * 60)
        processingReq.requiresExternalPower = requiresExternalPower
        processingReq.requiresNetworkConnectivity = requiresNetworkConnectivity
        do {
            try scheduler.submit(processingReq)
            processingScheduled = true
        } catch {
            processingError = error.localizedDescription
            logger.error("Failed to submit BGProcessingTask '\(self.processingTaskId)': \(error.localizedDescription)")
        }

        let result = SchedulingResult(
            refreshScheduled: refreshScheduled,
            processingScheduled: processingScheduled,
            refreshError: refreshError,
            processingError: processingError
        )
        // Snapshot the submit result so `getSchedulingStatus` can report it.
        persistSchedulingResult(result)
        // M2: `scheduleNext` is the single source of truth for the aggregate
        // `lastScheduleError`. Persisting it here (not only in `startKeepalive`)
        // makes background/expiration-driven reschedule failures visible and
        // clears a stale error on a later success. Per-task errors are tracked
        // independently above via `persistSchedulingResult`.
        persistScheduleError(result.refreshError ?? result.processingError)
        return result
    }
}

// Tauri iOS plugin entry point. The Rust side's
// `tauri::ios_plugin_binding!(init_plugin_background_service)` expands to a
// `swift_rs::swift!` extern over this `@_cdecl` symbol; without it the iOS link
// fails with `Undefined symbols: _init_plugin_background_service`. Mirrors the
// official `tauri-plugin-notification` registration pattern.
@_cdecl("init_plugin_background_service")
public func initPlugin() -> Plugin {
    return BackgroundServicePlugin()
}

/// Loosely-typed args accessor backporting the pre-2.9 `invoke.args(as: [String: Any].self)`
/// shape. tauri-api 2.9 replaced it with typed `parseArgs<T: Decodable>` / `getArgs() -> JSObject`,
/// but this plugin's command handlers (and `persistStartConfig([String: Any])`) read fields via
/// `as?` casts on a `[String: Any]` dict. Parsing `getRawArgs()` preserves that verbatim.
private extension Invoke {
    var anyArgs: [String: Any]? {
        let raw = getRawArgs()
        guard !raw.isEmpty,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return nil }
        return obj
    }
}
