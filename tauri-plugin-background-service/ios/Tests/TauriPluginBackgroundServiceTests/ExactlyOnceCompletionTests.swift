import XCTest
import BackgroundTasks
@testable import tauri_plugin_background_service

/// Step 18 / I4: the safety-critical exactly-once `setTaskCompleted` invariant,
/// proven across all four terminal paths — iOS expiration, the safety timer,
/// Rust-driven natural completion (`completeBgTask`), and `stopKeepalive` — plus
/// duplicate calls, complete-after-expiration, and a concurrent race.
///
/// iOS terminates the app if `setTaskCompleted` is called twice (or never) for a
/// single BGTask. The guard is the shared `taskCompleted` flag plus nil-ing the
/// active-task ref, all on the one serialized `.main` context (H1/Step 3, M3/Step
/// 15, L3). This suite drives the *real* terminal-path methods against a
/// `FakeBGTask` completion recorder (injected via `injectedActiveTask`) and asserts
/// `completionCount == 1` for every path and combination — locking the nil-ref
/// guard in so a future refactor cannot silently reintroduce a double- or
/// zero-completion.
///
/// XCTest runs on `.main`, so each plugin `onMain { ... }` body and `.main`-bound
/// terminal handler executes inline; completion counts are final synchronously
/// except in the explicit concurrent test, which drains the main queue first.
final class ExactlyOnceCompletionTests: XCTestCase {

    private var plugin: BackgroundServicePlugin!
    private var suite: UserDefaults!

    override func setUp() {
        super.setUp()
        // IOS-CLEAN-01: isolated suite so this class cannot leak state.
        suite = TestDefaults.makeIsolatedSuite()
        plugin = freshPlugin()
        // A terminal path that finds a task active reschedules via `scheduleNext`,
        // which only submits when desired_running is true.
        suite.set(true, forKey: "ios_desired_running")
    }

    override func tearDown() {
        TestDefaults.clearAll(on: suite)
        plugin = nil
        suite = nil
        super.tearDown()
    }

    /// A plugin wired to recording/no-op seams so terminal paths never touch real
    /// iOS services (`BGTaskScheduler`, `UNUserNotificationCenter`).
    private func freshPlugin() -> BackgroundServicePlugin {
        let p = BackgroundServicePlugin()
        p.scheduler = FakeBGTaskScheduler()
        p.notificationAuthorizer = FakeNotificationAuthorizer()
        p.defaults = suite
        return p
    }

    /// Install a `FakeBGTask` as the active run and return the completion recorder.
    @discardableResult
    private func startActiveRun(on p: BackgroundServicePlugin? = nil) -> FakeBGTask {
        let fake = FakeBGTask()
        (p ?? plugin).injectedActiveTask = fake
        return fake
    }

    // MARK: - Each terminal path completes exactly once; duplicates are no-ops

    func testExpiration_completesExactlyOnce_evenIfFiredTwice() {
        let fake = startActiveRun()
        plugin.handleExpiration()
        plugin.handleExpiration()  // duplicate terminal path → nil ref + flag → no-op
        XCTAssertEqual(fake.completionCount, 1, "iOS expiration completes the task exactly once")
        XCTAssertEqual(fake.lastSuccess, false, "an expired run completes with success=false")
    }

    func testSafetyTimeout_completesExactlyOnce_evenIfFiredTwice() {
        let fake = startActiveRun()
        plugin.handleSafetyTimerExpiration()
        plugin.handleSafetyTimerExpiration()
        XCTAssertEqual(fake.completionCount, 1, "the safety timer completes the task exactly once")
        XCTAssertEqual(fake.lastSuccess, false, "a self-imposed budget expiry completes with success=false")
    }

    func testManualComplete_completesExactlyOnce_evenIfCalledTwice() {
        let fake = startActiveRun()
        plugin.completeBgTask(InvokeCapture().makeInvoke(args: "{\"success\":true}"))
        plugin.completeBgTask(InvokeCapture().makeInvoke(args: "{\"success\":true}"))
        XCTAssertEqual(fake.completionCount, 1, "manual completeBgTask completes the task exactly once")
        XCTAssertEqual(fake.lastSuccess, true, "a natural completion carries the success flag")
    }

    func testStop_completesExactlyOnce_evenIfCalledTwice() {
        let fake = startActiveRun()
        plugin.stopKeepalive(InvokeCapture().makeInvoke())
        plugin.stopKeepalive(InvokeCapture().makeInvoke())
        XCTAssertEqual(fake.completionCount, 1, "stopKeepalive completes the active task exactly once")
        XCTAssertEqual(fake.lastSuccess, false, "a user stop completes the in-flight task with success=false")
    }

    // MARK: - Cross-path combinations stay at one completion (complete-after-X)

    func testCompleteAfterExpiration_completesExactlyOnce() {
        let fake = startActiveRun()
        plugin.handleExpiration()                                   // iOS expired the run
        plugin.completeBgTask(                                      // late Rust completion
            InvokeCapture().makeInvoke(args: "{\"success\":true}"))
        XCTAssertEqual(fake.completionCount, 1,
                       "a manual completion arriving after expiration must not double-complete")
        XCTAssertEqual(fake.lastSuccess, false,
                       "the expiration's completion stands; the late manual call is a no-op")
    }

    func testStopAfterExpiration_completesExactlyOnce() {
        let fake = startActiveRun()
        plugin.handleExpiration()
        plugin.stopKeepalive(InvokeCapture().makeInvoke())
        XCTAssertEqual(fake.completionCount, 1, "a stop after expiration must not double-complete")
    }

    func testSafetyThenExpiration_completesExactlyOnce() {
        let fake = startActiveRun()
        plugin.handleSafetyTimerExpiration()
        plugin.handleExpiration()
        XCTAssertEqual(fake.completionCount, 1,
                       "a safety-timer completion followed by an iOS expiration must not double-complete")
    }

    // MARK: - Concurrent terminal paths from background queues complete once

    /// `completeBgTask` and `stopKeepalive` both marshal onto `.main` and both end
    /// the active task. Driven concurrently from two background (ipc-like) queues
    /// against a freshly-injected task, the shared one-shot guard must yield exactly
    /// one `setTaskCompleted`. A fresh plugin per iteration starts from
    /// `taskCompleted == false` (the flag is intentionally never reset by cleanup —
    /// M3); `.main` is a serial FIFO queue, so the `group.notify` block runs after
    /// both marshalled bodies and the count is final when asserted.
    func testConcurrentManualCompleteAndStop_completeExactlyOnce() {
        for i in 0..<50 {
            let p = freshPlugin()
            let fake = startActiveRun(on: p)

            let group = DispatchGroup()
            group.enter(); group.enter()
            DispatchQueue(label: "ipc.complete.\(i)").async {
                p.completeBgTask(InvokeCapture().makeInvoke(args: "{\"success\":true}"))
                group.leave()
            }
            DispatchQueue(label: "ipc.stop.\(i)").async {
                p.stopKeepalive(InvokeCapture().makeInvoke())
                group.leave()
            }

            let drained = expectation(description: "terminal paths drained \(i)")
            group.notify(queue: .main) { drained.fulfill() }
            wait(for: [drained], timeout: 5.0)

            XCTAssertEqual(
                fake.completionCount, 1,
                "concurrent completeBgTask + stopKeepalive must complete the task exactly once (iteration \(i))")
        }
    }
}
