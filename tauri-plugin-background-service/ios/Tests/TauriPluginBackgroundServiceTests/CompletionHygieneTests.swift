import XCTest
import BackgroundTasks
@testable import tauri_plugin_background_service

/// Step 15 (Wave 4): completion/cancel/error/schedule hygiene (M1, M2, M3, L1,
/// L2, L3). These prove the Swift edge cases now that Step 3 serialized the
/// shared BGTask state on `.main`. XCTest runs on `.main`, so the plugin's
/// `onMain { ... }` bodies execute inline and the assertions are final
/// synchronously.
final class CompletionHygieneTests: XCTestCase {

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
            "ios_last_task_outcome", "ios_adaptive_processing_begin_minutes",
        ] {
            d.removeObject(forKey: key)
        }
    }

    // MARK: - M1: waitForCancel supersedes a stale pending invoke

    func testWaitForCancel_supersedesPreviousInvoke() {
        let first = InvokeCapture()
        plugin.waitForCancel(first.makeInvoke())

        let second = InvokeCapture()
        plugin.waitForCancel(second.makeInvoke())

        XCTAssertEqual(first.rejectCount, 1,
                       "a new cancel listener must reject the previously-held (stale) invoke")
        XCTAssertTrue(first.rejectedPayload?.contains("superseded") ?? false,
                      "the superseded invoke is rejected with 'superseded': \(first.rejectedPayload ?? "nil")")
        XCTAssertEqual(second.resolveCount, 0, "the newest invoke is held, not resolved")
        XCTAssertEqual(second.rejectCount, 0, "the newest invoke is held, not rejected")
    }

    // MARK: - M2: scheduleNext owns lastScheduleError (set then cleared)

    func testScheduleNext_setsThenClearsAggregateScheduleError_viaBackgroundReschedule() {
        UserDefaults.standard.set(true, forKey: "ios_desired_running")

        // A background-transition reschedule that fails must make the error visible.
        scheduler.submitError = FakeSchedulerError()
        plugin.appDidEnterBackground()
        XCTAssertNotNil(
            UserDefaults.standard.string(forKey: "ios_last_schedule_error"),
            "a failed background reschedule must persist lastScheduleError (single source = scheduleNext)")

        // A subsequent successful reschedule clears the stale error.
        scheduler.submitError = nil
        plugin.appDidEnterBackground()
        XCTAssertNil(
            UserDefaults.standard.string(forKey: "ios_last_schedule_error"),
            "a successful reschedule must clear the stale lastScheduleError")
    }

    func testScheduleNext_tracksRefreshAndProcessingErrorsIndependently() {
        UserDefaults.standard.set(true, forKey: "ios_desired_running")
        // Fail only the processing identifier; refresh still succeeds.
        scheduler.shouldFailSubmit = { $0.identifier.hasSuffix(".bg-processing") }

        plugin.appDidEnterBackground()

        let d = UserDefaults.standard
        XCTAssertNil(d.string(forKey: "ios_last_refresh_error"),
                     "refresh succeeded → no refresh error")
        XCTAssertNotNil(d.string(forKey: "ios_last_processing_error"),
                        "processing failed → processing error tracked separately")
        XCTAssertNotNil(d.string(forKey: "ios_last_schedule_error"),
                        "aggregate lastScheduleError reflects the processing failure")
    }

    // MARK: - M3: cleanup() does not reset the exactly-once completion flag

    func testCleanup_doesNotResetTaskCompletedFlag() {
        // First completion sets the flag (via the completeOnce seam).
        let fake = FakeBGTask()
        XCTAssertTrue(plugin.completeOnce(fake, success: true), "first completion runs")

        // cleanup() must NOT clear the flag — only a new BGTask handler does (M3).
        plugin.cleanup()

        // So a second completion of the same task is still blocked.
        XCTAssertFalse(plugin.completeOnce(fake, success: true),
                       "cleanup() must not reset taskCompleted — completion stays one-shot")
        XCTAssertEqual(fake.completionCount, 1, "setTaskCompleted still fired exactly once")
    }

    // MARK: - L1: one pending request per identifier across start/background combos

    func testScheduleNext_keepsAtMostOnePendingPerIdentifier() {
        UserDefaults.standard.set(true, forKey: "ios_desired_running")

        // Start, then a background transition, then another — each reschedules.
        plugin.startKeepalive(InvokeCapture().makeInvoke(args: "{}"))
        plugin.appDidEnterBackground()
        plugin.appDidEnterBackground()

        let pending = scheduler.pendingTaskRequests()
        for (identifier, count) in pending {
            XCTAssertLessThanOrEqual(count, 1,
                "at most one pending request per identifier (\(identifier) had \(count))")
        }
        // And both identifiers are actually pending (not zero).
        XCTAssertEqual(pending.values.filter { $0 == 1 }.count, 2,
                       "both refresh and processing have exactly one pending request: \(pending)")
    }

    // MARK: - L2: appWillEnterForeground reconciles like appDidEnterBackground

    func testForeground_reschedulesWhenDesiredAndNoActiveTask() {
        UserDefaults.standard.set(true, forKey: "ios_desired_running")

        plugin.appWillEnterForeground()

        XCTAssertEqual(scheduler.submitted.count, 2,
                       "foreground with desired & no active task must (re)schedule both BGTasks")
    }

    func testForeground_doesNotRescheduleWhenNotDesired() {
        UserDefaults.standard.set(false, forKey: "ios_desired_running")

        plugin.appWillEnterForeground()

        XCTAssertEqual(scheduler.submitted.count, 0,
                       "foreground must not schedule when recovery is not desired")
    }

    // MARK: - L3: expiration with no active task produces no submit

    func testHandleExpiration_withNoActiveTask_doesNotSubmit() {
        // No BGTask handler has run, so there is no active task.
        plugin.handleExpiration()

        XCTAssertTrue(scheduler.submitted.isEmpty,
                      "a stray expiration with no active task must not reschedule (L3 guard)")
        let pending = scheduler.pendingTaskRequests()
        XCTAssertTrue(pending.values.allSatisfy { $0 == 0 } || pending.isEmpty,
                      "no pending requests created by a no-op expiration: \(pending)")
    }
}
