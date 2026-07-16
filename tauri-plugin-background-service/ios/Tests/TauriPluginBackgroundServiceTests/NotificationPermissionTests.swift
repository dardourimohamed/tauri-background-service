import XCTest
import WebKit
import BackgroundTasks
@testable import tauri_plugin_background_service

/// Step 17 (Wave 4): defer notification permission to intent (M4). Proves the
/// plugin no longer prompts for notification authorization at `load()`, requests it
/// at most once on the first notification-requiring intent (service start), narrows
/// the requested types to what the lifecycle Notifier actually posts, forwards
/// `granted` into the Rust-readable durable store, and never couples background
/// service start to the notification decision.
///
/// All authorization goes through the injected `FakeNotificationAuthorizer` seam, so
/// no real system permission prompt is triggered. XCTest runs on `.main`, so the
/// plugin's `onMain { ... }` bodies execute inline and the fake's synchronous
/// completion handler resolves before each assertion.
final class NotificationPermissionTests: XCTestCase {

    private var plugin: BackgroundServicePlugin!
    private var scheduler: FakeBGTaskScheduler!
    private var authorizer: FakeNotificationAuthorizer!

    private let grantedKey = "ios_notification_granted"

    override func setUp() {
        super.setUp()
        plugin = BackgroundServicePlugin()
        scheduler = FakeBGTaskScheduler()
        authorizer = FakeNotificationAuthorizer()
        plugin.scheduler = scheduler
        plugin.notificationAuthorizer = authorizer
        clearKeys()
    }

    override func tearDown() {
        clearKeys()
        plugin = nil
        scheduler = nil
        authorizer = nil
        super.tearDown()
    }

    private func clearKeys() {
        let d = UserDefaults.standard
        for key in [
            "ios_desired_running", "ios_last_schedule_error", "ios_last_start_config",
            "ios_last_task_completed_at", "ios_notification_granted",
        ] {
            d.removeObject(forKey: key)
        }
    }

    // MARK: - M4: no prompt at load()

    func testLoad_doesNotRequestNotificationAuthorization() {
        plugin.load(webview: WKWebView())

        XCTAssertEqual(authorizer.requestCount, 0,
                       "load() must NOT request notification authorization (deferred to intent, M4)")
        XCTAssertNil(UserDefaults.standard.object(forKey: grantedKey),
                     "no granted fact is persisted before the user expresses intent")
    }

    // MARK: - M4: request at the first notification-requiring intent (service start)

    func testStartKeepalive_requestsNotificationAuthorizationAtMostOnce() {
        plugin.startKeepalive(InvokeCapture().makeInvoke(args: "{}"))
        XCTAssertEqual(authorizer.requestCount, 1,
                       "service start is the first notification-requiring intent — request fires once")

        // A second start within the process must not re-prompt.
        plugin.startKeepalive(InvokeCapture().makeInvoke(args: "{}"))
        XCTAssertEqual(authorizer.requestCount, 1,
                       "authorization is requested at most once per process (M4)")
    }

    // MARK: - M4: request only the types the Notifier needs

    func testNotificationAuthorization_requestsOnlyNeededTypes() {
        plugin.startKeepalive(InvokeCapture().makeInvoke(args: "{}"))

        let options = authorizer.lastOptions ?? []
        XCTAssertTrue(options.contains(.alert), "alerts are needed for lifecycle notifications")
        XCTAssertTrue(options.contains(.sound), "sound is needed for lifecycle notifications")
        XCTAssertFalse(options.contains(.badge),
                       "badge is not posted by the lifecycle Notifier — don't over-request (M4)")
    }

    // MARK: - M4: granted is forwarded to Rust (durable, surfaced by getDesiredStateStatus)

    func testStartKeepalive_forwardsGrantedTrueToRust() {
        authorizer.grantResult = true
        plugin.startKeepalive(InvokeCapture().makeInvoke(args: "{}"))

        XCTAssertEqual(UserDefaults.standard.object(forKey: grantedKey) as? Bool, true,
                       "granted is persisted into the Rust-readable durable store")

        let capture = InvokeCapture()
        plugin.getDesiredStateStatus(capture.makeInvoke())
        let payload = capture.resolvedPayload ?? ""
        XCTAssertTrue(payload.contains("\"notificationGranted\":true"),
                      "getDesiredStateStatus forwards granted to Rust: \(payload)")
    }

    func testStartKeepalive_forwardsGrantedFalseToRust() {
        authorizer.grantResult = false
        plugin.startKeepalive(InvokeCapture().makeInvoke(args: "{}"))

        XCTAssertEqual(UserDefaults.standard.object(forKey: grantedKey) as? Bool, false,
                       "a denied decision is forwarded too, so the Notifier can degrade")
    }

    // MARK: - M4: denied notification permission does NOT block service start

    func testServiceStart_isIndependentOfNotificationDenial() {
        authorizer.grantResult = false
        let capture = InvokeCapture()
        plugin.startKeepalive(capture.makeInvoke(args: "{}"))

        XCTAssertEqual(capture.resolveCount, 1,
                       "background service start succeeds even when notification permission is denied")
        XCTAssertEqual(capture.rejectCount, 0)
        XCTAssertEqual(scheduler.submitted.count, 2,
                       "both BGTasks are still scheduled regardless of the notification decision")
    }
}
