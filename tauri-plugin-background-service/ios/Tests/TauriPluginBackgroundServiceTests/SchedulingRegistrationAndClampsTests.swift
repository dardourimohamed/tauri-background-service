import XCTest
import WebKit
import BackgroundTasks
@testable import tauri_plugin_background_service

/// IOS-SCHED-01: BGTaskScheduler.register Bool results are no longer swallowed
/// (registration failure is observable in the persisted status and logged),
/// and the Rust-configured numeric seams are defensively clamped at the Swift
/// boundary so bad config cannot reach BGTaskScheduler / Timer.
final class SchedulingRegistrationAndClampsTests: XCTestCase {

    private var plugin: BackgroundServicePlugin!
    private var scheduler: FakeBGTaskScheduler!
    private var suite: UserDefaults!

    override func setUp() {
        super.setUp()
        plugin = BackgroundServicePlugin()
        scheduler = FakeBGTaskScheduler()
        plugin.scheduler = scheduler
        plugin.notificationAuthorizer = FakeNotificationAuthorizer()
        suite = TestDefaults.makeIsolatedSuite()
        plugin.defaults = suite
        TestDefaults.clearAll(on: suite)
    }

    override func tearDown() {
        TestDefaults.clearAll(on: suite)
        plugin = nil
        scheduler = nil
        suite = nil
        super.tearDown()
    }

    // MARK: - registration failure is observable

    /// A refresh-identifier registration failure must be persisted into the
    /// aggregate `lastScheduleError` so `getDesiredStateStatus` surfaces it
    /// (Rust reads this through the handler). Pre-fix the Bool was discarded
    /// and the host saw only a silent scheduling failure later.
    func testLoad_refreshRegistrationFailure_persistsScheduleError() {
        let bid = Bundle.main.bundleIdentifier ?? "app"
        scheduler.registerResults = [
            "\(bid).bg-refresh": false,
            "\(bid).bg-processing": true,
        ]

        plugin.load(webview: WKWebView())

        let error = suite.string(forKey: "ios_last_schedule_error")
        XCTAssertNotNil(error, "refresh register=false must persist lastScheduleError")
        XCTAssertTrue(error?.contains("register failed") ?? false,
                      "error names the failed registration: \(error ?? "nil")")
    }

    func testLoad_processingRegistrationFailure_persistsScheduleError() {
        let bid = Bundle.main.bundleIdentifier ?? "app"
        scheduler.registerResults = [
            "\(bid).bg-refresh": true,
            "\(bid).bg-processing": false,
        ]

        plugin.load(webview: WKWebView())

        let error = suite.string(forKey: "ios_last_schedule_error")
        XCTAssertNotNil(error, "processing register=false must persist lastScheduleError")
        XCTAssertTrue(error?.contains("register failed") ?? false)
    }

    func testLoad_successfulRegistration_clearsNoError() {
        // Both registrations succeed → no error persisted.
        plugin.load(webview: WKWebView())

        XCTAssertNil(suite.string(forKey: "ios_last_schedule_error"),
                     "successful registration writes no error")
    }

    // MARK: - numeric clamps (pure static helpers)

    func testClampPositiveTimeout_validValuePassesThrough() {
        XCTAssertEqual(BackgroundServicePlugin.clampPositiveTimeout(30.0, fallback: 28.0), 30.0)
    }

    func testClampPositiveTimeout_zeroFallsBack() {
        XCTAssertEqual(BackgroundServicePlugin.clampPositiveTimeout(0.0, fallback: 28.0), 28.0)
    }

    func testClampPositiveTimeout_negativeFallsBack() {
        XCTAssertEqual(BackgroundServicePlugin.clampPositiveTimeout(-5.0, fallback: 28.0), 28.0)
    }

    func testClampPositiveTimeout_nanFallsBack() {
        XCTAssertEqual(BackgroundServicePlugin.clampPositiveTimeout(.nan, fallback: 28.0), 28.0)
    }

    func testClampPositiveTimeout_infiniteFallsBack() {
        XCTAssertEqual(
            BackgroundServicePlugin.clampPositiveTimeout(.infinity, fallback: 28.0), 28.0)
    }

    func testClampNonNegativeMinutes_validValuePassesThrough() {
        XCTAssertEqual(BackgroundServicePlugin.clampNonNegativeMinutes(15.0), 15.0)
    }

    func testClampNonNegativeMinutes_zeroStaysZero() {
        // 0 is the schedule-immediately value — valid for earliestBeginDate.
        XCTAssertEqual(BackgroundServicePlugin.clampNonNegativeMinutes(0.0), 0.0)
    }

    func testClampNonNegativeMinutes_negativeClampsToZero() {
        XCTAssertEqual(BackgroundServicePlugin.clampNonNegativeMinutes(-10.0), 0.0,
                       "negative earliestBeginDate would be rejected by BGTaskScheduler.submit")
    }

    func testClampNonNegativeMinutes_nanClampsToZero() {
        XCTAssertEqual(BackgroundServicePlugin.clampNonNegativeMinutes(.nan), 0.0)
    }

    func testClampMinimumMultiplier_validValuePassesThrough() {
        XCTAssertEqual(BackgroundServicePlugin.clampMinimumMultiplier(4.0), 4.0)
    }

    func testClampMinimumMultiplier_exactlyOnePasses() {
        XCTAssertEqual(BackgroundServicePlugin.clampMinimumMultiplier(1.0), 1.0)
    }

    func testClampMinimumMultiplier_subOneClampsToOne() {
        // Sub-1 would collapse the adaptive ceiling below the floor.
        XCTAssertEqual(BackgroundServicePlugin.clampMinimumMultiplier(0.5), 1.0)
    }

    func testClampMinimumMultiplier_negativeClampsToOne() {
        XCTAssertEqual(BackgroundServicePlugin.clampMinimumMultiplier(-3.0), 1.0)
    }

    func testClampMinimumMultiplier_nanClampsToOne() {
        XCTAssertEqual(BackgroundServicePlugin.clampMinimumMultiplier(.nan), 1.0)
    }

    // MARK: - clamps applied in startKeepalive (observable via submitted request)

    /// A negative `iosEarliestRefreshBeginMinutes` must be clamped to 0 so the
    /// submitted `BGAppRefreshTaskRequest.earliestBeginDate` is not in the past
    /// (which `BGTaskScheduler.submit` rejects). Observed through the fake
    /// scheduler's recorded request.
    func testStartKeepalive_clampsNegativeEarliestRefreshBeginMinutes() {
        let capture = InvokeCapture()
        plugin.startKeepalive(capture.makeInvoke(
            args: "{\"iosEarliestRefreshBeginMinutes\":-10.0}"))

        XCTAssertEqual(capture.resolveCount, 1)
        let refreshReq = scheduler.submitted.first { $0.identifier.hasSuffix(".bg-refresh") }
        XCTAssertNotNil(refreshReq?.earliestBeginDate, "refresh request carries an earliestBeginDate")
        // Clamped to 0 → earliestBeginDate ≈ now (allow scheduling slack).
        let now = Date()
        let drift = refreshReq!.earliestBeginDate!.timeIntervalSince(now)
        XCTAssertGreaterThanOrEqual(drift, -1.0,
                                    "earliestBeginDate is not in the deep past (negative clamped to 0): drift=\(drift)")
        XCTAssertLessThanOrEqual(drift, 60.0,
                                 "earliestBeginDate is essentially now (0 minutes), not -10*60s in the past")
    }

    /// A non-finite multiplier must be clamped to 1 so it does not poison the
    /// persisted adaptive value. The static clamp is already covered above; this
    /// proves the plugin wires `startKeepalive` through it (observable via the
    /// processing request's earliestBeginDate not being NaN/∞).
    func testStartKeepalive_clampsNonFiniteMultiplierSoProcessingScheduleIsFinite() {
        let capture = InvokeCapture()
        plugin.startKeepalive(capture.makeInvoke(
            args: "{\"iosProcessingCeilingMultiplier\":1.7976931348623157e308}"))

        XCTAssertEqual(capture.resolveCount, 1)
        let processingReq = scheduler.submitted.first { $0.identifier.hasSuffix(".bg-processing") }
        XCTAssertNotNil(processingReq?.earliestBeginDate)
        XCTAssertTrue(processingReq!.earliestBeginDate!.isFinite,
                      "processing earliestBeginDate must be finite (multiplier clamped)")
    }
}

