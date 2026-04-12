package app.tauri.backgroundservice

import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.os.Build
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.*
import org.junit.Assume
import org.junit.Before
import org.junit.BeforeClass
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Instrumented tests for service behaviour when POST_NOTIFICATIONS permission is denied.
 *
 * On Android 13+ (API 33), apps require POST_NOTIFICATIONS to post user-facing
 * notifications. Foreground service notifications are exempt from this restriction,
 * so the service should start and function correctly regardless of the permission state.
 *
 * On API < 33, POST_NOTIFICATIONS does not exist — these tests still pass because
 * the permission grant/revoke is skipped.
 */
@RunWith(AndroidJUnit4::class)
class PermissionDenialTest {

    companion object {
        @BeforeClass
        @JvmStatic
        fun skipOnWaydroid() {
            Assume.assumeFalse(
                "Waydroid cannot revoke permissions at runtime",
                Build.FINGERPRINT.contains("waydroid", ignoreCase = true)
            )
        }
    }

    private fun isWaydroid(): Boolean =
        Build.FINGERPRINT.contains("waydroid", ignoreCase = true)

    private lateinit var context: Context
    private lateinit var prefs: SharedPreferences

    @Before
    fun setup() {
        context = InstrumentationRegistry.getInstrumentation().targetContext
        prefs = context.getSharedPreferences("bg_service", Context.MODE_PRIVATE)

        // Revoke POST_NOTIFICATIONS on API 33+
        revokeNotificationPermission()

        // Reset state
        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
        prefs.edit().clear().apply()
    }

    @After
    fun tearDown() {
        // Restore permission on API 33+
        grantNotificationPermission()

        // Stop service
        try {
            context.startService(
                Intent(context, LifecycleService::class.java).apply {
                    action = LifecycleService.ACTION_STOP
                }
            )
        } catch (_: Exception) { /* service may not be running */ }

        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
        prefs.edit().clear().apply()
    }

    private fun revokeNotificationPermission() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            InstrumentationRegistry.getInstrumentation().uiAutomation
                .executeShellCommand(
                    "pm revoke ${context.packageName} android.permission.POST_NOTIFICATIONS"
                )
            Thread.sleep(500)
        }
    }

    private fun grantNotificationPermission() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            InstrumentationRegistry.getInstrumentation().uiAutomation
                .executeShellCommand(
                    "pm grant ${context.packageName} android.permission.POST_NOTIFICATIONS"
                )
            Thread.sleep(500)
        }
    }

    private fun startForegroundService(label: String, type: String = "dataSync") {
        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, label)
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, type)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            context.startForegroundService(intent)
        } else {
            context.startService(intent)
        }
        Thread.sleep(1000)
    }

    private fun stopService() {
        context.startService(
            Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_STOP
            }
        )
        Thread.sleep(1000)
    }

    // ── Service starts without notification permission ─────────────────

    @Test
    fun serviceStartsWithoutNotificationPermission() {
        startForegroundService("No Permission Test")

        // Service should still start — foreground service notifications are
        // exempt from the POST_NOTIFICATIONS requirement
        assertTrue(
            "Service should start without POST_NOTIFICATIONS",
            LifecycleService.isRunning
        )

        // Channel should still be created
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = nm.getNotificationChannel(LifecycleService.CHANNEL_ID)
        assertNotNull("Notification channel should be created", channel)
    }

    // ── SharedPreferences persist without notification permission ──────

    @Test
    fun sharedPreferencesPersistedWithoutNotificationPermission() {
        startForegroundService("Persist Test", "specialUse")

        assertEquals(
            "Persist Test",
            prefs.getString("bg_service_label", null)
        )
        assertEquals(
            "specialUse",
            prefs.getString("bg_service_type", null)
        )
    }

    // ── Service stops correctly without notification permission ────────

    @Test
    fun serviceStopWorksWithoutNotificationPermission() {
        startForegroundService("Stop Test")
        assertTrue("Service should be running", LifecycleService.isRunning)

        stopService()

        assertFalse(
            "Service should stop without POST_NOTIFICATIONS",
            LifecycleService.isRunning
        )
        assertNull(
            "Prefs should be cleared after stop",
            prefs.getString("bg_service_label", null)
        )
    }

    // ── Foreground notification visible despite denied permission ──────

    @Test
    fun foregroundNotificationVisibleDespiteDeniedPermission() {
        // Foreground service notifications are exempt from POST_NOTIFICATIONS
        startForegroundService("Exempt Notification")

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val notifications = nm.activeNotifications
        val found = notifications.any { it.id == LifecycleService.NOTIF_ID }
        assertTrue(
            "Foreground notification should be visible despite denied POST_NOTIFICATIONS",
            found
        )
    }
}
