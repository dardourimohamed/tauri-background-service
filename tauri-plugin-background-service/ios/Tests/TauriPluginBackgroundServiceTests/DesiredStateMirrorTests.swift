import XCTest
import BackgroundTasks
@testable import tauri_plugin_background_service

/// H4 (Step 5): proves the iOS `setDesiredRunning` mirror handler gives the
/// intent-only recovery commands a real, observable effect on iOS — it writes
/// `desiredRunning` (+ `lastStartConfig`) into `UserDefaults` and (re)schedules
/// or cancels BGTasks — instead of the previous silent no-op. Rust owns the
/// desired-state authority (D1); this is the mirror seam it drives.
final class DesiredStateMirrorTests: XCTestCase {

    private var plugin: BackgroundServicePlugin!
    private var scheduler: FakeBGTaskScheduler!
    private var suite: UserDefaults!

    override func setUp() {
        super.setUp()
        plugin = BackgroundServicePlugin()
        scheduler = FakeBGTaskScheduler()
        plugin.scheduler = scheduler
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

    // MARK: - desiredRunning=true → persist + schedule

    func testSetDesiredRunningTrue_persistsAndSchedules() {
        let capture = InvokeCapture()
        plugin.setDesiredRunning(
            capture.makeInvoke(
                args: "{\"desiredRunning\":true,\"lastStartConfig\":\"{\\\"serviceLabel\\\":\\\"App\\\"}\"}"
            )
        )

        XCTAssertEqual(capture.resolveCount, 1, "mirror must resolve, never silently no-op")
        // UserDefaults mirrored.
        XCTAssertTrue(suite.bool(forKey: "ios_desired_running"))
        XCTAssertEqual(
            suite.string(forKey: "ios_last_start_config"),
            "{\"serviceLabel\":\"App\"}",
            "lastStartConfig should be persisted verbatim for auto-start to parse"
        )
        // BGTasks scheduled (refresh + processing submitted via the fake scheduler).
        XCTAssertEqual(scheduler.submitted.count, 2, "both BGTask types should be scheduled")
    }

    // MARK: - desiredRunning=false → persist + cancel

    func testSetDesiredRunningFalse_persistsAndCancels() {
        // Arm first so there is something to cancel.
        plugin.setDesiredRunning(InvokeCapture().makeInvoke(args: "{\"desiredRunning\":true}"))

        let capture = InvokeCapture()
        plugin.setDesiredRunning(capture.makeInvoke(args: "{\"desiredRunning\":false}"))

        XCTAssertEqual(capture.resolveCount, 1)
        XCTAssertFalse(suite.bool(forKey: "ios_desired_running"))
        // Both BGTask types are cancelled on desired=false. (scheduleNext now also
        // cancels-before-submit per L1, so the cancel log carries the earlier
        // arming reschedule's cancels too — assert presence of both ids, not an
        // exact count.)
        XCTAssertTrue(scheduler.cancelled.contains { $0.hasSuffix(".bg-refresh") },
                      "refresh BGTask should be cancelled")
        XCTAssertTrue(scheduler.cancelled.contains { $0.hasSuffix(".bg-processing") },
                      "processing BGTask should be cancelled")
    }

    // MARK: - getDesiredStateStatus reflects the mirror (Rust↔Swift agreement)

    func testGetDesiredStateStatus_reflectsMirror() {
        plugin.setDesiredRunning(InvokeCapture().makeInvoke(args: "{\"desiredRunning\":true}"))

        let capture = InvokeCapture()
        plugin.getDesiredStateStatus(capture.makeInvoke())

        let payload = capture.resolvedPayload ?? ""
        XCTAssertTrue(
            payload.contains("\"desiredRunning\":true"),
            "getDesiredStateStatus must reflect the mirrored desired state: \(payload)"
        )
    }
}
