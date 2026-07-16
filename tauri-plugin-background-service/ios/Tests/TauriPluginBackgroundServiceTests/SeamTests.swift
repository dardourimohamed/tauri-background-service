import XCTest
import BackgroundTasks
@testable import tauri_plugin_background_service

/// Proves the four Wave-0 / H12 test seams are injectable and that the plugin uses the
/// injected fakes, while production defaults to the real implementations. These tests
/// characterize *current* plugin behavior through the seams — the behavior changes
/// themselves land in later steps (3, 6, 12, 15, 18), which assert through these seams.
final class SeamTests: XCTestCase {

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
            "ios_last_task_completed_at", "ios_adaptive_processing_begin_minutes",
            "ios_pending_task_consumed_at",
        ] {
            d.removeObject(forKey: key)
        }
    }

    // MARK: - BGTaskScheduler seam + Invoke capture

    func testStopKeepalive_cancelsBothViaSchedulerSeam_andResolvesInvoke() {
        let capture = InvokeCapture()
        plugin.stopKeepalive(capture.makeInvoke())

        XCTAssertEqual(scheduler.cancelled.count, 2, "stopKeepalive cancels both task ids via the seam")
        XCTAssertTrue(scheduler.cancelled.contains { $0.hasSuffix(".bg-refresh") })
        XCTAssertTrue(scheduler.cancelled.contains { $0.hasSuffix(".bg-processing") })
        XCTAssertEqual(capture.resolveCount, 1)
        XCTAssertEqual(capture.rejectCount, 0)
    }

    func testStartKeepalive_submitsBothViaSchedulerSeam_andResolves() {
        let capture = InvokeCapture()
        plugin.startKeepalive(capture.makeInvoke(args: "{}"))

        XCTAssertEqual(scheduler.submitted.count, 2, "startKeepalive submits refresh + processing via the seam")
        XCTAssertEqual(capture.resolveCount, 1)
        XCTAssertEqual(capture.rejectCount, 0)
        XCTAssertEqual(
            UserDefaults.standard.bool(forKey: "ios_desired_running"), true,
            "desired state is persisted on a successful start")
    }

    func testStartKeepalive_bothSubmitFail_rejectsSchedulerUnavailable() {
        scheduler.submitError = FakeSchedulerError()
        let capture = InvokeCapture()
        plugin.startKeepalive(capture.makeInvoke(args: "{}"))

        XCTAssertEqual(scheduler.submitted.count, 0, "no submit is recorded when the seam throws")
        XCTAssertEqual(capture.resolveCount, 0)
        XCTAssertEqual(capture.rejectCount, 1, "both-fail rejects instead of resolving")
        XCTAssertTrue(
            capture.rejectedPayload?.contains("schedulerUnavailable") ?? false,
            "reject carries schedulerUnavailable; got: \(capture.rejectedPayload ?? "nil")")
    }

    // MARK: - Clock seam

    func testStopKeepalive_stampsCompletedAtViaClockSeam() {
        // `clearPendingBgTask` no longer stamps a timestamp (Step 6/H5 deletes
        // the pending keys instead), so prove the clock seam through
        // `stopKeepalive`, which records `lastTaskCompletedAt` via `now()`.
        plugin.now = { 4242.0 }
        let capture = InvokeCapture()
        plugin.stopKeepalive(capture.makeInvoke())

        XCTAssertEqual(
            UserDefaults.standard.double(forKey: "ios_last_task_completed_at"), 4242.0,
            accuracy: 0.0001, "lastTaskCompletedAt comes from the injected clock, not Date()")
        XCTAssertEqual(capture.resolveCount, 1)
    }

    // MARK: - Fake BGTask + completion seam

    func testCompleteOnce_usesCompleteTaskSeam_exactlyOnce() {
        var recorded = 0
        plugin.completeTask = { _, _ in recorded += 1 }
        let fake = FakeBGTask()

        XCTAssertTrue(plugin.completeOnce(fake, success: true), "first completion runs")
        XCTAssertFalse(plugin.completeOnce(fake, success: true), "guard blocks the second")
        XCTAssertFalse(plugin.completeOnce(fake, success: false), "guard blocks the third")
        XCTAssertEqual(recorded, 1, "the completeTask seam is invoked exactly once")
    }

    func testFakeBGTask_recordsSetTaskCompletedThroughDefaultSeam() {
        // With the DEFAULT (real) completeTask seam, completeOnce forwards to the
        // injected task's setTaskCompleted — proving the production default is what
        // drives the FakeBGTask, and that it fires exactly once.
        let fake = FakeBGTask()

        XCTAssertTrue(plugin.completeOnce(fake, success: true))
        XCTAssertFalse(plugin.completeOnce(fake, success: false))
        XCTAssertEqual(fake.completionCount, 1, "setTaskCompleted called exactly once")
        XCTAssertEqual(fake.lastSuccess, true)
    }
}
