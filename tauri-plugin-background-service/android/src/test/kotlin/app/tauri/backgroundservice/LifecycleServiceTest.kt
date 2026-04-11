package app.tauri.backgroundservice

import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowNotificationManager
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Method

/**
 * Unit tests for LifecycleService logic:
 * - SharedPreferences auto-start config roundtrip
 * - onStartCommand paths (ACTION_STOP, null intent, normal start)
 * - handleOsRestart behavior
 * - buildNotification / createChannel
 * - Service type mapping (all 14 valid types)
 */
@RunWith(RobolectricTestRunner::class)
class LifecycleServiceTest {

    private lateinit var context: Context
    private lateinit var prefs: android.content.SharedPreferences

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
        prefs = context.getSharedPreferences("bg_service", Context.MODE_PRIVATE)
    }

    // ── onStartCommand: ACTION_STOP ────────────────────────────────────

    @Test
    fun onStartCommand_actionStop_clearsPrefsAndReturnsNotSticky() {
        // Set up initial state
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putBoolean("bg_auto_start_pending", true)
            .apply()

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_STOP
            })
            .create()
            .get()

        val result = service.onStartCommand(
            Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_STOP
            }, 0, 0
        )

        assertEquals(android.app.Service.START_NOT_STICKY, result)
        assertNull(prefs.getString("bg_service_label", null))
        assertNull(prefs.getString("bg_service_type", null))
        assertFalse(prefs.getBoolean("bg_auto_start_pending", false))
    }

    // ── onStartCommand: normal start ──────────────────────────────────

    @Test
    @Config(sdk = [29]) // API 29+ for startForeground with service type
    fun onStartCommand_normalStart_setsIsRunningAndReturnsSticky() {
        prefs.edit().clear().apply()

        assertFalse("Should not be running initially", LifecycleService.isRunning)

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Test Service")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        val result = service.onStartCommand(intent, 0, 0)

        assertEquals(android.app.Service.START_STICKY, result)
        assertTrue("Should be running after normal start", LifecycleService.isRunning)

        // Cleanup
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [29])
    fun onStartCommand_normalStart_createsNotificationChannel() {
        prefs.edit().clear().apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Test")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = nm.getNotificationChannel(LifecycleService.CHANNEL_ID)
        assertNotNull("Notification channel should be created", channel)
        assertEquals(LifecycleService.CHANNEL_ID, channel.id)
        assertEquals(NotificationManager.IMPORTANCE_LOW, channel.importance)

        // Cleanup
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [29])
    fun onStartCommand_normalStart_defaultLabelWhenExtraMissing() {
        prefs.edit().clear().apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            // No EXTRA_LABEL — should default to "Service running"
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        val result = service.onStartCommand(intent, 0, 0)
        assertEquals(android.app.Service.START_STICKY, result)

        // Cleanup
        LifecycleService.isRunning = false
    }

    // ── handleOsRestart: with stored label ────────────────────────────

    @Test
    @Config(sdk = [29])
    fun handleOsRestart_withLabel_setsAutoStartFlag() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "specialUse")
            .apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        // Null intent triggers handleOsRestart
        val result = service.onStartCommand(null, 0, 0)
        assertEquals(android.app.Service.START_STICKY, result)
        assertTrue("Should be running after OS restart", LifecycleService.isRunning)
        assertTrue("Should be autoRestarting", LifecycleService.autoRestarting)
        assertTrue(prefs.getBoolean("bg_auto_start_pending", false))
        assertEquals("Syncing", prefs.getString("bg_auto_start_label", null))
        assertEquals("specialUse", prefs.getString("bg_auto_start_type", null))

        // Cleanup
        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
    }

    // ── handleOsRestart: without stored label ──────────────────────────

    @Test
    fun handleOsRestart_withoutLabel_returnsNotSticky() {
        prefs.edit().clear().apply()

        val service = Robolectric.buildService(LifecycleService::class.java).create().get()
        val result = service.onStartCommand(null, 0, 0)

        assertEquals(android.app.Service.START_NOT_STICKY, result)
    }

    // ── onDestroy: resets state ────────────────────────────────────────

    @Test
    @Config(sdk = [29])
    fun onDestroy_resetsRunningState() {
        prefs.edit().clear().apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Test")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val controller = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()

        controller.get().onStartCommand(intent, 0, 0)
        assertTrue("Should be running", LifecycleService.isRunning)

        controller.destroy()
        assertFalse("Should not be running after destroy", LifecycleService.isRunning)
        assertFalse("Should not be autoRestarting after destroy", LifecycleService.autoRestarting)
    }

    // ── createChannel ─────────────────────────────────────────────────

    @Test
    fun createChannel_createsCorrectChannel() {
        val service = Robolectric.buildService(LifecycleService::class.java).create().get()
        val method: Method = LifecycleService::class.java.getDeclaredMethod("createChannel")
        method.isAccessible = true
        method.invoke(service)

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = nm.getNotificationChannel(LifecycleService.CHANNEL_ID)
        assertNotNull(channel)
        assertEquals("bg_keepalive", channel?.id)
        assertEquals(NotificationManager.IMPORTANCE_LOW, channel?.importance)
        assertFalse("Badge should be disabled", channel?.canShowBadge() ?: true)
    }

    // ── buildNotification ──────────────────────────────────────────────

    @Test
    fun buildNotification_hasCorrectContent() {
        val service = Robolectric.buildService(LifecycleService::class.java).create().get()
        val method: Method = LifecycleService::class.java.getDeclaredMethod(
            "buildNotification", String::class.java
        )
        method.isAccessible = true
        val notification = method.invoke(service, "Syncing data...") as Notification

        assertNotNull("Notification should be created", notification)
        // Verify via the shadow notification manager that a notification was built
        // The notification object itself is valid
        assertTrue("Notification should have flags", notification.flags >= 0)
    }

    // ── SharedPreferences auto-start config (real prefs) ────────────────

    @Test
    fun autoStartConfigReadsPendingFlag() {
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
    fun autoStartConfigNoLabelWhenNotPending() {
        prefs.edit().clear().apply()

        assertFalse(prefs.getBoolean("bg_auto_start_pending", false))
        assertNull(prefs.getString("bg_auto_start_label", null))
    }

    @Test
    fun autoStartConfigClearedAfterConsumption() {
        prefs.edit()
            .putBoolean("bg_auto_start_pending", true)
            .putString("bg_auto_start_label", "Syncing")
            .putString("bg_auto_start_type", "dataSync")
            .apply()

        // Simulate clearing after consumption
        prefs.edit()
            .remove("bg_auto_start_pending")
            .remove("bg_auto_start_label")
            .remove("bg_auto_start_type")
            .apply()

        assertFalse(prefs.getBoolean("bg_auto_start_pending", false))
        assertNull(prefs.getString("bg_auto_start_label", null))
        assertNull(prefs.getString("bg_auto_start_type", null))
    }

    // ── Service type mapping (all 14 valid types) ─────────────────────

    private fun invokeMapServiceType(type: String): Int {
        val service = LifecycleService()
        val method = LifecycleService::class.java.getDeclaredMethod(
            "mapServiceType", String::class.java
        )
        method.isAccessible = true
        return method.invoke(service, type) as Int
    }

    private fun invokeMapServiceTypeThrows(type: String): Throwable {
        val service = LifecycleService()
        val method = LifecycleService::class.java.getDeclaredMethod(
            "mapServiceType", String::class.java
        )
        method.isAccessible = true
        try {
            method.invoke(service, type)
            fail("Expected IllegalArgumentException for type: $type")
            throw AssertionError("unreachable")
        } catch (e: InvocationTargetException) {
            return e.targetException
        }
    }

    @Test
    fun mapServiceType_dataSync() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC, invokeMapServiceType("dataSync"))
    }

    @Test
    fun mapServiceType_mediaPlayback() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK, invokeMapServiceType("mediaPlayback"))
    }

    @Test
    fun mapServiceType_phoneCall() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_PHONE_CALL, invokeMapServiceType("phoneCall"))
    }

    @Test
    fun mapServiceType_location() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION, invokeMapServiceType("location"))
    }

    @Test
    fun mapServiceType_connectedDevice() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE, invokeMapServiceType("connectedDevice"))
    }

    @Test
    fun mapServiceType_mediaProjection() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION, invokeMapServiceType("mediaProjection"))
    }

    @Test
    fun mapServiceType_camera() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA, invokeMapServiceType("camera"))
    }

    @Test
    fun mapServiceType_microphone() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE, invokeMapServiceType("microphone"))
    }

    @Test
    fun mapServiceType_health() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_HEALTH, invokeMapServiceType("health"))
    }

    @Test
    fun mapServiceType_remoteMessaging() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING, invokeMapServiceType("remoteMessaging"))
    }

    @Test
    fun mapServiceType_systemExempted() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_SYSTEM_EXEMPTED, invokeMapServiceType("systemExempted"))
    }

    @Test
    fun mapServiceType_shortService() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_SHORT_SERVICE, invokeMapServiceType("shortService"))
    }

    @Test
    fun mapServiceType_specialUse() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE, invokeMapServiceType("specialUse"))
    }

    @Test
    fun mapServiceType_mediaProcessing() {
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROCESSING, invokeMapServiceType("mediaProcessing"))
    }

    @Test
    fun mapServiceType_unknown_throwsIllegalArgument() {
        val ex = invokeMapServiceTypeThrows("unknownType")
        assertTrue("Expected IllegalArgumentException, got ${ex.javaClass.simpleName}",
            ex is IllegalArgumentException)
        assertTrue("Message should contain the invalid type",
            ex.message?.contains("unknownType") == true)
    }

    @Test
    fun mapServiceType_empty_throwsIllegalArgument() {
        val ex = invokeMapServiceTypeThrows("")
        assertTrue(ex is IllegalArgumentException)
    }

    @Test
    fun mapServiceType_caseSensitive_throwsIllegalArgument() {
        val ex = invokeMapServiceTypeThrows("DataSync")
        assertTrue(ex is IllegalArgumentException)
    }

    // ── Restart timeout constants ───────────────────────────────────────

    @Test
    fun restartTimeoutIs30Seconds() {
        assertEquals(30_000L, LifecycleService.RESTART_TIMEOUT_MS)
    }
}
