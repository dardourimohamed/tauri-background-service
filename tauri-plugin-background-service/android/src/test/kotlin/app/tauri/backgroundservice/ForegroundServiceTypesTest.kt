package app.tauri.backgroundservice

import android.content.Context
import android.content.pm.ServiceInfo
import androidx.test.core.app.ApplicationProvider
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * AND-01: a configured/allowlisted FGS type that is NOT declared in the merged
 * `<service foregroundServiceType>` must be rejected before dispatch (it would
 * crash late at `startForeground(..., type)` on Android 14+).
 *
 * Primary gate: the pure [BackgroundServicePlugin.validateDeclaredForegroundServiceType]
 * over an injected declared-bits value (allowlisted-but-undeclared rejects;
 * declared type passes; pre-Q / undeclared manifest is a no-op).
 *
 * Corroboration: [ForegroundServiceTypes.declaredBits] reflects the merged
 * manifest (the LifecycleService declares phoneCall), proving the runtime query
 * the preflight uses is populated from the manifest.
 */
@RunWith(Robolectric::class)
class ForegroundServiceTypesTest {

    private val declared = ForegroundServiceTypes.bitFor("dataSync") or
        ForegroundServiceTypes.bitFor("remoteMessaging") or
        ForegroundServiceTypes.bitFor("specialUse") or
        ForegroundServiceTypes.bitFor("phoneCall") or
        ForegroundServiceTypes.bitFor("microphone")

    // ── bitFor: shared single source ────────────────────────────────────

    @Test
    fun bitFor_mapsKnownTypesToTheirServiceInfoConstants() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC, ForegroundServiceTypes.bitFor("dataSync"))
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING, ForegroundServiceTypes.bitFor("remoteMessaging"))
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_PHONE_CALL, ForegroundServiceTypes.bitFor("phoneCall"))
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA, ForegroundServiceTypes.bitFor("camera"))
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE, ForegroundServiceTypes.bitFor("specialUse"))
    }

    @Test(expected = IllegalArgumentException::class)
    fun bitFor_unknownTypeThrows() {
        ForegroundServiceTypes.bitFor("bogus")
    }

    // ── validateDeclaredForegroundServiceType: the load-bearing gate ────

    @Test
    fun validate_allowlistedButUndeclared_isRejectedWithStructuredError() {
        // "camera" is a known, allowlistable type but NOT declared in `declared`.
        val error = BackgroundServicePlugin.validateDeclaredForegroundServiceType("camera", declared)
        assertNotNull("an allowlisted-but-undeclared type must be rejected (AND-01)", error)
        val json = JSONObject(error)
        assertEquals("fgs_type_not_declared", json.getString("code"))
        assertEquals("camera", json.getString("invalidType"))
    }

    @Test
    fun validate_declaredType_passes() {
        assertNull(
            "a declared type must pass (AND-01)",
            BackgroundServicePlugin.validateDeclaredForegroundServiceType("dataSync", declared),
        )
        assertNull(
            "phoneCall is declared and must pass",
            BackgroundServicePlugin.validateDeclaredForegroundServiceType("phoneCall", declared),
        )
    }

    @Test
    fun validate_preQOrUndeclaredManifest_isNoOp() {
        // declaredBits == 0 means no declaration to check against (API < 29 or
        // absent service): the config allowlist already gated the request.
        assertNull(
            "declaredBits==0 must not block (allowlist already gated)",
            BackgroundServicePlugin.validateDeclaredForegroundServiceType("dataSync", 0),
        )
    }

    @Test
    fun validate_unknownType_isDeferredToTheAllowlist() {
        // An unknown type is not a declared-bits concern; validateForegroundServiceType
        // / mapServiceType reject it elsewhere. validate must not mask that with a
        // spurious declared-bits error.
        assertNull(
            "unknown type is the allowlist's concern, not declared-bits",
            BackgroundServicePlugin.validateDeclaredForegroundServiceType("bogus", declared),
        )
    }

    // ── declaredBits: the runtime manifest query the preflight uses ─────

    @Test
    @Config(sdk = [34])
    fun declaredBits_reflectsMergedManifestPhoneCallDeclaration() {
        val ctx = ApplicationProvider.getApplicationContext<Context>()
        val bits = ForegroundServiceTypes.declaredBits(ctx)
        assertTrue(
            "declaredBits must be populated from the merged manifest (LifecycleService " +
                "declares phoneCall); got bits=$bits",
            bits != 0,
        )
        assertTrue(
            "the merged manifest declares phoneCall, so declaredBits must include it",
            (bits and ServiceInfo.FOREGROUND_SERVICE_TYPE_PHONE_CALL) != 0,
        )
        // The undeclared camera bit must NOT be set.
        assertFalse(
            "camera is not declared in the merged manifest",
            (bits and ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA) != 0,
        )
    }
}
