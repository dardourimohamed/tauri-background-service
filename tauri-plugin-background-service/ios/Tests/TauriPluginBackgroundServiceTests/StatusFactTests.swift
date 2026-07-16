import XCTest
import BackgroundTasks
@testable import tauri_plugin_background_service

/// Step 16 (Wave 4): structured iOS status facts (M7). Proves the native layer
/// surfaces a *durable* last-completion reason and that `getDesiredStateStatus`
/// reports it, so the unified status snapshot can answer "why did the last run
/// end?" — alongside the split scheduling errors (`getSchedulingStatus`) and the
/// other six status questions.
///
/// XCTest runs on `.main`, so the plugin's `onMain { ... }` bodies execute inline
/// and assertions are final synchronously.
final class StatusFactTests: XCTestCase {

    private var plugin: BackgroundServicePlugin!
    private var scheduler: FakeBGTaskScheduler!

    private let outcomeKey = "ios_last_task_outcome"
    private let completionReasonKey = "ios_last_completion_reason"

    override func setUp() {
        super.setUp()
        plugin = BackgroundServicePlugin()
        scheduler = FakeBGTaskScheduler()
        plugin.scheduler = scheduler
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
            "ios_last_task_outcome", "ios_last_completion_reason",
            "ios_adaptive_processing_begin_minutes",
        ] {
            d.removeObject(forKey: key)
        }
    }

    // MARK: - M7 (req 3): persistTaskOutcome writes a durable reason too

    func testPersistTaskOutcome_writesBothConsumableOutcomeAndDurableReason() {
        plugin.persistTaskOutcome("expired")

        let d = UserDefaults.standard
        XCTAssertEqual(d.string(forKey: outcomeKey), "expired",
                       "the consumable adaptation outcome is written")
        XCTAssertEqual(d.string(forKey: completionReasonKey), "expired",
                       "the durable completion reason is written with the same value (M7)")
    }

    // MARK: - M7 (req 3): the reason is DURABLE across scheduleNext

    func testCompletionReason_survivesScheduleNext_whileOutcomeIsConsumed() {
        // A task just ended naturally: both keys carry "completed".
        plugin.persistTaskOutcome("completed")
        // Desired-running so a background transition reschedules (runs scheduleNext).
        UserDefaults.standard.set(true, forKey: "ios_desired_running")

        plugin.appDidEnterBackground()

        let d = UserDefaults.standard
        XCTAssertNil(d.string(forKey: outcomeKey),
                     "scheduleNext() consumes the one-shot adaptation outcome")
        XCTAssertEqual(d.string(forKey: completionReasonKey), "completed",
                       "the durable completion reason must SURVIVE scheduleNext() (M7 'why?')")
    }

    // MARK: - M7: getDesiredStateStatus surfaces the durable reason

    func testGetDesiredStateStatus_reportsLastCompletionReason() {
        plugin.persistTaskOutcome("expired")

        let capture = InvokeCapture()
        plugin.getDesiredStateStatus(capture.makeInvoke())

        XCTAssertEqual(capture.resolveCount, 1)
        let payload = capture.resolvedPayload ?? ""
        XCTAssertTrue(payload.contains("\"lastCompletionReason\":\"expired\""),
                      "getDesiredStateStatus must report the durable last-completion reason: \(payload)")
    }

    func testGetDesiredStateStatus_reasonKeyPresentNullBeforeAnyRun() {
        // Fresh launch: no run has ended, so the key is present but null — the
        // snapshot still *answers* "why?" (nothing yet) rather than omitting it.
        let capture = InvokeCapture()
        plugin.getDesiredStateStatus(capture.makeInvoke())

        let payload = capture.resolvedPayload ?? ""
        XCTAssertTrue(payload.contains("lastCompletionReason"),
                      "the reason fact is always present in the desired-state payload: \(payload)")
    }

    // MARK: - M7 (req 2): split scheduling errors are reported per task type

    func testGetSchedulingStatus_reportsRefreshAndProcessingErrorsIndependently() {
        UserDefaults.standard.set(true, forKey: "ios_desired_running")
        // Fail only the processing identifier; refresh succeeds.
        scheduler.shouldFailSubmit = { $0.identifier.hasSuffix(".bg-processing") }

        plugin.appDidEnterBackground()

        let capture = InvokeCapture()
        plugin.getSchedulingStatus(capture.makeInvoke())
        let payload = capture.resolvedPayload ?? ""
        XCTAssertTrue(payload.contains("\"refreshScheduled\":true"),
                      "refresh scheduled successfully: \(payload)")
        XCTAssertTrue(payload.contains("\"refreshError\":null"),
                      "no refresh error — the split keeps it distinct: \(payload)")
        XCTAssertTrue(payload.contains("\"processingScheduled\":false"),
                      "processing failed to schedule: \(payload)")
        XCTAssertFalse(payload.contains("\"processingError\":null"),
                       "a real processing error is reported, not null: \(payload)")
    }
}
