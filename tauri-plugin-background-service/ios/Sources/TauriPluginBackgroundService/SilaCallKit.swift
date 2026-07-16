import Foundation
import CallKit
import AVFoundation
import os.log

// MARK: - F3 degraded-mode decision (pure, unit-tested)

/// The iOS app lifecycle states relevant to delivering an inbound call (spec 08 §F3).
///
/// F3: ringing works only while the app is foreground, in the short background-active
/// window, or during a live `BGTask`. A suspended or force-quit app cannot be woken by an
/// inbound QUIC packet, so v1 (no APNs push relay) gives the caller a fast `Unreachable`
/// (offer-ack timeout T1, Step 4) and queues a missed-call notice through the control
/// outbox (Step 5) for delivery on the next wake.
enum SilaAppCallState: String, Equatable {
    /// App visible; report the call to CallKit (system ring + in-call UI).
    case foreground
    /// Short background-active window / live BGTask; report the call to CallKit.
    case backgroundActive
    /// Cannot be woken by inbound QUIC; do NOT ring. Caller gets `Unreachable` + a
    /// missed-call control-outbox record drained on the next wake.
    case suspended
}

/// What the iOS side does with an inbound call offer, given the app's lifecycle state.
enum SilaCallDeliveryAction: Equatable {
    /// Report the call to CallKit so the system rings and shows the in-call UI.
    case ring(hasVideo: Bool)
    /// App is suspended: skip CallKit entirely. The caller's T1 yields `Unreachable` and a
    /// missed-call record is drained from the control outbox on the next wake.
    case deferToControlOutbox
}

/// Pure F3(a)+(c) decision logic for iOS call delivery.
///
/// Mirrors the Kotlin/Android fast-path but expresses iOS's honest constraint: there is no
/// push relay in v1, so a suspended app simply cannot ring. Decoupled from `CXProvider` /
/// UIKit so it is unit-testable without the iOS runtime (spec-01 seam philosophy).
enum SilaCallDecision {
    static func deliveryAction(appState: SilaAppCallState, offerHasVideo: Bool) -> SilaCallDeliveryAction {
        switch appState {
        case .foreground, .backgroundActive:
            return .ring(hasVideo: offerHasVideo)
        case .suspended:
            return .deferToControlOutbox
        }
    }

    /// Step 12 (M-NATIVE-4 = NR-6): native-ring foreground gate — the iOS twin of
    /// the Rust `event_bridge::should_ring_native`.
    ///
    /// One ring owner per app-state (DEC-060 option a — "UI owns foreground,
    /// native owns background/locked/closed"): when the webview is
    /// **foreground/visible** the in-app `IncomingCallScreen` overlay owns the
    /// ring, so the CallKit ring is **suppressed** (no double-ring); when
    /// **backgrounded/locked** the native CallKit ring fires.
    ///
    /// In production the Rust gate already suppresses `notify_mobile_incoming_call`
    /// (which drives this CallKit path) before it is reached when the webview is
    /// foreground, so this is the explicit iOS-side decision / defense-in-depth and
    /// the seam a future PushKit/APNs wake path (F3(b)) would consult. Host-build
    /// deferred (no macOS toolchain) — written + unit-shaped, recorded NOT done.
    static func shouldRingCallKit(appForeground: Bool) -> Bool {
        return !appForeground
    }

    /// Step 13 (M-NATIVE-5 = CCF-14/NR-5): the iOS twin of the Rust
    /// `sila_core::call_capabilities("ios").suspended_incoming_ring_supported`.
    ///
    /// **`false` on iOS, honestly.** Step 17 declares the `voip` UIBackgroundMode
    /// and Step 18 Task B lands the on-device `PKPushRegistry`
    /// (`BackgroundServicePlugin` → `sila_set_push_token`), but the APNs **relay**
    /// that actually delivers a VoIP push to a suspended app is an EXTERNAL
    /// dependency (Fork 2) and not yet device-verified. Until it is, a suspended
    /// app gets the documented missed-call path
    /// (`SilaCallDeliveryAction.deferToControlOutbox`), never a reliable ring —
    /// so this must never be presented as native incoming-call parity. The
    /// registry-vs-relay distinction is observable via
    /// `MobilePackagingValidation.push_relay_configured` (token registered, NOT
    /// delivery verified). Host-build deferred (no macOS toolchain) — written +
    /// unit-shaped, recorded NOT done.
    static let suspendedIncomingRingSupported: Bool = false
}

// MARK: - Audio-session configuration (pure value, unit-tested)

/// Pure description of the desired `AVAudioSession` configuration for a call.
///
/// Decoupled from `AVAudioSession` so the routing choice is unit-testable; applied by
/// `SilaCallKitController` inside `CXProviderDelegate.didActivate`.
struct SilaAudioSessionConfiguration: Equatable {
    let category: String
    let mode: String
    /// Route to the speaker (default for video; CallKit owns routing for audio).
    let defaultToSpeaker: Bool
    /// Allow Bluetooth A2DP output (headphones/speaker).
    let allowsBluetoothA2DP: Bool

    /// VOIP audio call: `playAndRecord` + `voiceChat`. CallKit's `didActivate` owns routing,
    /// so `defaultToSpeaker` is false (honor the system call UI / earpiece).
    static let audioCall = SilaAudioSessionConfiguration(
        category: "AVAudioSessionCategoryPlayAndRecord",
        mode: "AVAudioSessionModeVoiceChat",
        defaultToSpeaker: false,
        allowsBluetoothA2DP: true
    )

    /// Video call: `playAndRecord` + `videoChat`, default to speaker — speakerphone is the
    /// sensible default when the camera is active and the screen is away from the ear.
    static let videoCall = SilaAudioSessionConfiguration(
        category: "AVAudioSessionCategoryPlayAndRecord",
        mode: "AVAudioSessionModeVideoChat",
        defaultToSpeaker: true,
        allowsBluetoothA2DP: true
    )

    static func forCall(hasVideo: Bool) -> SilaAudioSessionConfiguration {
        hasVideo ? .videoCall : .audioCall
    }
}

/// Device audio route for an active call (M-NATIVE-3 / CCF-11, Step 11) — mirrors
/// the Rust `CallAudioRoute` / Android `audioRouteFor`. The routing choice is
/// decoupled from `AVAudioSession` so it is unit-testable like
/// `SilaAudioSessionConfiguration`; applied by `SilaCallKitController.setAudioRoute`.
enum SilaAudioRoute: String {
    case speaker
    case earpiece
    case bluetooth
    case system

    /// Pure routing-choice → `AVAudioSession` output-port override:
    /// - `.speaker` forces the loudspeaker (`.speaker`).
    /// - `.earpiece` forces the receiver (override `.none`).
    /// - `.bluetooth` / `.system` defer to the platform/CallKit route — no override
    ///   (`nil`); BT selection is owned by the system route picker.
    var portOverride: AVAudioSession.PortOverride? {
        switch self {
        case .speaker: return .speaker
        case .earpiece: return AVAudioSession.PortOverride.none
        case .bluetooth, .system: return nil
        }
    }
}

// MARK: - Rust direct-FFI bridge (BGS-10 + BGS-08, doc-08 Step 18 Task B)

/// Swift bindings for the iOS C-FFI `extern "C"` entries in
/// `tauri/src/ios_ffi.rs`.
///
/// CallKit perform-handlers route Answer/Reject/End DIRECTLY to the Rust control
/// plane through these symbols — not through the suspended webview — so a
/// lock-screen action reaches the running Core even while the app is backgrounded
/// (BGS-10: the webview bridge `onCallEvent → trigger → useNativeCallActions.ts
/// → Tauri cmd` is suspended exactly when CallKit rings, so a webview-routed
/// lock-screen answer may never connect). The symbols resolve at app link time
/// from the Rust cdylib (`sila_lib`).
///
/// Host-validation (no macOS toolchain on this host — written + runbook-gated,
/// xcodebuild iOS-Simulator in Step 21): the `sila_call_action` routing is
/// pinned by `bgs10_native_accept_drives_fsm` and the PushKit token shims by
/// `bgs08_pushkit_ffi_shim_plumbed` (`tauri/src/ios_ffi.rs` + `headless_core.rs`).
///
/// CROSS-DOC: doc 04 owns call-control semantics (`answer_call`/`reject_call`/
/// `end_call`); these bindings are the iOS transport, mirroring the Android JNI
/// `callAction` single-symbol contract.
enum SilaNativeFFI {
    /// `sila_call_action(call_id, action) -> *mut c_char` — route an
    /// `"answer"`/`"reject"`/`"end"` action for `call_id` to
    /// `headless_core::call_action` → the Core control plane. Returns an OWNED
    /// JSON report the caller MUST reclaim via `silaFreeString` (a
    /// `CString::into_raw` allocation — Rust allocator-matched, not Swift's).
    ///
    /// Call-context precondition (documented on the Rust entry): invoke from a
    /// thread with NO ambient tokio runtime. CallKit perform-handlers run on the
    /// `CXProvider` queue (no ambient runtime — safe); a future tokio-spawned
    /// re-entry would panic (panic=abort aborts the process).
    @_silgen_name("sila_call_action")
    private static func silaCallAction(
        _ callId: UnsafePointer<CChar>?,
        _ action: UnsafePointer<CChar>?
    ) -> UnsafeMutablePointer<CChar>?

    /// `sila_free_string(ptr)` — reclaim a report C string returned by
    /// `silaCallAction`. No-op on null. Must be called exactly once per
    /// returned pointer.
    @_silgen_name("sila_free_string")
    private static func silaFreeString(_ ptr: UnsafeMutablePointer<CChar>?)

    /// `sila_set_push_token(token)` — persist the PushKit device token into the
    /// doc-08 token store (null/empty clears). Used by `BackgroundServicePlugin`'s
    /// `PKPushRegistry` delegate.
    @_silgen_name("sila_set_push_token")
    static func silaSetPushToken(_ token: UnsafePointer<CChar>?)

    /// `sila_clear_push_token()` — clear the stored PushKit token
    /// (invalidate/logout).
    @_silgen_name("sila_clear_push_token")
    static func silaClearPushToken()

    /// Default production bridge: route a CallKit perform action to Rust and
    /// free the returned report (the perform-handlers do not consume it). Exposed
    /// so `SilaCallKitController.performCallAction` can default to it while
    /// XCTest injects a recorder — asserting the perform→action mapping without a
    /// live Core.
    static func bridgeCallAction(callId: String, action: String) {
        callId.withCString { cCallId in
            action.withCString { cAction in
                if let raw = silaCallAction(cCallId, cAction) {
                    silaFreeString(raw)
                }
            }
        }
    }
}

// MARK: - CallKit + AVAudioSession wrapper (runtime glue; device-tested)

/// CallKit + `AVAudioSession` wrapper for Sila calls (spec 08 C6, Step 16).
///
/// The headless Rust core signals an incoming call via the Tauri mobile-plugin invoke
/// `showIncomingCall` (routed by `MobileLifecycle::show_incoming_call`); this controller
/// reports it to CallKit (system ring + in-call UI) and configures the VOIP audio session.
///
/// Degraded mode (F3): because there is no APNs push relay in v1, this path is only
/// reached while the app is foreground/background-active — exactly `SilaCallDecision`'s
/// `.ring` arm. A suspended app is never woken to reach this path; the caller instead gets
/// `Unreachable` + a missed-call control-outbox record. The PushKit/APNs relay (F3(b)) is
/// the documented, explicitly-deferred path; since iOS 13 a VoIP push must report to
/// CallKit, so this controller is a prerequisite for any future push anyway.
final class SilaCallKitController: NSObject, CXProviderDelegate {

    private static let logger = OSLog(subsystem: "social.sila", category: "CallKit")

    private let provider: CXProvider
    private let callController: CXCallController
    private var activeCalls: [UUID: Bool] = [:]  // uuid → hasVideo

    /// Number of calls currently reported to CallKit. Internal (not private) so the
    /// XCTest target can assert the foreground-ring gate suppressed/allowed a report
    /// — mirrors the seam-visibility style used elsewhere (e.g. `completeOnce`).
    var activeCallCount: Int { activeCalls.count }

    /// CallKit UUID → original 32-hex Rust `call_id` (H7). Populated in
    /// `reportIncomingCall`, reverse-looked-up by the perform-handlers so the
    /// Swift→Rust bridge carries the ORIGINAL session key (not the derived UUID),
    /// and cleared on `endCall`/teardown/reset.
    private var callIdsByUUID: [UUID: String] = [:]

    /// CallKit UUIDs that have been answered (CXAnswerCallAction fulfilled). Lets a
    /// subsequent CXEndCallAction distinguish a hang-up of a live call
    /// (`call_ended`) from a decline of a still-ringing call (`call_rejected`).
    private var answeredCallUUIDs: Set<UUID> = []

    /// The call whose audio session is (about to be) active — set when an answer is
    /// performed. `didActivate` derives `hasVideo` from this call's `activeCalls`
    /// entry (L6) instead of a clobber-prone scalar.
    private var activeAudioCallUUID: UUID?

    /// Deprecated fallback seam for the CallKit→webview event path (H7). The
    /// perform-handlers now route Answer/Reject/End DIRECTLY to Rust via
    /// `performCallAction` (BGS-10: the webview is suspended when CallKit
    /// rings, so a webview-routed lock-screen answer may never connect), so this
    /// closure is NO LONGER fired by the perform-handlers. Retained for a future
    /// webview-foreground-answer path; currently dormant for answer/reject/end.
    /// The plugin still wires it (`BackgroundServicePlugin.callKitController`);
    /// the webview consumer (`useNativeCallActions.ts`) is now dead code for
    /// these actions (carry-forward cleanup — no merge concern: no sibling branch
    /// touches it).
    var onCallEvent: ((_ event: String, _ callId: String) -> Void)?

    /// The active CallKit→Rust call-action bridge (BGS-10, Step 18 Task B).
    /// Defaults to the real FFI (`SilaNativeFFI.bridgeCallAction`); XCTest
    /// swaps in a recorder to assert the perform→action mapping without a live
    /// Core. The perform-handlers invoke this with `("answer" | "reject" | "end")`
    /// + the ORIGINAL 32-hex `call_id`, mirroring the Android JNI `callAction`
    /// single-symbol contract.
    var performCallAction: (String, String) -> Void = { callId, action in
        SilaNativeFFI.bridgeCallAction(callId: callId, action: action)
    }

    override init() {
        self.callController = CXCallController()
        self.provider = CXProvider(configuration: SilaCallKitController.providerConfiguration())
        super.init()
        provider.setDelegate(self, queue: nil)
    }

    /// CallKit provider configuration for self-managed VOIP calls.
    static func providerConfiguration() -> CXProviderConfiguration {
        let config = CXProviderConfiguration()
        config.maximumCallGroups = 1
        config.maximumCallsPerCallGroup = 1
        config.supportsVideo = true
        config.supportedHandleTypes = [.generic]
        config.includesCallsInRecents = false
        return config
    }

    // MARK: - Call-id → CallKit UUID derivation (pure, unit-tested)

    /// Deterministically derive the CallKit `UUID` for a Rust call session key.
    ///
    /// `call_id` is a 32-char lowercase hex string (exactly 128 bits) minted by
    /// `core::call_manager::new_call_id` — it is NOT an RFC-4122 UUID string, so
    /// `UUID(uuidString:)` always returns `nil` for it (the prior code's
    /// `UUID(uuidString: callId) ?? UUID()` drew an INDEPENDENT random UUID on every
    /// call, so `endCall` could never dismiss the call `reportIncomingCall` had
    /// reported). Parsing the 16 raw bytes guarantees both sites derive the SAME
    /// `UUID` for a given call: CallKit dismisses the ring only when the end is
    /// reported against the UUID the call was reported with. Mirrors the Android
    /// fast-path, which keys `show`/`cancel` deterministically off the opaque string
    /// (`IncomingCallNotifier.notificationIdFor(callId) = callId.hashCode()`).
    /// Returns `nil` for a malformed (non-32-hex) id; callers fall back to a random
    /// `UUID` only in that defensive case.
    static func callKitUUID(for callId: String) -> UUID? {
        guard callId.count == 32 else { return nil }
        var bytes = [UInt8](repeating: 0, count: 16)
        var idx = callId.startIndex
        for i in 0..<16 {
            let next = callId.index(idx, offsetBy: 2)
            guard let byte = UInt8(callId[idx..<next], radix: 16) else { return nil }
            bytes[i] = byte
            idx = next
        }
        return UUID(uuid: (bytes[0], bytes[1], bytes[2], bytes[3],
                           bytes[4], bytes[5], bytes[6], bytes[7],
                           bytes[8], bytes[9], bytes[10], bytes[11],
                           bytes[12], bytes[13], bytes[14], bytes[15]))
    }

    /// Whether `callId` is the contract-shaped 32-char lowercase-hex Rust session
    /// key (L5). Reuses the same parse as `callKitUUID(for:)` so a value that
    /// validates here is guaranteed to derive a deterministic, non-fallback UUID.
    /// The plugin's `showIncomingCall`/`cancelIncomingCall` reject anything else so
    /// a malformed id never rings (and never strands a random-UUID call CallKit can
    /// never dismiss).
    static func isValidCallId(_ callId: String) -> Bool {
        return callKitUUID(for: callId) != nil
    }

    /// Report an incoming call to CallKit (system ring + in-call UI).
    ///
    /// `callerName` is the resolved display name. The webview `IncomingCallScreen` (Step 13)
    /// resolves it independently via `getByChatKey`; CallKit surfaces it on the native
    /// lock/call screen. `callId` is the Rust call session key (§3.3) — deterministically
    /// mapped to the CallKit UUID via `callKitUUID(for:)` so `endCall` can dismiss the same
    /// call; falls back to a random UUID only for a malformed id.
    func reportIncomingCall(callId: String, callerName: String, hasVideo: Bool) {
        let derived = SilaCallKitController.callKitUUID(for: callId)
        if derived == nil {
            os_log("reportIncomingCall: malformed call_id, using random UUID fallback",
                   log: SilaCallKitController.logger, type: .error)
        }
        let uuid = derived ?? UUID()
        let update = CXCallUpdate()
        update.remoteHandle = CXHandle(type: .generic, value: callerName)
        update.hasVideo = hasVideo
        update.localizedCallerName = callerName
        update.supportsHolding = false       // Held state is v1.1 (spec §3.3)
        update.supportsGrouping = false
        update.supportsUngrouping = false
        update.supportsDTMF = false
        activeCalls[uuid] = hasVideo
        // H7: remember the original session key so the perform-handlers can bridge
        // it back to Rust (the derived/fallback UUID is not the session key).
        callIdsByUUID[uuid] = callId
        provider.reportNewIncomingCall(with: uuid, update: update) { error in
            if let error = error {
                os_log("reportNewIncomingCall failed: %{public}@",
                       log: SilaCallKitController.logger, type: .error, "\(error)")
            }
        }
    }

    /// End a call reported to CallKit (caller hung up / answered-elsewhere / rejected).
    ///
    /// Derives the UUID with the SAME `callKitUUID(for:)` as `reportIncomingCall` so the
    /// end is reported against the call CallKit actually has (otherwise the native ring
    /// screen would persist until `providerDidReset` and `activeCalls` would leak).
    func endCall(callId: String, reason: CXCallEndedReason) {
        let uuid = SilaCallKitController.callKitUUID(for: callId) ?? UUID()
        provider.reportCall(with: uuid, endedAt: Date(), reason: reason)
        forgetCall(uuid)
    }

    /// Drop all per-call bookkeeping for a CallKit UUID (call ended/declined/reset).
    private func forgetCall(_ uuid: UUID) {
        activeCalls.removeValue(forKey: uuid)
        callIdsByUUID.removeValue(forKey: uuid)
        answeredCallUUIDs.remove(uuid)
        if activeAudioCallUUID == uuid { activeAudioCallUUID = nil }
    }

    // MARK: - CXProviderDelegate

    func providerDidReset(_ provider: CXProvider) {
        activeCalls.removeAll()
        callIdsByUUID.removeAll()
        answeredCallUUIDs.removeAll()
        activeAudioCallUUID = nil
    }

    /// User tapped Answer in the native CallKit UI (H7). Bridge the answer to Rust
    /// carrying the ORIGINAL 32-hex `call_id`, then `fulfill()`. An action for a call
    /// we never reported (no map entry) is failed without bridging — there is no
    /// session key to carry.
    func provider(_ provider: CXProvider, perform action: CXAnswerCallAction) {
        guard let callId = callIdsByUUID[action.callUUID] else {
            action.fail()
            return
        }
        answeredCallUUIDs.insert(action.callUUID)
        // The answered call owns the audio session; `didActivate` reads its video
        // flag from `activeCalls` (L6).
        activeAudioCallUUID = action.callUUID
        action.fulfill()
        // BGS-10 (Step 18 Task B): route Answer DIRECTLY to the Rust control plane
        // via `sila_call_action` (FFI), not the suspended webview. CROSS-DOC: doc
        // 04 owns call-control semantics (answer_call/reject_call/end_call).
        performCallAction(callId, "answer")
    }

    /// User tapped Decline (still ringing) or End (live call) in the native CallKit
    /// UI (H7). Bridge to Rust as `call_rejected` (never answered) or `call_ended`
    /// (was answered) carrying the original `call_id`, then `fulfill()`. An action
    /// for an unknown call is failed without bridging.
    func provider(_ provider: CXProvider, perform action: CXEndCallAction) {
        guard let callId = callIdsByUUID[action.callUUID] else {
            action.fail()
            return
        }
        let wasAnswered = answeredCallUUIDs.contains(action.callUUID)
        forgetCall(action.callUUID)
        action.fulfill()
        // BGS-10 (Step 18 Task B): Decline of a still-ringing call → "reject"; an
        // End of a live (answered) call → "end". Routed DIRECTLY to Rust via
        // `sila_call_action` (FFI), not the suspended webview.
        performCallAction(callId, wasAnswered ? "end" : "reject")
    }

    /// CallKit activated the audio session — configure VOIP routing (F3) for the
    /// answered call, deriving `hasVideo` from `activeCalls` (L6) so an interleaved
    /// second report can no longer clobber the routing of the call being activated.
    func provider(_ provider: CXProvider, didActivate audioSession: AVAudioSession) {
        SilaCallKitController.configure(
            audioSession: audioSession,
            configuration: audioConfigurationForActiveCall()
        )
    }

    /// The audio-session configuration for the call whose audio is being activated.
    /// Prefers the answered call's `activeCalls` entry; falls back to any single
    /// active call (defensive — `maximumCallsPerCallGroup == 1`), else audio.
    /// Separated from `didActivate` so the per-call video derivation (L6) is
    /// unit-testable without a live `AVAudioSession`.
    func audioConfigurationForActiveCall() -> SilaAudioSessionConfiguration {
        let hasVideo = activeAudioCallUUID.flatMap { activeCalls[$0] }
            ?? activeCalls.values.first
            ?? false
        return .forCall(hasVideo: hasVideo)
    }

    func provider(_ provider: CXProvider, didDeactivate audioSession: AVAudioSession) {
        // No-op: CallKit owns session deactivation; the OS restores the pre-call category
        // on full call teardown.
    }

    // MARK: - Audio-session configuration (separated for isolation)

    /// Apply a `SilaAudioSessionConfiguration` to a real `AVAudioSession`.
    static func configure(audioSession: AVAudioSession, configuration: SilaAudioSessionConfiguration) {
        let category = AVAudioSession.Category(rawValue: configuration.category)
        let mode = AVAudioSession.Mode(rawValue: configuration.mode)
        var options: AVAudioSession.CategoryOptions = [.allowBluetoothHFP]
        if configuration.defaultToSpeaker { options.insert(.defaultToSpeaker) }
        if configuration.allowsBluetoothA2DP { options.insert(.allowBluetoothA2DP) }
        do {
            try audioSession.setCategory(category, mode: mode, options: options)
            try audioSession.setActive(true, options: [])
        } catch {
            os_log("audio session config failed: %{public}@",
                   log: SilaCallKitController.logger, type: .error, "\(error)")
        }
    }

    /// Apply a device audio route (M-NATIVE-3 / CCF-11, Step 11) to the live call via
    /// `AVAudioSession.overrideOutputAudioPort`. `.bluetooth`/`.system` are
    /// platform-managed (no override); the system route picker owns BT selection.
    func setAudioRoute(_ route: SilaAudioRoute) {
        guard let override = route.portOverride else {
            os_log("audio route %{public}@ is platform-managed (no override)",
                   log: SilaCallKitController.logger, type: .info, route.rawValue)
            return
        }
        do {
            try AVAudioSession.sharedInstance().overrideOutputAudioPort(override)
            os_log("set audio route %{public}@",
                   log: SilaCallKitController.logger, type: .info, route.rawValue)
        } catch {
            os_log("overrideOutputAudioPort failed: %{public}@",
                   log: SilaCallKitController.logger, type: .error, "\(error)")
        }
    }
}
