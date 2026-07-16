import XCTest
import BackgroundTasks
@testable import tauri_plugin_background_service

/// H14: the warm-BGTask delivery signal. The Rust warm-start listener blocks on
/// `waitForBgTask`; a BGTask delivered to the warm process resolves it (so Rust
/// drives `run_warm_start`), and teardown rejects it (so the Rust thread does not
/// leak). Mirrors the `waitForCancel` Pending Invoke pattern.
///
/// XCTest runs on `.main`, so the plugin's `onMain { ... }` bodies execute inline
/// and the resolve/reject counts are final synchronously.
final class WarmDeliveryTests: XCTestCase {

    private var plugin: BackgroundServicePlugin!

    override func setUp() {
        super.setUp()
        plugin = BackgroundServicePlugin()
    }

    override func tearDown() {
        plugin = nil
        super.tearDown()
    }

    // MARK: - waitForBgTask holds the invoke (blocks Rust)

    func testWaitForBgTask_holdsInvoke_withoutResolving() {
        let capture = InvokeCapture()
        plugin.waitForBgTask(capture.makeInvoke())

        XCTAssertEqual(capture.resolveCount, 0,
                       "waitForBgTask must NOT resolve immediately — it blocks the Rust listener")
        XCTAssertEqual(capture.rejectCount, 0,
                       "waitForBgTask must NOT reject a freshly-stored invoke")
    }

    // MARK: - Delivery resolves the held warm invoke (wakes Rust)

    func testResolvePendingWarmInvoke_resolvesHeldInvoke_exactlyOnce() {
        let capture = InvokeCapture()
        plugin.waitForBgTask(capture.makeInvoke())

        // Simulate a BGTask delivery — the handlers call this after persisting.
        plugin.resolvePendingWarmInvoke()

        XCTAssertEqual(capture.resolveCount, 1,
                       "a delivery must resolve the held warm invoke so Rust warm-starts")
        XCTAssertEqual(capture.rejectCount, 0, "delivery resolves, never rejects")

        // A second delivery with no listener registered is a clean no-op.
        plugin.resolvePendingWarmInvoke()
        XCTAssertEqual(capture.resolveCount, 1,
                       "the warm invoke is consumed exactly once per delivery")
    }

    // MARK: - Teardown rejects the held warm invoke (unblocks Rust)

    func testCancelWarmListener_rejectsHeldInvoke_andResolvesOwn() {
        let warmCapture = InvokeCapture()
        plugin.waitForBgTask(warmCapture.makeInvoke())

        let cancelCapture = InvokeCapture()
        plugin.cancelWarmListener(cancelCapture.makeInvoke())

        XCTAssertEqual(warmCapture.rejectCount, 1,
                       "cancelWarmListener must reject the held warm invoke to unblock the Rust thread")
        XCTAssertEqual(warmCapture.resolveCount, 0, "teardown rejects the held invoke, never resolves it")
        XCTAssertEqual(cancelCapture.resolveCount, 1,
                       "cancelWarmListener resolves its own invoke")
    }

    func testCancelWarmListener_withNoListener_resolvesOwnInvoke() {
        let cancelCapture = InvokeCapture()
        plugin.cancelWarmListener(cancelCapture.makeInvoke())

        XCTAssertEqual(cancelCapture.resolveCount, 1,
                       "cancelWarmListener must always resolve its own invoke, even with no listener held")
    }

    // MARK: - A new listener supersedes a stale one

    func testWaitForBgTask_supersedesPreviousInvoke() {
        let first = InvokeCapture()
        plugin.waitForBgTask(first.makeInvoke())

        let second = InvokeCapture()
        plugin.waitForBgTask(second.makeInvoke())

        XCTAssertEqual(first.rejectCount, 1,
                       "a new warm listener must reject the previously-held (stale) invoke")
        XCTAssertEqual(second.resolveCount, 0, "the newest invoke is held, not resolved")
        XCTAssertEqual(second.rejectCount, 0, "the newest invoke is held, not rejected")

        // Only the newest invoke is resolved on delivery.
        plugin.resolvePendingWarmInvoke()
        XCTAssertEqual(second.resolveCount, 1, "delivery resolves the current (newest) warm invoke")
        XCTAssertEqual(first.resolveCount, 0, "the superseded invoke is never resolved")
    }
}
