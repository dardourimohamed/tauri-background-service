import XCTest
import BackgroundTasks
@testable import tauri_plugin_background_service

/// C1/M9: proves the iOS status-query split resolves the two distinct payload
/// shapes the Rust DTOs expect — `getSchedulingStatus` -> submit-result
/// (`IOSSchedulingStatus`), `getDesiredStateStatus` -> persisted desired state
/// (`IOSDesiredStateStatus`). Before this step a single `getSchedulingStatus`
/// resolved the desired-state shape, so the typed Rust path never returned `Ok`.
final class SchedulingStatusContractTests: XCTestCase {

    private var plugin: BackgroundServicePlugin!
    private var scheduler: FakeBGTaskScheduler!
    private var suite: UserDefaults!

    override func setUp() {
        super.setUp()
        plugin = BackgroundServicePlugin()
        scheduler = FakeBGTaskScheduler()
        plugin.scheduler = scheduler
        // `startKeepalive` now requests notification authorization (M4); inject the
        // fake so the real `UNUserNotificationCenter.current()` isn't touched in the
        // test host (which has no app bundle).
        plugin.notificationAuthorizer = FakeNotificationAuthorizer()
        // IOS-CLEAN-01: isolated suite so this class cannot leak state.
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

    // MARK: - getSchedulingStatus = submit-result shape (IOSSchedulingStatus)

    func testGetSchedulingStatus_resolvesSubmitResultShape_afterStart() {
        // Start schedules both task types via the fake scheduler (no errors).
        plugin.startKeepalive(InvokeCapture().makeInvoke(args: "{}"))

        let capture = InvokeCapture()
        plugin.getSchedulingStatus(capture.makeInvoke())

        XCTAssertEqual(capture.resolveCount, 1)
        let payload = capture.resolvedPayload ?? ""
        // Submit-result keys present (the four IOSSchedulingStatus fields)…
        XCTAssertTrue(payload.contains("refreshScheduled"), "missing refreshScheduled: \(payload)")
        XCTAssertTrue(payload.contains("processingScheduled"), "missing processingScheduled: \(payload)")
        // …and the desired-state keys are absent — that's the other DTO now.
        XCTAssertFalse(payload.contains("desiredRunning"), "submit-result must not carry desiredRunning: \(payload)")
        XCTAssertFalse(payload.contains("lastStartConfig"), "submit-result must not carry lastStartConfig: \(payload)")
    }

    func testGetSchedulingStatus_freshLaunch_reportsNothingScheduled() {
        // No startKeepalive yet: the submit-result snapshot defaults to false,
        // which is a valid IOSSchedulingStatus (required booleans present).
        let capture = InvokeCapture()
        plugin.getSchedulingStatus(capture.makeInvoke())

        XCTAssertEqual(capture.resolveCount, 1)
        let payload = capture.resolvedPayload ?? ""
        XCTAssertTrue(payload.contains("\"refreshScheduled\":false"), payload)
        XCTAssertTrue(payload.contains("\"processingScheduled\":false"), payload)
    }

    // MARK: - getDesiredStateStatus = persisted shape (IOSDesiredStateStatus)

    func testGetDesiredStateStatus_resolvesPersistedShape_afterStart() {
        plugin.startKeepalive(InvokeCapture().makeInvoke(args: "{\"label\":\"App\"}"))

        let capture = InvokeCapture()
        plugin.getDesiredStateStatus(capture.makeInvoke())

        XCTAssertEqual(capture.resolveCount, 1)
        let payload = capture.resolvedPayload ?? ""
        // Desired-state keys present…
        XCTAssertTrue(payload.contains("\"desiredRunning\":true"), payload)
        XCTAssertTrue(payload.contains("lastStartConfig"), payload)
        // …and the submit-result keys are absent.
        XCTAssertFalse(payload.contains("refreshScheduled"), "desired-state must not carry refreshScheduled: \(payload)")
    }
}
