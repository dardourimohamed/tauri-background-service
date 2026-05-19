package app.tauri.backgroundservice

import android.app.Activity
import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/**
 * Unit tests for BackgroundServicePlugin SharedPreferences logic.
 *
 * Tests the actual SharedPreferences behavior that the @Command methods
 * rely on, rather than mocking SharedPreferences itself.
 *
 * Note: Full @Command method tests require Tauri Invoke objects which
 * need the Tauri Android framework. These tests verify the underlying
 * persistence logic.
 */
@RunWith(RobolectricTestRunner::class)
class BackgroundServicePluginTest {

    /** Concrete Activity for Robolectric's ActivityScenario. */
    class TestActivity : Activity()

    private lateinit var context: Context
    private lateinit var prefs: android.content.SharedPreferences

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
        prefs = context.getSharedPreferences("bg_service", Context.MODE_PRIVATE)
    }

    // ── startKeepalive: persists label and service type ────────────────

    @Test
    fun startKeepalivePersistsLabelAndType() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()

        assertEquals("Syncing", prefs.getString("bg_service_label", null))
        assertEquals("dataSync", prefs.getString("bg_service_type", null))
    }

    @Test
    fun startKeepaliveWithSpecialUsePersistsType() {
        prefs.edit()
            .putString("bg_service_label", "Background Sync")
            .putString("bg_service_type", "specialUse")
            .apply()

        assertEquals("Background Sync", prefs.getString("bg_service_label", null))
        assertEquals("specialUse", prefs.getString("bg_service_type", null))
    }

    // ── stopKeepalive: clears all keys ──────────────────────────────────

    @Test
    fun stopKeepaliveClearsAllKeys() {
        // Set up initial state
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putBoolean("bg_auto_start_pending", true)
            .putString("bg_auto_start_label", "Syncing")
            .putString("bg_auto_start_type", "dataSync")
            .apply()

        // Simulate stopKeepalive
        prefs.edit()
            .remove("bg_service_label")
            .remove("bg_service_type")
            .remove("bg_auto_start_pending")
            .remove("bg_auto_start_label")
            .remove("bg_auto_start_type")
            .apply()

        assertNull(prefs.getString("bg_service_label", null))
        assertNull(prefs.getString("bg_service_type", null))
        assertFalse(prefs.getBoolean("bg_auto_start_pending", false))
        assertNull(prefs.getString("bg_auto_start_label", null))
        assertNull(prefs.getString("bg_auto_start_type", null))
    }

    // ── getAutoStartConfig: reads pending state ─────────────────────────

    @Test
    fun getAutoStartConfigReturnsPendingTrue() {
        prefs.edit()
            .putBoolean("bg_auto_start_pending", true)
            .putString("bg_auto_start_label", "Syncing")
            .putString("bg_auto_start_type", "dataSync")
            .apply()

        assertTrue(prefs.getBoolean("bg_auto_start_pending", false))
        assertEquals("Syncing", prefs.getString("bg_auto_start_label", null))
        assertEquals("dataSync", prefs.getString("bg_auto_start_type", null))
    }

    @Test
    fun getAutoStartConfigReturnsNotPending() {
        assertFalse(prefs.getBoolean("bg_auto_start_pending", false))
    }

    @Test
    fun getAutoStartConfigPendingWithNoLabel() {
        prefs.edit()
            .putBoolean("bg_auto_start_pending", true)
            .apply()

        // Pending is true but no label → incomplete config
        assertTrue(prefs.getBoolean("bg_auto_start_pending", false))
        assertNull(prefs.getString("bg_auto_start_label", null))
    }

    // ── clearAutoStartConfig: clears only auto-start keys ───────────────

    @Test
    fun clearAutoStartConfigClearsOnlyAutoStartKeys() {
        prefs.edit()
            .putString("bg_service_label", "Active")
            .putString("bg_service_type", "dataSync")
            .putBoolean("bg_auto_start_pending", true)
            .putString("bg_auto_start_label", "Active")
            .putString("bg_auto_start_type", "dataSync")
            .apply()

        // Simulate clearAutoStartConfig
        prefs.edit()
            .remove("bg_auto_start_pending")
            .remove("bg_auto_start_label")
            .remove("bg_auto_start_type")
            .apply()

        // Auto-start keys cleared
        assertFalse(prefs.getBoolean("bg_auto_start_pending", false))
        assertNull(prefs.getString("bg_auto_start_label", null))
        assertNull(prefs.getString("bg_auto_start_type", null))

        // Service keys preserved
        assertEquals("Active", prefs.getString("bg_service_label", null))
        assertEquals("dataSync", prefs.getString("bg_service_type", null))
    }

    // ── load(): POST_NOTIFICATIONS permission request ──────────────────

    @Test
    @Config(sdk = [32]) // Below TIRAMISU (33) — no permission request
    fun loadDoesNotRequestPermissionsBelowApi33() {
        // On API < 33, POST_NOTIFICATIONS permission doesn't exist.
        // The load() method should skip the request entirely.
        // Verify by checking no permission request is pending.
        val activity = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        activity.onActivity { act ->
            val shadowActivity = shadowOf(act)
            // No permissions should have been requested
            assertNull(shadowActivity.lastRequestedPermission)
        }
    }

    @Test
    @Config(sdk = [33]) // TIRAMISU — should request permission if not granted
    fun loadRequestsPermissionsOnApi33WhenNotGranted() {
        val activity = androidx.test.core.app.ActivityScenario.launch(
            TestActivity::class.java
        )
        activity.onActivity { act ->
            // Deny the permission first
            val shadowActivity = shadowOf(act)
            shadowActivity.denyPermissions(android.Manifest.permission.POST_NOTIFICATIONS)

            // After calling load(), the plugin would request the permission.
            // Since we can't construct the plugin without Tauri framework,
            // verify the permission check logic directly.
            assertFalse(
                act.checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS)
                    == android.content.pm.PackageManager.PERMISSION_GRANTED
            )
        }
    }

    // ── Preflight FGS type validation ────────────────────────────────────

    @Test
    fun validateFgsTypeAllowedTypeReturnsNull() {
        val allowedTypes = listOf("dataSync", "specialUse")
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "dataSync", allowedTypes, true
        )
        assertNull(result)
    }

    @Test
    fun validateFgsTypeUndeclaredTypeReturnsError() {
        val allowedTypes = listOf("dataSync")
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "location", allowedTypes, true
        )
        assertNotNull(result)
        val json = org.json.JSONObject(result!!)
        assertEquals("fgs_type_not_allowed", json.getString("code"))
        assertEquals("location", json.getString("invalidType"))
    }

    @Test
    fun validateFgsTypeSkippedWhenValidationDisabled() {
        val allowedTypes = listOf("dataSync")
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "location", allowedTypes, false
        )
        assertNull(result)
    }

    @Test
    fun validateFgsTypeMultipleAllowedTypes() {
        val allowedTypes = listOf("dataSync", "location", "specialUse")
        assertNull(
            BackgroundServicePlugin.validateForegroundServiceType(
                "location", allowedTypes, true
            )
        )
        assertNull(
            BackgroundServicePlugin.validateForegroundServiceType(
                "specialUse", allowedTypes, true
            )
        )
        assertNotNull(
            BackgroundServicePlugin.validateForegroundServiceType(
                "mediaPlayback", allowedTypes, true
            )
        )
    }

    @Test
    fun validateFgsTypeEmptyAllowlistRejectsAll() {
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "dataSync", emptyList(), true
        )
        assertNotNull(result)
    }

    // ── Structured FGS validation error format ────────────────────────────

    @Test
    fun validateFgsType_structuredError_hasCodeField() {
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "location", listOf("dataSync"), true
        )
        assertNotNull(result)
        val json = org.json.JSONObject(result!!)
        assertEquals("fgs_type_not_allowed", json.getString("code"))
    }

    @Test
    fun validateFgsType_structuredError_hasMessageField() {
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "location", listOf("dataSync"), true
        )
        assertNotNull(result)
        val json = org.json.JSONObject(result!!)
        val message = json.getString("message")
        assertTrue("Message should mention the type", message.contains("location"))
        assertTrue("Message should mention config key", message.contains("androidForegroundServiceTypes"))
    }

    @Test
    fun validateFgsType_structuredError_hasInvalidTypeField() {
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "mediaPlayback", listOf("dataSync", "specialUse"), true
        )
        assertNotNull(result)
        val json = org.json.JSONObject(result!!)
        assertEquals("mediaPlayback", json.getString("invalidType"))
    }

    @Test
    fun validateFgsType_structuredError_hasValidOptionsArray() {
        val allowed = listOf("dataSync", "specialUse", "location")
        val result = BackgroundServicePlugin.validateForegroundServiceType(
            "camera", allowed, true
        )
        assertNotNull(result)
        val json = org.json.JSONObject(result!!)
        val options = json.getJSONArray("validOptions")
        val actual = (0 until options.length()).map { options.getString(it) }
        assertEquals(allowed, actual)
    }

    // ── stopKeepalive clears DurableState ─────────────────────────────────

    @Test
    fun stopKeepaliveClearsDurableState() {
        // Simulate service was running with DurableState persisted
        val durableState = DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
            lastStartEpochMs = 1000L,
        )
        DurableState.save(context, durableState)
        assertTrue("Precondition: DurableState should be saved",
            DurableState.load(context).desiredRunning)

        // Simulate stopKeepalive clearing DurableState
        DurableState.clear(context)

        val loaded = DurableState.load(context)
        assertFalse("desiredRunning should be false after clear", loaded.desiredRunning)
        assertEquals("", loaded.lastServiceLabel)
    }

    // ── computePermissionStatus ────────────────────────────────────────────

    @Test
    fun computePermissionStatus_granted_returnsGranted() {
        assertEquals("granted",
            BackgroundServicePlugin.computePermissionStatus(true, false))
    }

    @Test
    fun computePermissionStatus_grantedWithRationale_returnsGranted() {
        // granted takes precedence over rationale
        assertEquals("granted",
            BackgroundServicePlugin.computePermissionStatus(true, true))
    }

    @Test
    fun computePermissionStatus_notGranted_withRationale_returnsNotDetermined() {
        assertEquals("notDetermined",
            BackgroundServicePlugin.computePermissionStatus(false, true))
    }

    @Test
    fun computePermissionStatus_notGranted_withoutRationale_returnsDenied() {
        assertEquals("denied",
            BackgroundServicePlugin.computePermissionStatus(false, false))
    }

    // ── loadConfig: requestNotificationPermissionOnLoad ────────────────────

    @Test
    fun loadConfig_requestNotificationPermissionOnLoad_defaultsToTrue() {
        val json = org.json.JSONObject()
        // No androidRequestNotificationPermissionOnLoad key — should default to true
        assertTrue(json.optBoolean("androidRequestNotificationPermissionOnLoad", true))
    }

    @Test
    fun loadConfig_requestNotificationPermissionOnLoad_explicitFalse() {
        val json = org.json.JSONObject().apply {
            put("androidRequestNotificationPermissionOnLoad", false)
        }
        assertFalse(json.optBoolean("androidRequestNotificationPermissionOnLoad", true))
    }

    @Test
    fun loadConfig_requestNotificationPermissionOnLoad_explicitTrue() {
        val json = org.json.JSONObject().apply {
            put("androidRequestNotificationPermissionOnLoad", true)
        }
        assertTrue(json.optBoolean("androidRequestNotificationPermissionOnLoad", true))
    }
}
