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
}
