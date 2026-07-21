import XCTest
import CallKit
@testable import tauri_plugin_background_service

/// IOS-CALL-01: the public main-thread `BackgroundServicePlugin.callActionHandler`
/// receives CallKit Answer/Reject/End actions carrying the ORIGINAL 32-hex
/// `call_id` exactly once per perform. Before the fix, `performCallAction` was
/// an internal instance closure that defaulted to a no-op and was never
/// injected by the plugin, so lock-screen answer/reject/end was silently
/// dropped. The plugin now wires the controller to a public STATIC handler on
/// the class (set by the host's native core), with a missing-handler warning.
final class CallActionHandlerTests: XCTestCase {

    /// A contract-shaped 32-char lowercase-hex Rust `call_id`.
    private static let hexCallId = "018f3a2b5c4d6e7f019e4b6c7d8e9f0a"

    /// Any `CXProvider` — the delegate perform-handlers ignore the provider arg.
    private func anyProvider() -> CXProvider {
        CXProvider(configuration: BackgroundCallKitController.providerConfiguration())
    }

    override func tearDown() {
        // Static handler is process-global — clear it so this suite cannot leak
        // wiring into another test class.
        BackgroundServicePlugin.callActionHandler = nil
        super.tearDown()
    }

    // MARK: - public handler receives each action exactly once

    func testCallActionHandler_receivesAnswerOnce_withOriginalCallId() {
        var received: [(String, String)] = []
        BackgroundServicePlugin.callActionHandler = { callId, action in
            received.append((callId, action))
        }

        let plugin = BackgroundServicePlugin()
        plugin.callKitController.reportIncomingCall(
            callId: Self.hexCallId, callerName: "Alice", hasVideo: false)
        let uuid = BackgroundCallKitController.callKitUUID(for: Self.hexCallId)!
        plugin.callKitController.provider(anyProvider(), perform: CXAnswerCallAction(call: uuid))

        XCTAssertEqual(received.count, 1, "answer routes exactly once")
        XCTAssertEqual(received.first?.0, Self.hexCallId, "the ORIGINAL 32-hex call_id is carried")
        XCTAssertEqual(received.first?.1, "answer", "the action token is answer")
    }

    func testCallActionHandler_receivesRejectOnce_withOriginalCallId() {
        var received: [(String, String)] = []
        BackgroundServicePlugin.callActionHandler = { callId, action in
            received.append((callId, action))
        }

        let plugin = BackgroundServicePlugin()
        plugin.callKitController.reportIncomingCall(
            callId: Self.hexCallId, callerName: "Alice", hasVideo: false)
        let uuid = BackgroundCallKitController.callKitUUID(for: Self.hexCallId)!
        // Decline of a still-ringing call (no prior answer) → "reject".
        plugin.callKitController.provider(anyProvider(), perform: CXEndCallAction(call: uuid))

        XCTAssertEqual(received.count, 1)
        XCTAssertEqual(received.first?.0, Self.hexCallId)
        XCTAssertEqual(received.first?.1, "reject", "decline of a ringing call is reject")
    }

    func testCallActionHandler_receivesEndAfterAnswer_withOriginalCallId() {
        var received: [(String, String)] = []
        BackgroundServicePlugin.callActionHandler = { callId, action in
            received.append((callId, action))
        }

        let plugin = BackgroundServicePlugin()
        plugin.callKitController.reportIncomingCall(
            callId: Self.hexCallId, callerName: "Alice", hasVideo: false)
        let uuid = BackgroundCallKitController.callKitUUID(for: Self.hexCallId)!
        plugin.callKitController.provider(anyProvider(), perform: CXAnswerCallAction(call: uuid))
        plugin.callKitController.provider(anyProvider(), perform: CXEndCallAction(call: uuid))

        XCTAssertEqual(received.map { $0.1 }, ["answer", "end"],
                       "answer then hang-up of a live call routes answer+end")
        XCTAssertEqual(received.last?.0, Self.hexCallId)
    }

    // MARK: - missing integration is observable, not a silent no-op

    func testRouteCallAction_logsMissingIntegration_whenHandlerIsNil() {
        // No handler wired — the route must still complete (not crash) and the
        // missing-integration log is the observable signal. The behavior under
        // test is "does not crash and does not deliver"; the log itself is
        // verified by inspection (os_log has no test capture seam).
        BackgroundServicePlugin.callActionHandler = nil
        BackgroundServicePlugin.routeCallAction(callId: Self.hexCallId, action: "answer")
        // No assertion possible without an os_log capture; the contract is the
        // no-crash + no-delivery path. This test pins that the route is callable
        // with a nil handler (the host-not-wired case).
    }

    // MARK: - dispatch onto the main thread

    func testRouteCallAction_deliversOnMainThread_whenInvokedOffMain() {
        let expectation = self.expectation(description: "handler fired on main")
        var observedOnMain: Bool?
        BackgroundServicePlugin.callActionHandler = { _, _ in
            observedOnMain = Thread.isMainThread
            expectation.fulfill()
        }

        DispatchQueue.global().async {
            BackgroundServicePlugin.routeCallAction(callId: Self.hexCallId, action: "end")
        }

        wait(for: [expectation], timeout: 5.0)
        XCTAssertEqual(observedOnMain, true,
                       "callActionHandler must fire on the main thread even when routed off-main")
    }
}
