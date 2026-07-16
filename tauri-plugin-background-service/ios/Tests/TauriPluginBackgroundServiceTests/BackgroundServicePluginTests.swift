import XCTest
@testable import tauri_plugin_background_service

/// Real plugin-behavior tests that don't fit a step-specific suite: the pure D2
/// adaptive-schedule policy, `TaskOutcome` parsing, and the L5 callId/callerName
/// hygiene on `showIncomingCall`/`cancelIncomingCall`. Each one exercises actual
/// plugin code (a static function, an initializer, or a command driven through the
/// `InvokeCapture` seam).
///
/// Step 18 / H11 purged this file's former UserDefaults-echo-only and commented-out
/// pseudo tests (they only set/read `UserDefaults` or a local closure without
/// touching plugin code, giving false confidence). The behaviors they gestured at
/// are proven for real elsewhere: desired-state persistence in
/// `DesiredStateMirrorTests`, the split scheduling status in
/// `SchedulingStatusContractTests` / `StatusFactTests`, pending-task lifecycle in
/// `PendingTaskLifecycleTests`, schedule-error hygiene in `CompletionHygieneTests`,
/// and the exactly-once completion guard in `ExactlyOnceCompletionTests`.
final class BackgroundServicePluginTests: XCTestCase {

    // MARK: - Adaptive Processing Schedule (pure function, D2)

    /// Convenience wrapper: most cases only vary kind/outcome/previous/multiplier.
    private func adaptive(
        configured: Double = 15.0,
        ceilingMultiplier: Double = 4.0,
        lastTaskKind: String? = "processing",
        lastOutcome: BackgroundServicePlugin.TaskOutcome,
        previous: Double
    ) -> Double {
        return BackgroundServicePlugin.adaptiveProcessingBeginMinutes(
            configured: configured,
            ceilingMultiplier: ceilingMultiplier,
            lastStartedAt: nil,
            lastCompletedAt: nil,
            lastTaskKind: lastTaskKind,
            lastOutcome: lastOutcome,
            previous: previous
        )
    }

    func testAdaptive_expiredBacksOff() {
        // Expired run → back off 1.5×: 15 * 1.5 = 22.5, well below the 60 ceiling
        let result = adaptive(lastOutcome: .expired, previous: 15.0)
        XCTAssertEqual(result, 22.5, accuracy: 0.0001)
    }

    func testAdaptive_expiredClampsToCeiling() {
        // 50 * 1.5 = 75 exceeds the ceiling (15 * 4 = 60) → clamp
        let result = adaptive(lastOutcome: .expired, previous: 50.0)
        XCTAssertEqual(result, 60.0, accuracy: 0.0001)
    }

    func testAdaptive_completedTightens() {
        // Completed run → tighten 1.5×: 45 / 1.5 = 30, above the 15 floor
        let result = adaptive(lastOutcome: .completedNaturally, previous: 45.0)
        XCTAssertEqual(result, 30.0, accuracy: 0.0001)
    }

    func testAdaptive_completedClampsToFloor() {
        // 18 / 1.5 = 12 is below the configured floor (15) → clamp
        let result = adaptive(lastOutcome: .completedNaturally, previous: 18.0)
        XCTAssertEqual(result, 15.0, accuracy: 0.0001)
    }

    func testAdaptive_unknownHolds() {
        // Unknown outcome → previous value held unchanged
        let result = adaptive(lastOutcome: .unknown, previous: 30.0)
        XCTAssertEqual(result, 30.0, accuracy: 0.0001)
    }

    func testAdaptive_noPreviousUsesConfigured() {
        // No persisted adaptive value: caller passes configured as previous;
        // unknown outcome holds it → configured comes back out.
        let result = adaptive(lastOutcome: .unknown, previous: 15.0)
        XCTAssertEqual(result, 15.0, accuracy: 0.0001)
    }

    func testAdaptive_refreshKindDoesNotMove() {
        // A refresh-kind run never moves the processing value, even on expiry
        let result = adaptive(lastTaskKind: "refresh", lastOutcome: .expired, previous: 30.0)
        XCTAssertEqual(result, 30.0, accuracy: 0.0001)
    }

    func testAdaptive_nilKindDoesNotMove() {
        // No recorded task kind → hold previous
        let result = adaptive(lastTaskKind: nil, lastOutcome: .expired, previous: 30.0)
        XCTAssertEqual(result, 30.0, accuracy: 0.0001)
    }

    // MARK: - Adaptive Guards (clamp + NaN, mandated by Step 6 review)

    func testAdaptive_multiplierBelowOneClampsCeilingToConfigured() {
        // multiplier <= 1 would put the ceiling below the floor; the effective
        // ceiling must clamp to configured. Expired from the floor stays at it.
        let result = adaptive(ceilingMultiplier: 0.5, lastOutcome: .expired, previous: 15.0)
        XCTAssertEqual(result, 15.0, accuracy: 0.0001)
    }

    func testAdaptive_multiplierExactlyOneClampsCeilingToConfigured() {
        let result = adaptive(ceilingMultiplier: 1.0, lastOutcome: .expired, previous: 15.0)
        XCTAssertEqual(result, 15.0, accuracy: 0.0001)
    }

    func testAdaptive_negativeMultiplierClampsCeilingToConfigured() {
        let result = adaptive(ceilingMultiplier: -3.0, lastOutcome: .expired, previous: 40.0)
        XCTAssertEqual(result, 15.0, accuracy: 0.0001)
    }

    func testAdaptive_nanMultiplierClampsCeilingToConfigured() {
        // NaN must not poison the persisted adaptive value
        let result = adaptive(ceilingMultiplier: .nan, lastOutcome: .expired, previous: 15.0)
        XCTAssertEqual(result, 15.0, accuracy: 0.0001)
    }

    func testAdaptive_nanPreviousFallsBackToConfigured() {
        // NaN previous is treated as "no previous" → start from configured
        let result = adaptive(lastOutcome: .expired, previous: .nan)
        XCTAssertEqual(result, 22.5, accuracy: 0.0001)
    }

    func testAdaptive_infinitePreviousFallsBackToConfigured() {
        let result = adaptive(lastOutcome: .expired, previous: .infinity)
        XCTAssertEqual(result, 22.5, accuracy: 0.0001)
    }

    func testAdaptive_nonPositivePreviousFallsBackToConfigured() {
        let result = adaptive(lastOutcome: .unknown, previous: -7.0)
        XCTAssertEqual(result, 15.0, accuracy: 0.0001)
    }

    func testAdaptive_nanConfiguredFallsBackToDefault() {
        // Invalid configured floor falls back to the 15-minute scheduler default
        let result = adaptive(configured: .nan, lastOutcome: .expired, previous: 15.0)
        XCTAssertEqual(result, 22.5, accuracy: 0.0001)
    }

    func testAdaptive_zeroConfiguredFallsBackToDefault() {
        let result = adaptive(configured: 0.0, lastOutcome: .unknown, previous: 15.0)
        XCTAssertEqual(result, 15.0, accuracy: 0.0001)
    }

    func testAdaptive_allInvalidInputsStillFiniteResult() {
        // Worst case: every numeric input invalid — result must stay finite
        // and land on the default floor (NaN multiplier collapses the ceiling
        // to the floor), never NaN.
        let result = adaptive(
            configured: .nan, ceilingMultiplier: .nan,
            lastOutcome: .expired, previous: .nan
        )
        XCTAssertTrue(result.isFinite)
        XCTAssertEqual(result, 15.0, accuracy: 0.0001)
    }

    // MARK: - Task Outcome Parsing

    func testTaskOutcome_parsesCompleted() {
        XCTAssertEqual(BackgroundServicePlugin.TaskOutcome(persisted: "completed"), .completedNaturally)
    }

    func testTaskOutcome_parsesExpired() {
        XCTAssertEqual(BackgroundServicePlugin.TaskOutcome(persisted: "expired"), .expired)
    }

    func testTaskOutcome_nilIsUnknown() {
        XCTAssertEqual(BackgroundServicePlugin.TaskOutcome(persisted: nil), .unknown)
    }

    func testTaskOutcome_unrecognizedIsUnknown() {
        XCTAssertEqual(BackgroundServicePlugin.TaskOutcome(persisted: "garbage"), .unknown)
    }

    // MARK: - L5: callId / callerName hygiene in showIncomingCall / cancelIncomingCall

    private static let validCallId = "018f3a2b5c4d6e7f019e4b6c7d8e9f0a"

    func testShowIncomingCall_rejectsEmptyCallId() {
        let plugin = BackgroundServicePlugin()
        let capture = InvokeCapture()
        plugin.showIncomingCall(
            capture.makeInvoke(args: "{\"callId\":\"\",\"callerName\":\"Alice\",\"isVideo\":false}"))
        XCTAssertEqual(capture.rejectCount, 1, "empty callId never rings")
        XCTAssertEqual(capture.resolveCount, 0)
    }

    func testShowIncomingCall_rejectsNonHexCallId() {
        let plugin = BackgroundServicePlugin()
        let capture = InvokeCapture()
        // 32 chars but not hex.
        plugin.showIncomingCall(
            capture.makeInvoke(args: "{\"callId\":\"zzzz3a2b5c4d6e7f019e4b6c7d8e9f0a\",\"callerName\":\"Alice\",\"isVideo\":false}"))
        XCTAssertEqual(capture.rejectCount, 1, "non-hex callId never rings")
        XCTAssertEqual(capture.resolveCount, 0)
    }

    func testShowIncomingCall_rejectsEmptyCallerName() {
        let plugin = BackgroundServicePlugin()
        let capture = InvokeCapture()
        plugin.showIncomingCall(
            capture.makeInvoke(args: "{\"callId\":\"\(Self.validCallId)\",\"callerName\":\"\",\"isVideo\":false}"))
        XCTAssertEqual(capture.rejectCount, 1, "empty callerName is rejected")
        XCTAssertEqual(capture.resolveCount, 0)
    }

    func testCancelIncomingCall_rejectsNonHexCallId() {
        let plugin = BackgroundServicePlugin()
        let capture = InvokeCapture()
        plugin.cancelIncomingCall(
            capture.makeInvoke(args: "{\"callId\":\"not-a-valid-id\"}"))
        XCTAssertEqual(capture.rejectCount, 1, "malformed callId on cancel is rejected")
        XCTAssertEqual(capture.resolveCount, 0)
    }
}
