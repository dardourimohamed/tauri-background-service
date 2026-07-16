import XCTest
@testable import tauri_plugin_background_service

/// M-NATIVE-4 / NR-6 (Step 12): proves `showIncomingCall` honors the
/// `BackgroundCallDecision.shouldRingCallKit` foreground gate (DEC-060 "one ring owner
/// per app-state"). While the webview is foreground the in-app IncomingCallScreen
/// owns the ring, so the CallKit ring must be suppressed (no double-ring); while
/// backgrounded the native CallKit ring fires. The gate reads the live UIKit app
/// state in production; here the `appIsForeground` seam is injected so the wiring
/// is provable without a real app lifecycle.
final class ForegroundRingGateTests: XCTestCase {

    private var plugin: BackgroundServicePlugin!

    /// A contract-shaped 32-char lowercase-hex Rust `call_id` (passes `isValidCallId`).
    private static let validCallId = "018f3a2b5c4d6e7f019e4b6c7d8e9f0a"

    override func setUp() {
        super.setUp()
        plugin = BackgroundServicePlugin()
        // Avoid touching the real BGTaskScheduler if any path reaches it.
        plugin.scheduler = FakeBGTaskScheduler()
    }

    override func tearDown() {
        plugin = nil
        super.tearDown()
    }

    private func validArgs(isVideo: Bool = false) -> String {
        "{\"callId\":\"\(Self.validCallId)\",\"callerName\":\"Alice\",\"isVideo\":\(isVideo)}"
    }

    func testShowIncomingCall_foreground_suppressesCallKitRing() {
        plugin.appIsForeground = { true }
        let capture = InvokeCapture()
        plugin.showIncomingCall(capture.makeInvoke(args: validArgs()))

        // Still resolves — the in-app overlay owns the ring; this is not an error.
        XCTAssertEqual(capture.resolveCount, 1)
        XCTAssertEqual(capture.rejectCount, 0)
        // No call reported to CallKit → no double-ring.
        XCTAssertEqual(plugin.callKitController.activeCallCount, 0,
                       "a foreground call must NOT ring CallKit (the in-app overlay owns it)")
    }

    func testShowIncomingCall_background_ringsCallKit() {
        plugin.appIsForeground = { false }
        let capture = InvokeCapture()
        plugin.showIncomingCall(capture.makeInvoke(args: validArgs(isVideo: true)))

        XCTAssertEqual(capture.resolveCount, 1)
        XCTAssertEqual(plugin.callKitController.activeCallCount, 1,
                       "a backgrounded call must fire the native CallKit ring")
    }

    func testShowIncomingCall_validationPrecedesGate() {
        // A malformed call_id is rejected regardless of app state — validation runs
        // before the foreground gate, so a bad call never rings and never silently
        // resolves.
        plugin.appIsForeground = { false }
        let capture = InvokeCapture()
        plugin.showIncomingCall(capture.makeInvoke(args: "{\"callId\":\"bad\",\"callerName\":\"Alice\"}"))

        XCTAssertEqual(capture.rejectCount, 1)
        XCTAssertEqual(capture.resolveCount, 0)
        XCTAssertEqual(plugin.callKitController.activeCallCount, 0)
    }
}
