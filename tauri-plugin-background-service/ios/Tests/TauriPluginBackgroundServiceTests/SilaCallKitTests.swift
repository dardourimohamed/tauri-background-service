import XCTest
import CallKit
import AVFoundation
@testable import tauri_plugin_background_service

/// Pure-logic tests for the iOS CallKit / audio-session wrapper (spec 08 C6, Step 16).
///
/// Per the spec-01 seam philosophy, only the pure F3 decision + audio-session config choice
/// is unit-testable here; the `CXProvider` / `AVAudioSession` runtime glue
/// (`SilaCallKitController`) is exercised on-device in the verify-calls runbook (Step 20).
final class SilaCallKitTests: XCTestCase {

    // MARK: - F3 degraded-mode decision

    func testDeliveryAction_foregroundRings() {
        XCTAssertEqual(
            SilaCallDecision.deliveryAction(appState: .foreground, offerHasVideo: false),
            .ring(hasVideo: false)
        )
        XCTAssertEqual(
            SilaCallDecision.deliveryAction(appState: .foreground, offerHasVideo: true),
            .ring(hasVideo: true)
        )
    }

    func testDeliveryAction_backgroundActiveRings() {
        XCTAssertEqual(
            SilaCallDecision.deliveryAction(appState: .backgroundActive, offerHasVideo: true),
            .ring(hasVideo: true)
        )
    }

    func testDeliveryAction_suspendedDefersToControlOutbox() {
        // F3(c): a suspended app cannot ring; the caller gets Unreachable (T1) + a
        // missed-call control-outbox record. The hasVideo flag is irrelevant here.
        XCTAssertEqual(
            SilaCallDecision.deliveryAction(appState: .suspended, offerHasVideo: false),
            .deferToControlOutbox
        )
        XCTAssertEqual(
            SilaCallDecision.deliveryAction(appState: .suspended, offerHasVideo: true),
            .deferToControlOutbox
        )
    }

    func testDeliveryAction_videoFlagRoundTripsIntoRing() {
        // The has-video flag must propagate into the ring action (drives the CXCallUpdate
        // + audio-session routing). Probe: it is never dropped.
        for hasVideo in [false, true] {
            let action = SilaCallDecision.deliveryAction(appState: .foreground, offerHasVideo: hasVideo)
            XCTAssertEqual(action, .ring(hasVideo: hasVideo))
        }
    }

    // MARK: - Audio-session configuration

    func testAudioConfig_audioCallUsesVoiceChatNoSpeaker() {
        let cfg = SilaAudioSessionConfiguration.audioCall
        XCTAssertEqual(cfg.category, "AVAudioSessionCategoryPlayAndRecord")
        XCTAssertEqual(cfg.mode, "AVAudioSessionModeVoiceChat")
        // CallKit's didActivate owns routing for audio calls → earpiece, not speaker.
        XCTAssertFalse(cfg.defaultToSpeaker)
        XCTAssertTrue(cfg.allowsBluetoothA2DP)
    }

    func testAudioConfig_videoCallUsesVideoChatDefaultsToSpeaker() {
        let cfg = SilaAudioSessionConfiguration.videoCall
        XCTAssertEqual(cfg.category, "AVAudioSessionCategoryPlayAndRecord")
        XCTAssertEqual(cfg.mode, "AVAudioSessionModeVideoChat")
        XCTAssertTrue(cfg.defaultToSpeaker)
        XCTAssertTrue(cfg.allowsBluetoothA2DP)
    }

    func testAudioConfig_forCallSelectsByVideoFlag() {
        XCTAssertEqual(SilaAudioSessionConfiguration.forCall(hasVideo: false), .audioCall)
        XCTAssertEqual(SilaAudioSessionConfiguration.forCall(hasVideo: true), .videoCall)
    }

    func testProviderConfiguration_isSelfManagedNoHoldingNoRecents() {
        // Held is v1.1 (spec §3.3) → supportsHolding is never advertised; calls are not
        // written to the system recents (Sila keeps its own encrypted call_log).
        let config = SilaCallKitController.providerConfiguration()
        XCTAssertEqual(config.maximumCallGroups, 1)
        XCTAssertEqual(config.maximumCallsPerCallGroup, 1)
        XCTAssertEqual(config.supportsVideo, true)
        XCTAssertEqual(config.includesCallsInRecents, false)
    }

    // MARK: - Call-id → CallKit UUID derivation

    /// A realistic `call_id` is the 32-char lowercase hex string minted by
    /// `core::call_manager::new_call_id` (`format!("{:016x}{:016x}", …)`) — exactly 128
    /// bits, NOT an RFC-4122 UUID string. This is the value `reportIncomingCall` /
    /// `endCall` actually receive.
    private static let hexCallId = "018f3a2b5c4d6e7f019e4b6c7d8e9f0a"

    func testCallKitUUID_isDeterministicFor32HexCharCallId() {
        // Regression for the review.rejected bug: the prior `UUID(uuidString: callId) ??
        // UUID()` parsed nil for a 32-char hex id and drew an INDEPENDENT random UUID on
        // every call. A deterministic derivation must return the SAME UUID twice.
        guard let uuidA = SilaCallKitController.callKitUUID(for: Self.hexCallId) else {
            return XCTFail("callKitUUID must be non-nil for a 32-char hex call_id")
        }
        let uuidB = SilaCallKitController.callKitUUID(for: Self.hexCallId)
        XCTAssertEqual(uuidA, uuidB, "callKitUUID must be deterministic for a fixed call_id")
    }

    func testCallKitUUID_reportAndEndAgreeOnSameCallId() {
        // The core guarantee the cancel path depends on: the UUID `endCall` derives must
        // equal the UUID `reportIncomingCall` derived for the SAME call_id, so CallKit can
        // dismiss the ring it showed. Both sites call the same pure `callKitUUID(for:)`,
        // so this is tautologically true post-fix and impossible under the old random-UUID
        // code — which is exactly why the ring was never dismissed.
        let reported = SilaCallKitController.callKitUUID(for: Self.hexCallId)
        let ended = SilaCallKitController.callKitUUID(for: Self.hexCallId)
        XCTAssertNotNil(reported)
        XCTAssertEqual(reported, ended)
    }

    func testCallKitUUID_distinctCallIdsYieldDistinctUUIDs() {
        // Sanity: two distinct session keys must not collapse onto the same CallKit UUID
        // (would otherwise cross-dismiss an unrelated call). Only the leading word differs.
        let other = "018f3a2b5c4d6e7f0000000000000000"
        let a = SilaCallKitController.callKitUUID(for: Self.hexCallId)
        let b = SilaCallKitController.callKitUUID(for: other)
        XCTAssertNotNil(a)
        XCTAssertNotNil(b)
        XCTAssertNotEqual(a, b)
    }

    func testCallKitUUID_rejectsMalformedIds() {
        // Only the defensive fallback (`?? UUID()`) is reachable for a malformed id.
        // A 32-char hex id is the contract; anything else returns nil.
        XCTAssertNil(SilaCallKitController.callKitUUID(for: ""))                 // empty
        XCTAssertNil(SilaCallKitController.callKitUUID(for: "018f3a2b5c4d6e7f019e4b6c7d8e9f0"))  // 30 chars (too short)
        XCTAssertNil(SilaCallKitController.callKitUUID(for: "018f3a2b5c4d6e7f019e4b6c7d8e9f0a0"))  // 33 chars (too long)
        XCTAssertNil(SilaCallKitController.callKitUUID(for: "zzzz3a2b5c4d6e7f019e4b6c7d8e9f0a"))  // non-hex
        // A real RFC-4122 UUID STRING ("8-4-4-4-12" w/ hyphens) is the wrong shape too —
        // the contract is the undashed 32-hex session key, so this also returns nil.
        XCTAssertNil(SilaCallKitController.callKitUUID(for: "018f3a2b-5c4d-6e7f-019e-4b6c7d8e9f0a"))
    }

    // MARK: - BGS-10 perform-handlers route answer/reject/end to Rust via `sila_call_action`

    /// A second realistic 32-hex `call_id` (only the low word differs from `hexCallId`)
    /// for the interleaved-report L6 case.
    private static let otherHexCallId = "018f3a2b5c4d6e7f0000000000000000"

    /// Any `CXProvider` — the delegate perform-handlers ignore the provider argument; the
    /// controller drives its own internal provider for real reports. Built fresh per test.
    private func anyProvider() -> CXProvider {
        CXProvider(configuration: SilaCallKitController.providerConfiguration())
    }

    func testPerformAnswer_routesAnswerActionWithOriginalCallId() {
        // BGS-10 (Step 18 Task B): tapping Answer in the native CallKit UI delivers
        // CXAnswerCallAction; the perform-handler must reverse-look-up the original
        // 32-hex call_id from the [UUID:String] map populated by reportIncomingCall
        // and route it DIRECTLY to Rust via `sila_call_action` with action "answer"
        // (CROSS-DOC: doc 04 owns call-control semantics). The seam defaults to the
        // real FFI; here a recorder captures the routed (call_id, action) without a
        // live Core (the real-symbol reachability is pinned host-side by
        // `bgs10_native_accept_drives_fsm`).
        let controller = SilaCallKitController()
        var actions: [(String, String)] = []
        controller.performCallAction = { callId, action in actions.append((callId, action)) }

        controller.reportIncomingCall(callId: Self.hexCallId, callerName: "Alice", hasVideo: false)
        let uuid = SilaCallKitController.callKitUUID(for: Self.hexCallId)!
        controller.provider(anyProvider(), perform: CXAnswerCallAction(call: uuid))

        XCTAssertEqual(actions.count, 1, "answer routes exactly one call action")
        XCTAssertEqual(actions.first?.1, "answer", "the action token is answer")
        XCTAssertEqual(actions.first?.0, Self.hexCallId, "the ORIGINAL 32-hex call_id is carried, not the UUID")
    }

    func testPerformEnd_afterAnswer_routesEndAction() {
        // BGS-10 (Task B): an end after a successful answer is a hang-up of a live
        // call → action "end" (wasAnswered true), distinguishing a decline ("reject").
        let controller = SilaCallKitController()
        var actions: [(String, String)] = []
        controller.performCallAction = { callId, action in actions.append((callId, action)) }

        controller.reportIncomingCall(callId: Self.hexCallId, callerName: "Alice", hasVideo: false)
        let uuid = SilaCallKitController.callKitUUID(for: Self.hexCallId)!
        controller.provider(anyProvider(), perform: CXAnswerCallAction(call: uuid))
        controller.provider(anyProvider(), perform: CXEndCallAction(call: uuid))

        XCTAssertEqual(actions.map { $0.1 }, ["answer", "end"], "answer then end (wasAnswered)")
        XCTAssertEqual(actions.last?.0, Self.hexCallId, "end carries the original call_id")
    }

    func testPerformEnd_withoutAnswer_routesRejectAction() {
        // BGS-10 (Task B): declining a still-ringing call (no prior answer) → action
        // "reject" (wasAnswered false), so Rust/UI records the right call_log state.
        let controller = SilaCallKitController()
        var actions: [(String, String)] = []
        controller.performCallAction = { callId, action in actions.append((callId, action)) }

        controller.reportIncomingCall(callId: Self.hexCallId, callerName: "Alice", hasVideo: false)
        let uuid = SilaCallKitController.callKitUUID(for: Self.hexCallId)!
        controller.provider(anyProvider(), perform: CXEndCallAction(call: uuid))

        XCTAssertEqual(actions.count, 1)
        XCTAssertEqual(actions.first?.1, "reject", "decline of a ringing call is reject")
        XCTAssertEqual(actions.first?.0, Self.hexCallId)
    }

    func testPerformAnswer_unknownUuid_doesNotRoute() {
        // BGS-10 (Task B): a perform action for a call we never reported (stale/
        // cross-app) must not route a bogus action — there is no original call_id.
        let controller = SilaCallKitController()
        var actions: [(String, String)] = []
        controller.performCallAction = { callId, action in actions.append((callId, action)) }

        controller.provider(anyProvider(), perform: CXAnswerCallAction(call: UUID()))

        XCTAssertTrue(actions.isEmpty, "no map entry → no routed action")
    }

    // MARK: - L6 per-call video flag derived from activeCalls (no clobbered scalar)

    func testAudioConfiguration_matchesAnsweredCallDespiteInterleavedReport() {
        // L6 regression: the old `currentCallHasVideo` scalar was overwritten by the LAST
        // reportIncomingCall, so answering an earlier video call after a later audio call
        // was reported configured the session for audio. The flag must come from the
        // ANSWERED call's entry in `activeCalls`, not a shared scalar.
        let controller = SilaCallKitController()
        controller.reportIncomingCall(callId: Self.hexCallId, callerName: "Alice", hasVideo: true)   // video
        controller.reportIncomingCall(callId: Self.otherHexCallId, callerName: "Bob", hasVideo: false) // audio, later

        let videoUUID = SilaCallKitController.callKitUUID(for: Self.hexCallId)!
        controller.provider(anyProvider(), perform: CXAnswerCallAction(call: videoUUID))

        XCTAssertEqual(
            controller.audioConfigurationForActiveCall(), .videoCall,
            "didActivate must route for the answered video call, not the later-reported audio call")
    }

    func testAudioConfiguration_answeredAudioCallUsesAudioConfig() {
        let controller = SilaCallKitController()
        controller.reportIncomingCall(callId: Self.hexCallId, callerName: "Alice", hasVideo: false)
        let uuid = SilaCallKitController.callKitUUID(for: Self.hexCallId)!
        controller.provider(anyProvider(), perform: CXAnswerCallAction(call: uuid))

        XCTAssertEqual(controller.audioConfigurationForActiveCall(), .audioCall)
    }

    // MARK: - Native-ring foreground gate (M-NATIVE-4 = NR-6, Step 12)

    func testShouldRingCallKit_foregroundSuppressesBackgroundFires() {
        XCTAssertFalse(
            SilaCallDecision.shouldRingCallKit(appForeground: true),
            "a foreground call must suppress the CallKit ring (the in-app overlay owns it)"
        )
        XCTAssertTrue(
            SilaCallDecision.shouldRingCallKit(appForeground: false),
            "a backgrounded/locked call must fire the native CallKit ring"
        )
    }

    func testSuspendedIncomingRingSupported_isFalseOnIos() {
        XCTAssertFalse(
            SilaCallDecision.suspendedIncomingRingSupported,
            "iOS must NOT claim it can ring a suspended/terminated app (no PushKit/APNs in v1)"
        )
    }

    // MARK: - Device audio route (M-NATIVE-3 / CCF-11, Step 11)

    func testAudioRoute_speakerForcesLoudspeaker() {
        XCTAssertEqual(SilaAudioRoute.speaker.portOverride, .speaker)
    }

    func testAudioRoute_earpieceOverridesToNoneReceiver() {
        XCTAssertEqual(SilaAudioRoute.earpiece.portOverride, AVAudioSession.PortOverride.none)
    }

    func testAudioRoute_bluetoothAndSystemDeferToPlatform() {
        // BT selection + the system default are platform/CallKit-owned → no override.
        XCTAssertNil(SilaAudioRoute.bluetooth.portOverride)
        XCTAssertNil(SilaAudioRoute.system.portOverride)
    }

    func testAudioRoute_parsesWireTokens() {
        XCTAssertEqual(SilaAudioRoute(rawValue: "speaker"), .speaker)
        XCTAssertEqual(SilaAudioRoute(rawValue: "earpiece"), .earpiece)
        XCTAssertEqual(SilaAudioRoute(rawValue: "bluetooth"), .bluetooth)
        XCTAssertEqual(SilaAudioRoute(rawValue: "system"), .system)
        XCTAssertNil(SilaAudioRoute(rawValue: "bogus"))
    }
}
