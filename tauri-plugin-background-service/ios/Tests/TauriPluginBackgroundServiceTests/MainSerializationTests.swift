import XCTest
import BackgroundTasks
@testable import tauri_plugin_background_service

/// H1 (Step 3): the five shared BGTask fields (`currentRefreshTask`,
/// `currentProcessingTask`, `pendingCancelInvoke`, `safetyTimer`, `taskCompleted`)
/// must be accessed from one execution context. Invoke handlers historically ran on
/// the Tauri `ipc` queue while BGTask/expiration handlers fire on `.main`, racing
/// those fields — exactly-once `setTaskCompleted` and single-response invokes held
/// only by luck.
///
/// The fix marshals every `@objc` handler body onto `.main`. These tests prove that
/// through the Step-2 seams: a handler invoked off the main queue resolves on
/// `.main`, and two concurrent terminal paths respond a shared pending invoke
/// exactly once. Both fail on the pre-fix (unserialized) behavior.
final class MainSerializationTests: XCTestCase {

    private var plugin: BackgroundServicePlugin!
    private var scheduler: FakeBGTaskScheduler!

    override func setUp() {
        super.setUp()
        plugin = BackgroundServicePlugin()
        scheduler = FakeBGTaskScheduler()
        plugin.scheduler = scheduler
    }

    override func tearDown() {
        plugin = nil
        scheduler = nil
        super.tearDown()
    }

    // MARK: - Handler body runs on .main even when invoked off-main

    /// Invoking an `@objc` command from a background (ipc-like) queue must run its
    /// body — and therefore its response — on `.main`. Pre-fix the body ran inline
    /// on the calling background queue, so `respondedOnMainThread` was `false`.
    func testObjcHandlerBody_marshalsOntoMain_whenInvokedOffMainQueue() {
        let capture = InvokeCapture()
        let responded = expectation(description: "cancelCancelListener responded")
        capture.onResponse = { responded.fulfill() }

        DispatchQueue(label: "ipc.test").async { [plugin] in
            plugin!.cancelCancelListener(capture.makeInvoke())
        }

        wait(for: [responded], timeout: 5.0)
        XCTAssertEqual(capture.resolveCount, 1, "cancelCancelListener resolves its own invoke")
        XCTAssertEqual(
            capture.respondedOnMainThread, true,
            "the @objc handler body must run on .main (H1 serialization), even when invoked from the ipc queue")
    }

    // MARK: - Concurrent terminal paths respond the pending cancel invoke once

    /// `completeBgTask` and `cancelCancelListener` both consume `pendingCancelInvoke`.
    /// Driven concurrently from two background queues against a stored cancel invoke,
    /// the shared field must yield exactly one response. Pre-fix the unserialized
    /// read-then-nil races and can respond the same invoke twice.
    func testConcurrentTerminalPaths_respondPendingCancelInvokeExactlyOnce() {
        for i in 0..<50 {
            let cancelCapture = InvokeCapture()
            // Stored on the main thread (the test runs on .main) → inline.
            plugin.waitForCancel(cancelCapture.makeInvoke())

            let group = DispatchGroup()
            group.enter(); group.enter()
            DispatchQueue(label: "ipc.complete.\(i)").async { [plugin] in
                plugin!.completeBgTask(InvokeCapture().makeInvoke(args: "{\"success\":true}"))
                group.leave()
            }
            DispatchQueue(label: "ipc.cancel.\(i)").async { [plugin] in
                plugin!.cancelCancelListener(InvokeCapture().makeInvoke())
                group.leave()
            }

            // Both handlers have returned (their marshalled bodies, if any, were
            // enqueued onto .main before each group.leave()). Because .main is a
            // serial FIFO queue, this notify block runs *after* those bodies, so
            // the response counts are final when we assert.
            let drained = expectation(description: "terminal paths drained \(i)")
            group.notify(queue: .main) { drained.fulfill() }
            wait(for: [drained], timeout: 5.0)

            let responses = cancelCapture.resolveCount + cancelCapture.rejectCount
            XCTAssertEqual(
                responses, 1,
                "pendingCancelInvoke must be responded exactly once across concurrent terminal paths (iteration \(i))")
        }
    }
}
