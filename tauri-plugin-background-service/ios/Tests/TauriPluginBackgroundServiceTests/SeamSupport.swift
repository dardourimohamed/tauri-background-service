import Foundation
import BackgroundTasks
import UserNotifications
import Tauri
@testable import tauri_plugin_background_service

// Test-only fakes for the four Wave-0 / H12 seams. These let XCTest drive plugin
// command logic on the Simulator without real BGTaskScheduler launches or wall-clock
// time. The plugin defaults each seam to its real implementation (see `Seams.swift` /
// `BackgroundServicePlugin`); these fakes are injected per-test.

/// Recording fake for the `BGTaskScheduling` seam: captures register/submit/cancel
/// and can simulate `submit` failure (`schedulerUnavailable`) or `register`
/// failure (IOS-SCHED-01).
final class FakeBGTaskScheduler: BGTaskScheduling {
    private(set) var registered: [String] = []
    private(set) var submitted: [BGTaskRequest] = []
    private(set) var cancelled: [String] = []
    /// When set, `submit` throws this instead of recording — simulates the real
    /// `BGTaskScheduler.submit(_:)` throw when the app isn't permitted to schedule.
    var submitError: Error?
    /// Optional per-request failure predicate for partial-failure tests: when set
    /// and it returns `true` for a request, `submit` throws instead of recording.
    /// Lets a test fail only one identifier (e.g. processing) to prove refresh vs
    /// processing scheduling errors are tracked independently (M2).
    var shouldFailSubmit: ((BGTaskRequest) -> Bool)?
    /// IOS-SCHED-01: default `register` return value. The real
    /// `BGTaskScheduler.register` returns false when the identifier is absent
    /// from `BGTaskSchedulerPermittedIdentifiers`; flip this to false to prove
    /// the plugin records the failure. Per-identifier overrides win over this
    /// default via `registerResults`.
    var registerResult: Bool = true
    /// IOS-SCHED-01: per-identifier `register` return overrides. Keys are task
    /// identifiers; absence falls back to `registerResult`.
    var registerResults: [String: Bool] = [:]

    /// Net pending requests per identifier. `submit` *appends* and `cancel` clears
    /// an identifier's entries — deliberately NOT the real iOS replace-on-submit
    /// dedup, so a test can prove the plugin's *explicit* cancel-before-submit
    /// (L1) rather than relying on iOS to dedup. With the cancel in place every
    /// identifier holds ≤1; without it, repeated `scheduleNext` accumulates >1.
    private(set) var pending: [String: [BGTaskRequest]] = [:]

    /// Count of pending requests per identifier (the L1 invariant under test).
    func pendingTaskRequests() -> [String: Int] {
        pending.mapValues { $0.count }
    }

    @discardableResult
    func register(
        forTaskWithIdentifier identifier: String,
        using queue: DispatchQueue?,
        launchHandler: @escaping (BGTask) -> Void
    ) -> Bool {
        registered.append(identifier)
        return registerResults[identifier] ?? registerResult
    }

    func submit(_ request: BGTaskRequest) throws {
        if let error = submitError { throw error }
        if shouldFailSubmit?(request) == true { throw FakeSchedulerError() }
        submitted.append(request)
        pending[request.identifier, default: []].append(request)
    }

    func cancel(taskRequestWithIdentifier identifier: String) {
        cancelled.append(identifier)
        pending[identifier] = []
    }
}

/// A stand-in scheduling error for the both-fail path.
struct FakeSchedulerError: Error {}

/// Fake for the `BGTaskCompleting` seam: records how many times `setTaskCompleted`
/// was called and the last success flag. Used to prove the exactly-once invariant.
final class FakeBGTask: BGTaskCompleting {
    private(set) var completionCount = 0
    private(set) var lastSuccess: Bool?

    func setTaskCompleted(success: Bool) {
        completionCount += 1
        lastSuccess = success
    }
}

/// Recording fake for the `NotificationAuthorizing` seam (M4): counts authorization
/// requests, records the options requested, and synchronously calls back with a
/// configurable `granted`/`error`. Lets the M4 tests prove the request is deferred
/// out of `load()`, fires at most once on the first notification-requiring intent,
/// and forwards `granted` — without a real system prompt.
final class FakeNotificationAuthorizer: NotificationAuthorizing {
    private(set) var requestCount = 0
    private(set) var lastOptions: UNAuthorizationOptions?
    /// Value handed to the completion handler as `granted` (default: granted).
    var grantResult = true
    /// Optional error handed to the completion handler alongside `granted`.
    var errorResult: Error?

    func requestAuthorization(
        options: UNAuthorizationOptions,
        completionHandler: @escaping (Bool, Error?) -> Void
    ) {
        requestCount += 1
        lastOptions = options
        completionHandler(grantResult, errorResult)
    }
}

/// Invoke capture seam: a genuine `Tauri.Invoke` whose response sink is captured, so a
/// test can read back what a plugin command resolved or rejected with. `Invoke` is
/// `public` (not `open`) in the Tauri module and so can't be subclassed across the
/// module boundary; instead this builds a real `Invoke` with a capturing `sendResponse`
/// and disambiguates resolve vs reject by the responding callback id.
final class InvokeCapture {
    private let callbackId: UInt64 = 1
    private let errorId: UInt64 = 2

    private(set) var resolveCount = 0
    private(set) var rejectCount = 0
    /// Serialized payload from the last `resolve(_:)` (nil for the no-arg `resolve()`).
    private(set) var resolvedPayload: String?
    /// Serialized payload from the last `reject(...)`.
    private(set) var rejectedPayload: String?
    /// Whether the most recent response (resolve or reject) was delivered on the
    /// main thread. Used by the H1 tests to prove `@objc` handler bodies are
    /// serialized onto `.main` even when the command is invoked off the main queue.
    private(set) var respondedOnMainThread: Bool?
    /// Fired on every response (resolve or reject). Lets a test await the handler
    /// body completing on `.main` after marshalling off a background queue.
    var onResponse: (() -> Void)?

    func makeInvoke(command: String = "test", args: String = "{}") -> Invoke {
        return Invoke(
            command: command,
            callback: callbackId,
            error: errorId,
            sendResponse: { [weak self] id, payload in
                guard let self = self else { return }
                self.respondedOnMainThread = Thread.isMainThread
                if id == self.callbackId {
                    self.resolveCount += 1
                    self.resolvedPayload = payload
                } else if id == self.errorId {
                    self.rejectCount += 1
                    self.rejectedPayload = payload
                }
                self.onResponse?()
            },
            sendChannelData: { _, _ in },
            data: args
        )
    }
}

// MARK: - FakeNotificationCenter (IOS-MSG-01)

/// Recording fake for the `NotificationCenterScheduling` seam (IOS-MSG-01):
/// captures the most recent `UNNotificationRequest` and registered categories,
/// and can simulate a scheduling error so the handler's resolve/reject path is
/// provable without a real system notification center.
final class FakeNotificationCenter: NotificationCenterScheduling {
    private(set) var addCount = 0
    private(set) var lastRequest: UNNotificationRequest?
    /// When set, `add` hands this error to the completion handler instead of
    /// scheduling — simulates a real `UNUserNotificationCenter.add` failure.
    var addError: Error?

    private(set) var setCategoriesCount = 0
    private(set) var lastCategories: Set<UNNotificationCategory>?

    func add(
        _ request: UNNotificationRequest,
        withCompletionHandler completionHandler: @escaping (Error?) -> Void
    ) {
        addCount += 1
        lastRequest = request
        completionHandler(addError)
    }

    func setNotificationCategories(_ categories: Set<UNNotificationCategory>) {
        setCategoriesCount += 1
        lastCategories = categories
    }
}

// MARK: - TestDefaults (IOS-CLEAN-01 centralization)

/// Canonical list of every `ios_*` UserDefaults key the plugin persists, plus
/// helpers to create an isolated suite and clear all keys. IOS-CLEAN-01
/// replaces the per-class `clearKeys()` helpers (each of which cleared only a
/// subset, leaking state between test classes) with this single source of
/// truth so test order no longer affects outcomes. Production reads/writes
/// through the plugin's `defaults` seam; tests inject an isolated suite built
/// here so they cannot leak into `UserDefaults.standard` either.
enum TestDefaults {
    /// The complete canonical list of `ios_*` UserDefaults keys the plugin
    /// persists (mirrors `BackgroundServicePlugin.DesiredStateKeys` +
    /// `PendingTaskKeys`, which are private). Add new keys here when you add
    /// one to the plugin so test cleanup stays total.
    static let allKeys: [String] = [
        // DesiredStateKeys
        "ios_desired_running",
        "ios_last_start_config",
        "ios_last_schedule_error",
        "ios_last_task_kind",
        "ios_last_task_started_at",
        "ios_last_task_completed_at",
        "ios_last_refresh_scheduled",
        "ios_last_processing_scheduled",
        "ios_last_refresh_error",
        "ios_last_processing_error",
        "ios_last_task_outcome",
        "ios_last_completion_reason",
        "ios_notification_granted",
        "ios_adaptive_processing_begin_minutes",
        // PendingTaskKeys
        "ios_pending_task_kind",
        "ios_pending_task_identifier",
        "ios_pending_task_received_at",
        "ios_pending_task_consumed_at",
        "ios_pending_task_last_failed_at",
    ]

    /// Remove every canonical key from the given defaults. Call this from
    /// each test class's `setUp` / `tearDown` once the suite is wired up.
    static func clearAll(on defaults: UserDefaults) {
        for key in allKeys {
            defaults.removeObject(forKey: key)
        }
    }

    /// Create a fresh, fully-cleared isolated `UserDefaults` suite so a test
    /// class's `plugin.defaults` cannot leak into `UserDefaults.standard` or
    /// another class's suite. Each call mints a unique suite name to keep
    /// concurrent / repeated runs isolated.
    @discardableResult
    static func makeIsolatedSuite(file: StaticString = #file, line: UInt = #line) -> UserDefaults {
        let name = "tauri-bg-svc.test.\(UUID().uuidString)"
        guard let suite = UserDefaults(suiteName: name) else {
            fatalError("UserDefaults(suiteName:) returned nil for \(name) at \(file):\(line)")
        }
        clearAll(on: suite)
        return suite
    }
}
