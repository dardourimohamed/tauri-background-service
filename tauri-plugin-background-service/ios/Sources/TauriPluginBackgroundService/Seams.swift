// Copyright The Tauri Background Service Plugin Authors.
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

import Foundation
import BackgroundTasks
import UserNotifications

/// Test seams for the iOS background-service plugin (Wave 0 / H12).
///
/// These protocols abstract the real iOS services the plugin drives so XCTest can
/// inject fakes on the Simulator without depending on actual `BGTaskScheduler`
/// background launches. The plugin defaults every seam to the real implementation
/// (see `BackgroundServicePlugin.scheduler` / `now` / `completeTask`); tests in the
/// `tauri-plugin-background-serviceTests` target override them. Later steps (3, 6,
/// 12, 15, 18) assert plugin behavior through these seams.

// MARK: - BGTaskScheduler seam

/// The subset of `BGTaskScheduler` the plugin uses: register, submit, cancel.
/// Production wires this to `SystemBGTaskScheduler`; XCTest injects a recording fake
/// so scheduling logic is provable without real background-task services.
protocol BGTaskScheduling: AnyObject {
    @discardableResult
    func register(
        forTaskWithIdentifier identifier: String,
        using queue: DispatchQueue?,
        launchHandler: @escaping (BGTask) -> Void
    ) -> Bool
    func submit(_ request: BGTaskRequest) throws
    func cancel(taskRequestWithIdentifier identifier: String)
}

/// Real seam implementation forwarding to `BGTaskScheduler.shared` — the production
/// default. Behavior is identical to calling `BGTaskScheduler.shared` directly.
final class SystemBGTaskScheduler: BGTaskScheduling {
    @discardableResult
    func register(
        forTaskWithIdentifier identifier: String,
        using queue: DispatchQueue?,
        launchHandler: @escaping (BGTask) -> Void
    ) -> Bool {
        BGTaskScheduler.shared.register(
            forTaskWithIdentifier: identifier, using: queue, launchHandler: launchHandler)
    }

    func submit(_ request: BGTaskRequest) throws {
        try BGTaskScheduler.shared.submit(request)
    }

    func cancel(taskRequestWithIdentifier identifier: String) {
        BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: identifier)
    }
}

// MARK: - BGTask completion seam

/// The one `BGTask` capability the plugin's terminal completion path needs:
/// `setTaskCompleted(success:)`. `BGAppRefreshTask`/`BGProcessingTask` (both `BGTask`
/// subclasses) already implement it — the extension below just declares conformance,
/// so the real tasks satisfy the seam with zero behavior change. XCTest injects a
/// `FakeBGTask` recording the call count to prove the exactly-once invariant
/// (locked in by Step 18 / I4).
protocol BGTaskCompleting: AnyObject {
    func setTaskCompleted(success: Bool)
}

extension BGTask: BGTaskCompleting {}

// MARK: - Notification authorization seam (M4)

/// The single `UNUserNotificationCenter` capability the plugin needs: request
/// notification authorization for a set of types and report back `granted`.
/// Production wires this to `SystemNotificationAuthorizer`; XCTest injects a
/// recording fake so M4 can prove authorization is deferred out of `load()`,
/// fires at most once on the first notification-requiring intent, and forwards
/// `granted` — all without triggering a real system permission prompt.
protocol NotificationAuthorizing: AnyObject {
    func requestAuthorization(
        options: UNAuthorizationOptions,
        completionHandler: @escaping (Bool, Error?) -> Void
    )
}

/// Real seam implementation forwarding to `UNUserNotificationCenter.current()` —
/// the production default. Behavior is identical to calling it directly.
final class SystemNotificationAuthorizer: NotificationAuthorizing {
    func requestAuthorization(
        options: UNAuthorizationOptions,
        completionHandler: @escaping (Bool, Error?) -> Void
    ) {
        UNUserNotificationCenter.current()
            .requestAuthorization(options: options, completionHandler: completionHandler)
    }
}

// MARK: - Notification-center scheduling seam (IOS-MSG-01)

/// The `UNUserNotificationCenter` capabilities the plugin's message-notification
/// path needs: `add(_:)` to schedule a `UNNotificationRequest` and
/// `setNotificationCategories(_:)` to register reply / mark-read actions.
/// Production wires this to `SystemNotificationCenter`; XCTest injects a
/// recording fake so IOS-MSG-01 can prove a request with a stable identifier,
/// metadata/deep-link `userInfo`, and a registered category is produced
/// without a real system notification center.
protocol NotificationCenterScheduling: AnyObject {
    /// Schedule a notification request. The completion fires with an error if
    /// scheduling failed (e.g. the request identifier is malformed).
    func add(
        _ request: UNNotificationRequest,
        withCompletionHandler completionHandler: @escaping (Error?) -> Void
    )

    /// Register the set of notification categories the app surfaces.
    func setNotificationCategories(_ categories: Set<UNNotificationCategory>)
}

/// Real seam implementation forwarding to `UNUserNotificationCenter.current()` —
/// the production default. Behavior is identical to calling it directly.
final class SystemNotificationCenter: NotificationCenterScheduling {
    func add(
        _ request: UNNotificationRequest,
        withCompletionHandler completionHandler: @escaping (Error?) -> Void
    ) {
        UNUserNotificationCenter.current().add(request, withCompletionHandler: completionHandler)
    }

    func setNotificationCategories(_ categories: Set<UNNotificationCategory>) {
        UNUserNotificationCenter.current().setNotificationCategories(categories)
    }
}
