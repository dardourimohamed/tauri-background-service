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

    override func setUp() {
        super.setUp()
        plugin = BackgroundServicePlugin()
        scheduler = FakeBGTaskScheduler()
        plugin.scheduler = scheduler
        // `startKeepalive` now requests notification authorization (M4); inject the
        // fake so the real `UNUserNotificationCenter.current()` isn't touched in the
        // test host (which has no app bundle).
        plugin.notificationAuthorizer = FakeNotificationAuthorizer()
        clearKeys()
    }

    override func tearDown() {
        clearKeys()
        plugin = nil
        scheduler = nil
        super.tearDown()
    }

    private func clearKeys() {
        let d = UserDefaults.standard
        for key in [
            "ios_desired_running", "ios_last_schedule_error", "ios_last_start_config",
            "ios_last_task_kind", "ios_last_task_started_at", "ios_last_task_completed_at",
            "ios_last_refresh_scheduled", "ios_last_processing_scheduled",
            "ios_last_refresh_error", "ios_last_processing_error",
            "ios_adaptive_processing_begin_minutes", "ios_pending_task_consumed_at",
        ] {
            d.removeObject(forKey: key)
        }
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
        plugin.startKeepalive(InvokeCapture().makeInvoke(args: "{\"label\":\"Sila\"}"))

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
