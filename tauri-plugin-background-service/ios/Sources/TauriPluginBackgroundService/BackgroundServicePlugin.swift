import UIKit
import BackgroundTasks
import UserNotifications
import WebKit

@objc public class BackgroundServicePlugin: Plugin {

    private var taskId: String {
        "\(Bundle.main.bundleIdentifier ?? "app").bg-refresh"
    }

    // MARK: - State for C1: BGTask lifecycle management
    private var currentTask: BGAppRefreshTask?
    private var pendingCancelInvoke: Invoke?
    private var safetyTimer: Timer?
    /// iOS safety timeout (default: 28.0s, Apple recommends keeping BG tasks under ~30s).
    /// Set via `startKeepalive` args from Rust (PluginConfig).
    private var safetyTimeout: TimeInterval = 28.0

    public override func load(webView: WKWebView) {
        super.load(webView)

        // Request notification permission once.
        // After this, Rust's Notifier can post notifications freely.
        UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }

        // Register background task handler before the app finishes launching.
        BGTaskScheduler.shared.register(forTaskWithIdentifier: taskId, using: .main) {
            [weak self] task in
            if let bgTask = task as? BGAppRefreshTask {
                self?.handleBackgroundTask(bgTask)
            } else {
                (task as? BGTask)?.setTaskCompleted(success: false)
            }
        }
    }

    // MARK: - BGTask Handler (C1: rewritten to not complete immediately)
    private func handleBackgroundTask(_ task: BGAppRefreshTask) {
        // Store task reference for later completion
        self.currentTask = task

        // Set expiration handler that signals cancellation to Rust
        task.expirationHandler = { [weak self] in
            self?.handleExpiration()
        }

        // Start 25-second safety timer (fallback if Rust panics)
        startSafetyTimer()

        // DO NOT call setTaskCompleted here — wait for Rust signal
    }

    // MARK: - Expiration Handler (C1: signals Rust to cancel)
    private func handleExpiration() {
        // Resolve pending cancel invoke (unblocks Rust thread)
        if let invoke = pendingCancelInvoke {
            invoke.resolve()
            pendingCancelInvoke = nil
        }

        // Complete task with failure
        currentTask?.setTaskCompleted(success: false)

        // Schedule next task
        scheduleNext()

        // Clear all state
        cleanup()
    }

    // MARK: - Safety Timer (C1: 25-second fallback)
    private func startSafetyTimer() {
        safetyTimer?.invalidate()
        safetyTimer = Timer.scheduledTimer(withTimeInterval: self.safetyTimeout, repeats: false) { [weak self] _ in
            self?.handleSafetyTimerExpiration()
        }
    }

    private func handleSafetyTimerExpiration() {
        // Force-complete task if Rust never called completeBgTask
        if currentTask != nil {
            // Reject pending cancel invoke (unblocks Rust thread)
            if let invoke = pendingCancelInvoke {
                invoke.reject(error: nil)
                pendingCancelInvoke = nil
            }

            // Complete task with failure
            currentTask?.setTaskCompleted(success: false)

            // Schedule next task
            scheduleNext()

            // Clear all state
            cleanup()
        }
    }

    // MARK: - Cleanup (C1: clear all state)
    private func cleanup() {
        currentTask = nil
        pendingCancelInvoke = nil
        safetyTimer?.invalidate()
        safetyTimer = nil
    }

    // MARK: - waitForCancel (C1: Pending Invoke pattern)
    @objc public func waitForCancel(_ invoke: Invoke) {
        // Always store invoke — it will be resolved by expiration/completion
        // or rejected by stopKeepalive, regardless of BGTask state.
        pendingCancelInvoke = invoke
    }

    // MARK: - completeBgTask (C1: Rust→Swift completion signal)
    @objc public func completeBgTask(_ invoke: Invoke) {
        // Extract success value from invoke arguments
        let success = invoke.args(as: [String: Bool].self)?["success"] ?? true

        // Complete the stored BGTask if still active
        if let task = currentTask {
            task.setTaskCompleted(success: success)
        }

        // Reject pending cancel invoke (unblocks Rust thread)
        if let cancelInvoke = pendingCancelInvoke {
            cancelInvoke.reject(error: nil)
            pendingCancelInvoke = nil
        }

        // Schedule next task
        scheduleNext()

        // Clear all state
        cleanup()

        // Resolve this invoke
        invoke.resolve()
    }

    // MARK: - startKeepalive (configurable iOS safety timer)
    @objc public func startKeepalive(_ invoke: Invoke) {
        // Read configurable timeout from Rust (default: 28.0s via PluginConfig)
        if let args = invoke.args(as: [String: Any].self),
           let timeout = args["iosSafetyTimeoutSecs"] as? Double {
            safetyTimeout = timeout
        }
        scheduleNext()
        invoke.resolve()
    }

    // MARK: - stopKeepalive (C1: modified to clean up active task)
    @objc public func stopKeepalive(_ invoke: Invoke) {
        // Cancel any pending schedule
        BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: taskId)

        // Reject pending cancel invoke unconditionally (unblocks Rust thread)
        // This must happen even when no BGTask is active (foreground stop).
        if let cancelInvoke = pendingCancelInvoke {
            cancelInvoke.reject(error: nil)
            pendingCancelInvoke = nil
        }

        // If a BGTask is active, complete it and clean up
        if currentTask != nil {
            // Complete active task with failure
            currentTask?.setTaskCompleted(success: false)

            // Clear all state
            cleanup()
        }

        // Safety timer cleanup
        safetyTimer?.invalidate()
        safetyTimer = nil

        invoke.resolve()
    }

    private func scheduleNext() {
        let req = BGAppRefreshTaskRequest(identifier: taskId)
        req.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
        try? BGTaskScheduler.shared.submit(req)
    }
}
