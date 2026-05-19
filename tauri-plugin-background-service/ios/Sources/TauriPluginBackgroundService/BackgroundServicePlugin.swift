import UIKit
import BackgroundTasks
import UserNotifications
import WebKit
import os.log

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

 Include both `background-processing` and `background-fetch` modes:

 ```
 <key>UIBackgroundModes</key>
 <array>
     <string>background-processing</string>
     <string>background-fetch</string>
 </array>
 ```

 - `background-fetch` enables `BGAppRefreshTask` scheduling (~30s budget).
 - `background-processing` enables `BGProcessingTask` scheduling (minutes/hours,
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
@objc public class BackgroundServicePlugin: Plugin {

    // MARK: - Task Identifiers

    private var refreshTaskId: String {
        "\(Bundle.main.bundleIdentifier ?? "app").bg-refresh"
    }

    private var processingTaskId: String {
        "\(Bundle.main.bundleIdentifier ?? "app").bg-processing"
    }

    // MARK: - State for BGTask lifecycle management

    /// Currently active BGAppRefreshTask, if any.
    private var currentRefreshTask: BGAppRefreshTask?

    /// Currently active BGProcessingTask, if any.
    /// iOS guarantees at most one BGTask is active at a time, so only one of
    /// `currentRefreshTask` or `currentProcessingTask` will be non-nil.
    private var currentProcessingTask: BGProcessingTask?

    /// Pending cancel invoke — shared between both task types since iOS runs at most one.
    private var pendingCancelInvoke: Invoke?

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
    }

    // MARK: - Pending Task Keys

    /// UserDefaults keys for iOS pending BGTask persistence.
    /// Survives timing gaps between BGTask handler and Rust setup.
    private enum PendingTaskKeys {
        static let kind = "ios_pending_task_kind"
        static let identifier = "ios_pending_task_identifier"
        static let receivedAt = "ios_pending_task_received_at"
        static let consumedAt = "ios_pending_task_consumed_at"
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
        UserDefaults.standard.set(Date().timeIntervalSince1970, forKey: DesiredStateKeys.lastTaskStartedAt)
    }

    private func persistTaskCompletedAt() {
        UserDefaults.standard.set(Date().timeIntervalSince1970, forKey: DesiredStateKeys.lastTaskCompletedAt)
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

    public override func load(webView: WKWebView) {
        super.load(webView)

        // Request notification permission once.
        // After this, Rust's Notifier can post notifications freely.
        UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }

        // Register both BGTask handlers before the app finishes launching.
        let refreshId = refreshTaskId
        let processingId = processingTaskId

        BGTaskScheduler.shared.register(forTaskWithIdentifier: refreshId, using: .main) {
            [weak self] task in
            if let bgTask = task as? BGAppRefreshTask {
                self?.handleBackgroundTask(bgTask)
            } else {
                (task as? BGTask)?.setTaskCompleted(success: false)
            }
        }

        BGTaskScheduler.shared.register(forTaskWithIdentifier: processingId, using: .main) {
            [weak self] task in
            if let bgTask = task as? BGProcessingTask {
                self?.handleProcessingTask(bgTask)
            } else {
                (task as? BGTask)?.setTaskCompleted(success: false)
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
    }

    // MARK: - Completion Safety

    /// Safely complete the active BGTask exactly once.
    ///
    /// iOS requires `setTaskCompleted` to be called exactly once per BGTask.
    /// This method guards against double-completion by checking the `taskCompleted`
    /// flag before calling `setTaskCompleted`. The flag is reset in `cleanup()`
    /// and when a new BGTask handler fires.
    ///
    /// - Returns: `true` if a task was completed, `false` if already completed or no task.
    @discardableResult
    private func completeActiveTask(success: Bool) -> Bool {
        guard !taskCompleted else { return false }

        if let task = currentRefreshTask {
            taskCompleted = true
            currentRefreshTask = nil
            task.setTaskCompleted(success: success)
            return true
        } else if let task = currentProcessingTask {
            taskCompleted = true
            currentProcessingTask = nil
            task.setTaskCompleted(success: success)
            return true
        }
        return false
    }

    // MARK: - BGAppRefreshTask Handler

    private func handleBackgroundTask(_ task: BGAppRefreshTask) {
        self.currentRefreshTask = task
        self.taskCompleted = false

        let now = Date().timeIntervalSince1970

        // Store pending task info for Rust auto-start on BGTask launch.
        self.pendingTaskInfo = PendingTaskInfo(
            taskKind: "refresh",
            identifier: refreshTaskId,
            receivedAt: now
        )

        // Persist to UserDefaults so info survives timing gaps.
        persistPendingTaskInfo(kind: "refresh", identifier: refreshTaskId, receivedAt: now)

        persistTaskKind("refresh")
        persistTaskStartedAt()

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

        let now = Date().timeIntervalSince1970

        // Store pending task info for Rust auto-start on BGTask launch.
        self.pendingTaskInfo = PendingTaskInfo(
            taskKind: "processing",
            identifier: processingTaskId,
            receivedAt: now
        )

        // Persist to UserDefaults so info survives timing gaps.
        persistPendingTaskInfo(kind: "processing", identifier: processingTaskId, receivedAt: now)

        persistTaskKind("processing")
        persistTaskStartedAt()

        task.expirationHandler = { [weak self] in
            self?.handleExpiration()
        }

        // Only start safety timer for processing tasks if an explicit timeout was configured
        if let timeout = processingSafetyTimeoutSecs, timeout > 0 {
            startSafetyTimer(with: timeout)
        }
    }

    // MARK: - Expiration Handler (signals Rust to cancel)

    private func handleExpiration() {
        persistTaskCompletedAt()

        // Resolve pending cancel invoke (unblocks Rust thread)
        if let invoke = pendingCancelInvoke {
            invoke.resolve()
            pendingCancelInvoke = nil
        }

        // Complete the active task exactly once
        completeActiveTask(success: false)

        // Schedule next tasks
        scheduleNext()

        // Clear remaining state
        cleanup()
    }

    // MARK: - Safety Timer

    private func startSafetyTimer(with interval: TimeInterval) {
        safetyTimer?.invalidate()
        safetyTimer = Timer.scheduledTimer(withTimeInterval: interval, repeats: false) { [weak self] _ in
            self?.handleSafetyTimerExpiration()
        }
    }

    private func handleSafetyTimerExpiration() {
        persistTaskCompletedAt()

        // Force-complete task if Rust never called completeBgTask
        if currentRefreshTask != nil || currentProcessingTask != nil {
            // Resolve pending cancel invoke (unblocks Rust thread)
            if let invoke = pendingCancelInvoke {
                invoke.resolve()
                pendingCancelInvoke = nil
            }

            // Complete the active task exactly once
            completeActiveTask(success: false)

            // Schedule next tasks
            scheduleNext()

            // Clear remaining state
            cleanup()
        }
    }

    // MARK: - Cleanup

    private func cleanup() {
        currentRefreshTask = nil
        currentProcessingTask = nil
        pendingCancelInvoke = nil
        safetyTimer?.invalidate()
        safetyTimer = nil
        taskCompleted = false
    }

    // MARK: - waitForCancel (Pending Invoke pattern)

    @objc public func waitForCancel(_ invoke: Invoke) {
        // Always store invoke — it will be resolved by expiration/completion
        // or rejected by stopKeepalive, regardless of BGTask state.
        pendingCancelInvoke = invoke
    }

    // MARK: - cancelCancelListener (timeout unblock)

    /// Reject the pending cancel invoke to unblock the Rust `spawn_blocking` thread.
    ///
    /// Called from Rust when the cancel listener timeout fires (default: 4h).
    /// This ensures the `wait_for_cancel` thread does not leak indefinitely
    /// when iOS never resolves the invoke (e.g., app killed in background).
    @objc public func cancelCancelListener(_ invoke: Invoke) {
        if let cancelInvoke = pendingCancelInvoke {
            cancelInvoke.reject(error: nil)
            pendingCancelInvoke = nil
        }
        invoke.resolve()
    }

    // MARK: - completeBgTask (Rust→Swift completion signal)

    @objc public func completeBgTask(_ invoke: Invoke) {
        // Extract success value from invoke arguments
        let success = invoke.args(as: [String: Bool].self)?["success"] ?? true

        // Track whether we had an active BGTask before completion.
        // Prevents spurious rescheduling when completeBgTask is called
        // after expiration or explicit stop already cleaned up the task.
        let hadActiveTask = currentRefreshTask != nil || currentProcessingTask != nil

        // Complete the active task exactly once
        completeActiveTask(success: success)

        // Reject pending cancel invoke (unblocks Rust thread)
        if let cancelInvoke = pendingCancelInvoke {
            cancelInvoke.reject(error: nil)
            pendingCancelInvoke = nil
        }

        // Only reschedule if we actually completed a background task.
        // Avoids scheduling when called after expiration or stop already handled it.
        if hadActiveTask {
            scheduleNext()
        }

        // Clear remaining state
        cleanup()

        // Resolve this invoke
        invoke.resolve()
    }

    // MARK: - startKeepalive (configurable iOS safety timers)

    @objc public func startKeepalive(_ invoke: Invoke) {
        let args = invoke.args(as: [String: Any].self)
        if let args = args {
            // BGAppRefreshTask safety timeout (default: 28.0s via PluginConfig)
            if let timeout = args["iosSafetyTimeoutSecs"] as? Double {
                safetyTimeout = timeout
            }
            // BGProcessingTask safety timeout (default: nil = no cap)
            if let processingTimeout = args["iosProcessingSafetyTimeoutSecs"] as? Double {
                processingSafetyTimeoutSecs = processingTimeout
            }
            // BGAppRefreshTask earliest begin date in minutes
            if let minutes = args["iosEarliestRefreshBeginMinutes"] as? Double {
                earliestRefreshBeginMinutes = minutes
            }
            // BGProcessingTask earliest begin date in minutes
            if let minutes = args["iosEarliestProcessingBeginMinutes"] as? Double {
                earliestProcessingBeginMinutes = minutes
            }
            // BGProcessingTask requires external power
            if let power = args["iosRequiresExternalPower"] as? Bool {
                requiresExternalPower = power
            }
            // BGProcessingTask requires network connectivity
            if let network = args["iosRequiresNetworkConnectivity"] as? Bool {
                requiresNetworkConnectivity = network
            }
        }

        let result = scheduleNext()

        // Persist desired state
        persistDesiredRunning(true)
        if let args = args {
            persistStartConfig(args)
        }
        persistScheduleError(result.refreshError ?? result.processingError)
        UserDefaults.standard.removeObject(forKey: DesiredStateKeys.lastTaskCompletedAt)

        // If both scheduling attempts failed, reject with schedulerUnavailable
        if !result.refreshScheduled && !result.processingScheduled {
            invoke.reject(error: "schedulerUnavailable")
            return
        }

        // Return structured scheduling result
        invoke.resolve([
            "refreshScheduled": result.refreshScheduled,
            "processingScheduled": result.processingScheduled,
            "refreshError": result.refreshError ?? NSNull(),
            "processingError": result.processingError ?? NSNull()
        ] as [String: Any])
    }

    // MARK: - stopKeepalive (clean up active task)

    @objc public func stopKeepalive(_ invoke: Invoke) {
        // Persist desired state
        persistDesiredRunning(false)
        persistTaskCompletedAt()

        // Cancel any pending schedules for both task types
        BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: refreshTaskId)
        BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: processingTaskId)

        // Reject pending cancel invoke unconditionally (unblocks Rust thread)
        // This must happen even when no BGTask is active (foreground stop).
        if let cancelInvoke = pendingCancelInvoke {
            cancelInvoke.reject(error: nil)
            pendingCancelInvoke = nil
        }

        // Complete the active task exactly once
        completeActiveTask(success: false)

        // Clear remaining state
        cleanup()

        invoke.resolve()
    }

    // MARK: - getSchedulingStatus (query scheduling state from UserDefaults)

    @objc public func getSchedulingStatus(_ invoke: Invoke) {
        let defaults = UserDefaults.standard
        invoke.resolve([
            "desiredRunning": defaults.object(forKey: DesiredStateKeys.desiredRunning) as? Bool ?? false,
            "lastStartConfig": defaults.string(forKey: DesiredStateKeys.lastStartConfig) ?? NSNull(),
            "lastScheduleError": defaults.string(forKey: DesiredStateKeys.lastScheduleError) ?? NSNull(),
            "lastTaskKind": defaults.string(forKey: DesiredStateKeys.lastTaskKind) ?? NSNull(),
            "lastTaskStartedAt": defaults.object(forKey: DesiredStateKeys.lastTaskStartedAt) ?? NSNull(),
            "lastTaskCompletedAt": defaults.object(forKey: DesiredStateKeys.lastTaskCompletedAt) ?? NSNull()
        ] as [String: Any])
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
        let defaults = UserDefaults.standard
        let kind = defaults.string(forKey: PendingTaskKeys.kind)
        let identifier = defaults.string(forKey: PendingTaskKeys.identifier)
        let receivedAt = defaults.object(forKey: PendingTaskKeys.receivedAt) as? TimeInterval
        let consumedAt = defaults.object(forKey: PendingTaskKeys.consumedAt) as? TimeInterval

        if let kind = kind, let identifier = identifier {
            invoke.resolve([
                "taskKind": kind,
                "identifier": identifier,
                "receivedAt": receivedAt ?? 0,
                "consumedAt": consumedAt ?? NSNull()
            ] as [String: Any])
        } else {
            invoke.resolve([
                "taskKind": NSNull(),
                "identifier": NSNull(),
                "receivedAt": NSNull(),
                "consumedAt": NSNull()
            ] as [String: Any])
        }
    }

    /// Mark the pending BGTask info as consumed by setting the consumed_at
    /// timestamp in UserDefaults. The in-memory property is also cleared.
    @objc public func clearPendingBgTask(_ invoke: Invoke) {
        UserDefaults.standard.set(Date().timeIntervalSince1970, forKey: PendingTaskKeys.consumedAt)
        pendingTaskInfo = nil
        invoke.resolve()
    }

    // MARK: - Foreground/Background Transitions

    /// When the app transitions to background, ensure BGTasks are scheduled
    /// if desired_running is true and no BGTask is currently active.
    /// This covers the case where the user started the service in the foreground
    /// and then backgrounds the app — iOS needs scheduled BGTasks to potentially
    /// relaunch the app later.
    @objc private func appDidEnterBackground() {
        let desired = UserDefaults.standard.bool(forKey: DesiredStateKeys.desiredRunning)
        if desired && currentRefreshTask == nil && currentProcessingTask == nil {
            scheduleNext()
        }
    }

    /// No special action on foreground transition — the service keeps running.
    @objc private func appWillEnterForeground() {
        // Intentionally empty. Service runs continuously while app is active.
    }

    // MARK: - Scheduling

    private let logger = Logger(subsystem: Bundle.main.bundleIdentifier ?? "app.tauri.backgroundservice", category: "BGTaskScheduler")

    @discardableResult
    private func scheduleNext() -> SchedulingResult {
        var refreshScheduled = false
        var refreshError: String?
        var processingScheduled = false
        var processingError: String?

        // BGAppRefreshTask — runs opportunistically, ~30s budget
        let refreshReq = BGAppRefreshTaskRequest(identifier: refreshTaskId)
        refreshReq.earliestBeginDate = Date(timeIntervalSinceNow: earliestRefreshBeginMinutes * 60)
        do {
            try BGTaskScheduler.shared.submit(refreshReq)
            refreshScheduled = true
        } catch {
            refreshError = error.localizedDescription
            logger.error("Failed to submit BGAppRefreshTask '\(self.refreshTaskId)': \(error.localizedDescription)")
        }

        // BGProcessingTask — runs when device idle, minutes budget
        let processingReq = BGProcessingTaskRequest(identifier: processingTaskId)
        processingReq.earliestBeginDate = Date(timeIntervalSinceNow: earliestProcessingBeginMinutes * 60)
        processingReq.requiresExternalPower = requiresExternalPower
        processingReq.requiresNetworkConnectivity = requiresNetworkConnectivity
        do {
            try BGTaskScheduler.shared.submit(processingReq)
            processingScheduled = true
        } catch {
            processingError = error.localizedDescription
            logger.error("Failed to submit BGProcessingTask '\(self.processingTaskId)': \(error.localizedDescription)")
        }

        return SchedulingResult(
            refreshScheduled: refreshScheduled,
            processingScheduled: processingScheduled,
            refreshError: refreshError,
            processingError: processingError
        )
    }
}
