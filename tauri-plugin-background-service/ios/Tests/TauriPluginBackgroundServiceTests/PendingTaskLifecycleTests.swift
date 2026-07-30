import XCTest
import BackgroundTasks
@testable import tauri_plugin_background_service

/// H5/M14 (part 1): proves the pending-BGTask lifecycle enforces
/// `pending ⟺ consumedAt == nil`. `getPendingBgTask` gates on `consumedAt`, and
/// `clearPendingBgTask` deletes every pending key so a consumed/stale record
/// can't re-arm a cold auto-start. Before this step `clearPendingBgTask` only
/// stamped `consumedAt` and `getPendingBgTask` ignored it, so a cleared record
/// still resolved as pending.
final class PendingTaskLifecycleTests: XCTestCase {

    private var plugin: BackgroundServicePlugin!
    private var suite: UserDefaults!

    // The literal UserDefaults keys the plugin persists pending state under
    // (mirrors `PendingTaskKeys`, which is private).
    private let kindKey = "ios_pending_task_kind"
    private let identifierKey = "ios_pending_task_identifier"
    private let receivedAtKey = "ios_pending_task_received_at"
    private let consumedAtKey = "ios_pending_task_consumed_at"
    private let lastFailedAtKey = "ios_pending_task_last_failed_at"

    override func setUp() {
        super.setUp()
        plugin = BackgroundServicePlugin()
        // IOS-CLEAN-01: isolated suite so this class cannot leak state.
        suite = TestDefaults.makeIsolatedSuite()
        plugin.defaults = suite
        TestDefaults.clearAll(on: suite)
    }

    override func tearDown() {
        TestDefaults.clearAll(on: suite)
        plugin = nil
        suite = nil
        super.tearDown()
    }

    /// Simulate a stored, unconsumed pending BGTask record.
    private func storePending(kind: String = "refresh", identifier: String = "com.example.bg-refresh") {
        suite.set(kind, forKey: kindKey)
        suite.set(identifier, forKey: identifierKey)
        suite.set(1_700_000_000.0 as TimeInterval, forKey: receivedAtKey)
        suite.removeObject(forKey: consumedAtKey)
    }

    // MARK: - H5: clear hides pending and deletes all keys

    func testClearThenGet_goesSomeToNone_andDeletesAllKeys() {
        storePending()

        // Before clear: the record is visible (Some).
        let before = InvokeCapture()
        plugin.getPendingBgTask(before.makeInvoke())
        XCTAssertEqual(before.resolveCount, 1)
        XCTAssertTrue((before.resolvedPayload ?? "").contains("\"taskKind\":\"refresh\""),
                      "pending should be visible before clear: \(before.resolvedPayload ?? "nil")")

        // Clear.
        let cleared = InvokeCapture()
        plugin.clearPendingBgTask(cleared.makeInvoke())
        XCTAssertEqual(cleared.resolveCount, 1)

        // After clear: no pending task (None), and every key is gone.
        let after = InvokeCapture()
        plugin.getPendingBgTask(after.makeInvoke())
        XCTAssertEqual(after.resolveCount, 1)
        let payload = after.resolvedPayload ?? ""
        XCTAssertTrue(payload.contains("\"taskKind\":null"),
                      "pending must be None after clear: \(payload)")

        let d = suite!
        XCTAssertNil(d.object(forKey: kindKey), "kind key must be deleted")
        XCTAssertNil(d.object(forKey: identifierKey), "identifier key must be deleted")
        XCTAssertNil(d.object(forKey: receivedAtKey), "receivedAt key must be deleted")
        XCTAssertNil(d.object(forKey: consumedAtKey), "consumedAt key must be deleted")
    }

    // MARK: - H5: getPendingBgTask gates on consumedAt

    func testGet_consumedRecord_reportsNoPending() {
        storePending()
        // Stamp consumedAt without deleting the other keys (a stale record).
        suite.set(1_700_000_060.0 as TimeInterval, forKey: consumedAtKey)

        let capture = InvokeCapture()
        plugin.getPendingBgTask(capture.makeInvoke())

        XCTAssertEqual(capture.resolveCount, 1)
        let payload = capture.resolvedPayload ?? ""
        XCTAssertTrue(payload.contains("\"taskKind\":null"),
                      "consumed record must report no pending task: \(payload)")
    }

    func testGet_unconsumedRecord_reportsPending() {
        storePending(kind: "processing", identifier: "com.example.bg-processing")

        let capture = InvokeCapture()
        plugin.getPendingBgTask(capture.makeInvoke())

        XCTAssertEqual(capture.resolveCount, 1)
        let payload = capture.resolvedPayload ?? ""
        XCTAssertTrue(payload.contains("\"taskKind\":\"processing\""),
                      "unconsumed record must report pending: \(payload)")
        XCTAssertTrue(payload.contains("\"consumedAt\":null"),
                      "visible pending must carry null consumedAt: \(payload)")
    }

    // MARK: - H3: failure marker preserves the pending record

    func testRecordFailedPending_preservesPending_andStampsMarker() {
        plugin.now = { 1_700_000_500.0 }
        storePending(kind: "refresh", identifier: "com.example.bg-refresh")

        // Record a failure marker (as the cold auto-start does on Start failure).
        let recorded = InvokeCapture()
        plugin.recordFailedPending(recorded.makeInvoke())
        XCTAssertEqual(recorded.resolveCount, 1)

        // The pending record must survive — the failure must NOT consume it.
        let after = InvokeCapture()
        plugin.getPendingBgTask(after.makeInvoke())
        XCTAssertEqual(after.resolveCount, 1)
        let payload = after.resolvedPayload ?? ""
        XCTAssertTrue(payload.contains("\"taskKind\":\"refresh\""),
                      "pending must be preserved after a recorded failure: \(payload)")

        // The failure marker is stamped for diagnostics.
        let marker = suite.object(forKey: lastFailedAtKey) as? TimeInterval
        XCTAssertEqual(marker, 1_700_000_500.0,
                       "lastFailedAt marker must be stamped via the clock seam")
    }
}
