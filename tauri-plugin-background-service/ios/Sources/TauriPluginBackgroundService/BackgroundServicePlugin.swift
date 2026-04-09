import UIKit
import BackgroundTasks
import UserNotifications
import WebKit

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
    }

    // MARK: - BGAppRefreshTask Handler

    private func handleBackgroundTask(_ task: BGAppRefreshTask) {
        self.currentRefreshTask = task

        task.expirationHandler = { [weak self] in
            self?.handleExpiration()
        }

        // Always start safety timer for refresh tasks (default: 28s)
        startSafetyTimer(with: safetyTimeout)
    }

    // MARK: - BGProcessingTask Handler

    private func handleProcessingTask(_ task: BGProcessingTask) {
        self.currentProcessingTask = task

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
        // Resolve pending cancel invoke (unblocks Rust thread)
        if let invoke = pendingCancelInvoke {
            invoke.resolve()
            pendingCancelInvoke = nil
        }

        // Complete whichever task is active — nil out BEFORE completing
        // to prevent double-completion if completeBgTask races in.
        if let task = currentRefreshTask {
            currentRefreshTask = nil
            task.setTaskCompleted(success: false)
        } else if let task = currentProcessingTask {
            currentProcessingTask = nil
            task.setTaskCompleted(success: false)
        }

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
        // Force-complete task if Rust never called completeBgTask
        if currentRefreshTask != nil || currentProcessingTask != nil {
            // Resolve pending cancel invoke (unblocks Rust thread)
            if let invoke = pendingCancelInvoke {
                invoke.resolve()
                pendingCancelInvoke = nil
            }

            // Complete whichever task is active — nil out BEFORE completing
            if let task = currentRefreshTask {
                currentRefreshTask = nil
                task.setTaskCompleted(success: false)
            } else if let task = currentProcessingTask {
                currentProcessingTask = nil
                task.setTaskCompleted(success: false)
            }

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
    }

    // MARK: - waitForCancel (Pending Invoke pattern)

    @objc public func waitForCancel(_ invoke: Invoke) {
        // Always store invoke — it will be resolved by expiration/completion
        // or rejected by stopKeepalive, regardless of BGTask state.
        pendingCancelInvoke = invoke
    }

    // MARK: - completeBgTask (Rust→Swift completion signal)

    @objc public func completeBgTask(_ invoke: Invoke) {
        // Extract success value from invoke arguments
        let success = invoke.args(as: [String: Bool].self)?["success"] ?? true

        // Track whether we had an active BGTask before nil-out.
        // Prevents spurious rescheduling when completeBgTask is called
        // after expiration or explicit stop already cleaned up the task.
        let hadActiveTask = currentRefreshTask != nil || currentProcessingTask != nil

        // Complete whichever task is active — nil out BEFORE completing
        // to prevent double-completion. At most one BGTask is active at a time.
        if let task = currentRefreshTask {
            currentRefreshTask = nil
            task.setTaskCompleted(success: success)
        } else if let task = currentProcessingTask {
            currentProcessingTask = nil
            task.setTaskCompleted(success: success)
        }

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
        if let args = invoke.args(as: [String: Any].self) {
            // BGAppRefreshTask safety timeout (default: 28.0s via PluginConfig)
            if let timeout = args["iosSafetyTimeoutSecs"] as? Double {
                safetyTimeout = timeout
            }
            // BGProcessingTask safety timeout (default: nil = no cap)
            if let processingTimeout = args["iosProcessingSafetyTimeoutSecs"] as? Double {
                processingSafetyTimeoutSecs = processingTimeout
            }
        }
        scheduleNext()
        invoke.resolve()
    }

    // MARK: - stopKeepalive (clean up active task)

    @objc public func stopKeepalive(_ invoke: Invoke) {
        // Cancel any pending schedules for both task types
        BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: refreshTaskId)
        BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: processingTaskId)

        // Reject pending cancel invoke unconditionally (unblocks Rust thread)
        // This must happen even when no BGTask is active (foreground stop).
        if let cancelInvoke = pendingCancelInvoke {
            cancelInvoke.reject(error: nil)
            pendingCancelInvoke = nil
        }

        // If a BGTask is active, nil out and complete it — prevents
        // completeBgTask from double-completing if it races in.
        if let task = currentRefreshTask {
            currentRefreshTask = nil
            task.setTaskCompleted(success: false)
        } else if let task = currentProcessingTask {
            currentProcessingTask = nil
            task.setTaskCompleted(success: false)
        }

        // Clear remaining state
        cleanup()

        invoke.resolve()
    }

    // MARK: - Scheduling

    private func scheduleNext() {
        // BGAppRefreshTask — runs opportunistically, ~30s budget
        let refreshReq = BGAppRefreshTaskRequest(identifier: refreshTaskId)
        refreshReq.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
        try? BGTaskScheduler.shared.submit(refreshReq)

        // BGProcessingTask — runs when device idle, minutes budget
        let processingReq = BGProcessingTaskRequest(identifier: processingTaskId)
        processingReq.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
        processingReq.requiresExternalPower = false
        processingReq.requiresNetworkConnectivity = false
        try? BGTaskScheduler.shared.submit(processingReq)
    }
}
