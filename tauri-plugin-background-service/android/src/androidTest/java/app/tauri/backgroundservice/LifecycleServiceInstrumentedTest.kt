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
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Instrumented tests for [LifecycleService], running on a real device/emulator.
 *
 * Tests foreground notification behaviour, notification channel creation,
 * START_STICKY restart semantics, and state cleanup on destroy.
 */
@RunWith(AndroidJUnit4::class)
class LifecycleServiceInstrumentedTest {

    private lateinit var context: Context
    private lateinit var prefs: SharedPreferences

    @Before
    fun setup() {
        context = InstrumentationRegistry.getInstrumentation().targetContext
        prefs = context.getSharedPreferences("bg_service", Context.MODE_PRIVATE)
        // Reset static state
        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
        prefs.edit().clear().apply()
    }

    @After
    fun tearDown() {
        // Stop the service if running
        try {
            context.startService(
                Intent(context, LifecycleService::class.java).apply {
                    action = LifecycleService.ACTION_STOP
                }
            )
        } catch (_: Exception) {
            // Service may not be running
        }
        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
        prefs.edit().clear().apply()
    }

    private fun waitUntil(
        timeoutMs: Long = 5_000L,
        intervalMs: Long = 100L,
        condition: () -> Boolean
    ) {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (!condition()) {
            if (System.currentTimeMillis() > deadline) {
                throw AssertionError("Condition not met within ${timeoutMs}ms")
            }
            Thread.sleep(intervalMs)
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
        waitUntil { LifecycleService.isRunning }
    }

    private fun stopService() {
        context.startService(
            Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_STOP
            }
        )
        waitUntil(timeoutMs = 3_000L) { !LifecycleService.isRunning }
    }

    // ── Foreground notification appears ────────────────────────────────

    @Test
    fun foregroundNotificationAppearsAfterStart() {
        startForegroundService("Instrumented Test")

        assertTrue("Service should be running", LifecycleService.isRunning)

        if (!isWaydroid()) {
            val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            val notifications = nm.activeNotifications
            val found = notifications.any { it.id == LifecycleService.NOTIF_ID }
            assertTrue(
                "Foreground notification with id ${LifecycleService.NOTIF_ID} should be active",
                found
            )
        }
    }

    // ── Notification channel created ───────────────────────────────────

    @Test
    fun notificationChannelCreatedCorrectly() {
        startForegroundService("Channel Test")

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = nm.getNotificationChannel(LifecycleService.CHANNEL_ID)

        assertNotNull("Notification channel should exist", channel)
        channel?.let {
            assertEquals("bg_keepalive", it.id)
            assertEquals(NotificationManager.IMPORTANCE_LOW, it.importance)
            assertEquals("Service Status", it.name.toString())
            assertFalse("Badge should be disabled", it.canShowBadge())
        }
    }

    // ── START_STICKY restart behaviour ─────────────────────────────────

    @Test
    fun serviceIsRunningWithStartStickySemantics() {
        startForegroundService("Sticky Test")

        // Service should be running and prefs should persist for OS restart detection
        assertTrue("Service should be running", LifecycleService.isRunning)
        assertEquals(
            "Service label should be persisted for restart detection",
            "Sticky Test",
            prefs.getString("bg_service_label", null)
        )
        assertEquals(
            "dataSync",
            prefs.getString("bg_service_type", null)
        )
    }

    @Test
    fun osRestartDetectsPersistedConfig() {
        // Simulate a previously running service by persisting config
        prefs.edit()
            .putString("bg_service_label", "Previous Run")
            .putString("bg_service_type", "specialUse")
            .apply()

        // Start service with null intent triggers handleOsRestart path
        // We simulate by starting with a null action intent
        val intent = Intent(context, LifecycleService::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            context.startForegroundService(intent)
        } else {
            context.startService(intent)
        }
        Thread.sleep(1000)

        // The OS restart path should set auto-start config
        assertTrue(
            "Auto-start pending flag should be set",
            prefs.getBoolean("bg_auto_start_pending", false)
        )
        assertEquals(
            "Previous Run",
            prefs.getString("bg_auto_start_label", null)
        )
        assertEquals(
            "specialUse",
            prefs.getString("bg_auto_start_type", null)
        )

        // Cleanup
        stopService()
    }

    // ── onDestroy resets state ─────────────────────────────────────────

    @Test
    fun onDestroy_resetsIsRunningAndAutoRestarting() {
        startForegroundService("Destroy Test")
        assertTrue("Should be running before stop", LifecycleService.isRunning)

        stopService()

        assertFalse(
            "isRunning should be false after onDestroy",
            LifecycleService.isRunning
        )
        assertFalse(
            "autoRestarting should be false after onDestroy",
            LifecycleService.autoRestarting
        )
    }

    // ── stop clears SharedPreferences ──────────────────────────────────

    @Test
    fun stopClearsSharedPreferences() {
        startForegroundService("Clear Test", "specialUse")
        assertNotNull("Label should exist before stop", prefs.getString("bg_service_label", null))

        stopService()

        assertNull("Label should be cleared", prefs.getString("bg_service_label", null))
        assertNull("Type should be cleared", prefs.getString("bg_service_type", null))
        assertFalse(
            "Auto-start pending should be cleared",
            prefs.getBoolean("bg_auto_start_pending", false)
        )
        assertNull(
            "Auto-start label should be cleared",
            prefs.getString("bg_auto_start_label", null)
        )
        assertNull(
            "Auto-start type should be cleared",
            prefs.getString("bg_auto_start_type", null)
        )
    }

    // ── Custom label reflected in notification ─────────────────────────

    @Test
    fun customLabelUsedInNotification() {
        startForegroundService("Custom Label Here")

        assertTrue("Service should be running", LifecycleService.isRunning)

        if (!isWaydroid()) {
            val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            val notifications = nm.activeNotifications.filter { it.id == LifecycleService.NOTIF_ID }
            assertTrue("Notification should be active", notifications.isNotEmpty())

            val extras = notifications.first().notification.extras
            val text = extras.getCharSequence(android.app.Notification.EXTRA_TEXT)?.toString()
            assertEquals("Custom Label Here", text)
        }
    }

    private fun isWaydroid(): Boolean =
        android.os.Build.FINGERPRINT.contains("waydroid", ignoreCase = true)
}
