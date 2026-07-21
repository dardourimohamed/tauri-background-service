import XCTest
import PushKit
@testable import tauri_plugin_background_service

/// IOS-PUSH-01: the plugin ships no PushKit surface — no `import PushKit`, no
/// `PKPushRegistryDelegate` conformance, no `PKPushRegistry` property, and none
/// of the three required delegate methods. v1 has no APNs/VoIP-push relay, so a
/// PushKit registry would (a) require an entitlement/mode the host may not hold
/// and (b) sink tokens into an inaccessible property. The honest active-process
/// CallKit-only position is locked in by `BackgroundCallDecision
/// .suspendedIncomingRingSupported == false` (covered in `BackgroundCallKitTests`).
///
/// These tests would FAIL on the pre-fix source: the class declared
/// `PKPushRegistryDelegate` conformance, `import PushKit`, `pushRegistry`, and
/// the three delegate methods.
final class PushKitRemovalTests: XCTestCase {

    /// The plugin must NOT declare `PKPushRegistryDelegate` conformance. Apple
    /// rejects a binary that declares `voip` UIBackgroundMode without a
    /// registry reporting pushes; the inverse (a registry with no relay to feed
    /// it) strands the host at submission and the runtime kills future pushes.
    /// Removing the conformance is the load-bearing change — the rest follows.
    func testPluginClass_doesNotConformToPKPushRegistryDelegate() {
        XCTAssertFalse(
            BackgroundServicePlugin.conforms(to: PKPushRegistryDelegate.self),
            "BackgroundServicePlugin must NOT conform to PKPushRegistryDelegate (IOS-PUSH-01). " +
            "v1 ships no APNs/VoIP relay; an idle PushKit registry would strand the host.")
    }

    /// The three required `PKPushRegistryDelegate` methods must be absent from
    /// the plugin's ObjC dispatch table. `responds(to:)` returns true for any
    /// `@objc` method the class declares (inherited or own); the pre-fix class
    /// implemented all three, so each selector answered `true`. Post-fix each
    /// must answer `false`.
    func testPluginInstance_doesNotRespondToPushKitDelegateSelectors() {
        let plugin = BackgroundServicePlugin()
        let selectors = [
            "pushRegistry:didUpdatePushCredentials:forType:",
            "pushRegistry:didReceiveIncomingPushWith:for:completion:",
            "pushRegistry:didReceiveIncomingPushWith:forType:completion:",
            "pushRegistry:didInvalidatePushTokenCapabilitiesForType:",
        ]
        for sel in selectors {
            XCTAssertFalse(
                plugin.responds(to: NSSelectorFromString(sel)),
                "plugin must not respond to \(sel) — the PushKit delegate surface is gone (IOS-PUSH-01)")
        }
    }
}
