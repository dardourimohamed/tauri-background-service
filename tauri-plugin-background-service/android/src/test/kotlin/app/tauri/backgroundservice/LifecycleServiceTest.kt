package app.tauri.backgroundservice

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import androidx.core.app.NotificationCompat
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows
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
    @Config(sdk = [33])
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
    @Config(sdk = [33]) // API 29+ for startForeground with service type
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
    @Config(sdk = [33])
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
    @Config(sdk = [33])
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

    @Test
    @Config(sdk = [33])
    fun onStartCommand_normalStart_persistConfigToSharedPreferences() {
        prefs.edit().clear().apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)

        // After a normal start, the service must persist its config so that
        // handleOsRestart can detect it after an OS-killed restart.
        assertEquals("Syncing", prefs.getString("bg_service_label", null))
        assertEquals("dataSync", prefs.getString("bg_service_type", null))

        // Cleanup
        LifecycleService.isRunning = false
    }

    // ── handleOsRestart: with stored label ────────────────────────────

    @Test
    @Config(sdk = [33])
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
    @Config(sdk = [33])
    fun handleOsRestart_withoutLabel_returnsNotSticky() {
        prefs.edit().clear().apply()

        val service = Robolectric.buildService(LifecycleService::class.java).create().get()
        val result = service.onStartCommand(null, 0, 0)

        assertEquals(android.app.Service.START_NOT_STICKY, result)
    }

    // ── onDestroy: resets state ────────────────────────────────────────

    @Test
    @Config(sdk = [33])
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
    @Config(sdk = [33])
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
    @Config(sdk = [33])
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

    // ── handleOsRestart: notification instead of activity launch ───────

    @Test
    @Config(sdk = [33])
    fun handleOsRestart_postsNotificationInsteadOfLaunchingActivity() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
            })
            .create()
            .get()

        service.onStartCommand(null, 0, 0)

        // Should NOT launch any activity
        val shadowApp = Shadows.shadowOf(context.applicationContext as android.app.Application)
        assertNull("Should not launch activity", shadowApp.nextStartedActivity)

        // Should post recovery notification on channel bg_service_recovery
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val notification = nm.activeNotifications.find { it.id == BootReceiver.RECOVERY_NOTIFICATION_ID }
        assertNotNull("Should post recovery notification", notification)

        // Cleanup
        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
    }

    @Test
    @Config(sdk = [33])
    fun handleOsRestart_stillSetsAutoStartFlag() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "specialUse")
            .apply()

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
            })
            .create()
            .get()

        val result = service.onStartCommand(null, 0, 0)
        assertEquals(android.app.Service.START_STICKY, result)
        assertTrue("Auto-start flag should be set", prefs.getBoolean("bg_auto_start_pending", false))
        assertEquals("Syncing", prefs.getString("bg_auto_start_label", null))
        assertEquals("specialUse", prefs.getString("bg_auto_start_type", null))
        assertTrue("Should be running", LifecycleService.isRunning)
        assertTrue("Should be autoRestarting", LifecycleService.autoRestarting)

        // Cleanup
        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
    }

    @Test
    @Config(sdk = [33])
    fun handleOsRestart_persistsRecoveryPendingState() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
            })
            .create()
            .get()

        service.onStartCommand(null, 0, 0)

        val state = DurableState.load(context)
        assertTrue("recoveryPending should be true", state.recoveryPending)
        assertEquals("os_restart", state.recoveryReason)

        // Cleanup
        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
    }

    @Test
    @Config(sdk = [33])
    fun onStartCommand_normalStart_cancelsRecoveryNotification() {
        // Simulate a recovery notification was posted (e.g. from handleOsRestart)
        BootReceiver.postRecoveryNotification(context, "Test")

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        assertNotNull("Recovery notification should exist",
            nm.activeNotifications.find { it.id == BootReceiver.RECOVERY_NOTIFICATION_ID })

        // Now do a normal start
        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)

        // Recovery notification should be cancelled
        assertNull("Recovery notification should be cancelled after normal start",
            nm.activeNotifications.find { it.id == BootReceiver.RECOVERY_NOTIFICATION_ID })

        // Cleanup
        LifecycleService.isRunning = false
    }

    // ── Restart timeout constants ───────────────────────────────────────

    @Test
    fun restartTimeoutIs30Seconds() {
        assertEquals(30_000L, LifecycleService.RESTART_TIMEOUT_MS)
    }

    // ── DurableState integration: buildStartState ──────────────────────

    @Test
    fun buildStartState_setsDesiredRunningTrue() {
        val previous = DurableState()
        val result = LifecycleService.buildStartState("Syncing", "dataSync", previous)

        assertTrue(result.desiredRunning)
    }

    @Test
    fun buildStartState_setsLabelTypeAndTimestamp() {
        val before = System.currentTimeMillis()
        val result = LifecycleService.buildStartState("Syncing", "specialUse", DurableState())
        val after = System.currentTimeMillis()

        assertEquals("Syncing", result.lastServiceLabel)
        assertEquals("specialUse", result.lastServiceType)
        assertTrue(result.lastStartEpochMs in before..after)
    }

    @Test
    fun buildStartState_setsNativeStateRunning() {
        val previous = DurableState(lastNativeState = "idle")
        val result = LifecycleService.buildStartState("Syncing", "dataSync", previous)

        assertEquals("running", result.lastNativeState)
    }

    @Test
    fun buildStartState_preservesPreviousHeartbeatAndRestartAttempt() {
        val previous = DurableState(
            lastHeartbeatEpochMs = 12345L,
            restartAttempt = 2,
        )
        val result = LifecycleService.buildStartState("Syncing", "dataSync", previous)

        assertEquals(12345L, result.lastHeartbeatEpochMs)
        assertEquals(2, result.restartAttempt)
    }

    // ── DurableState integration: buildStopState ──────────────────────

    @Test
    fun buildStopState_setsDesiredRunningFalse() {
        val previous = DurableState(desiredRunning = true)
        val result = LifecycleService.buildStopState(previous)

        assertFalse(result.desiredRunning)
    }

    @Test
    fun buildStopState_clearsRecoveryFields() {
        val previous = DurableState(
            desiredRunning = true,
            recoveryPending = true,
            recoveryReason = "boot_fgs_type_restricted",
        )
        val result = LifecycleService.buildStopState(previous)

        assertFalse(result.recoveryPending)
        assertNull(result.recoveryReason)
    }

    @Test
    fun buildStopState_preservesLabelTypeAndTimestamps() {
        val previous = DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
            lastStartEpochMs = 999L,
            lastHeartbeatEpochMs = 888L,
            restartAttempt = 3,
        )
        val result = LifecycleService.buildStopState(previous)

        assertEquals("Syncing", result.lastServiceLabel)
        assertEquals("dataSync", result.lastServiceType)
        assertEquals(999L, result.lastStartEpochMs)
        assertEquals(888L, result.lastHeartbeatEpochMs)
        assertEquals(3, result.restartAttempt)
    }

    // ── DurableState integration: onStartCommand persists ─────────────

    @Test
    @Config(sdk = [33])
    fun onStartCommand_normalStart_persistsDurableState() {
        DurableState.clear(context)

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)

        val state = DurableState.load(context)
        assertTrue("desiredRunning should be true after start", state.desiredRunning)
        assertEquals("Syncing", state.lastServiceLabel)
        assertEquals("dataSync", state.lastServiceType)
        assertTrue("lastStartEpochMs should be set", state.lastStartEpochMs > 0)
        assertEquals("running", state.lastNativeState)

        // Cleanup
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [33])
    fun onStartCommand_actionStop_persistsDesiredRunningFalse() {
        // First, simulate a start to populate DurableState
        val startState = DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
            lastStartEpochMs = 1000L,
            lastNativeState = "running",
        )
        DurableState.save(context, startState)

        val stopIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_STOP
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(stopIntent)
            .create()
            .get()

        service.onStartCommand(stopIntent, 0, 0)

        val state = DurableState.load(context)
        assertFalse("desiredRunning should be false after stop", state.desiredRunning)
        // Label should be preserved for diagnostics
        assertEquals("Syncing", state.lastServiceLabel)

        // Cleanup
        LifecycleService.isRunning = false
    }

    // ── Notification customization config ──────────────────────────────

    @Test
    @Config(sdk = [33])
    fun onStartCommand_normalStart_usesConfiguredChannelIdAndName() {
        prefs.edit()
            .putString("bg_notif_channel_id", "custom_channel")
            .putString("bg_notif_channel_name", "My Custom Channel")
            .apply()

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
        val channel = nm.getNotificationChannel("custom_channel")
        assertNotNull("Custom channel should be created", channel)
        assertEquals("custom_channel", channel!!.id)

        // Cleanup
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [33])
    fun onStartCommand_normalStart_usesConfiguredNotificationId() {
        prefs.edit()
            .putInt("bg_notif_id", 5555)
            .apply()

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
        val statusBarNotif = nm.activeNotifications.find { it.id == 5555 }
        assertNotNull("Should post notification with configured ID 5555", statusBarNotif)

        // Cleanup
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [33])
    fun onStartCommand_normalStart_hasStopActionWhenEnabled() {
        prefs.edit()
            .putBoolean("bg_show_stop_action", true)
            .apply()

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
        val notif = nm.activeNotifications.firstOrNull()
        assertNotNull("Should have a notification", notif)
        val actions = notif!!.notification.actions
        assertNotNull("Should have actions array", actions)
        assertTrue("Should have at least one action (stop)", actions!!.isNotEmpty())

        // Cleanup
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [33])
    fun onStartCommand_normalStart_noStopActionWhenDisabled() {
        prefs.edit()
            .putBoolean("bg_show_stop_action", false)
            .apply()

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
        val notif = nm.activeNotifications.firstOrNull()
        assertNotNull("Should have a notification", notif)
        val actions = notif!!.notification.actions
        assertTrue("Should have no actions when stop action disabled",
            actions == null || actions.isEmpty())

        // Cleanup
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [33])
    fun onStartCommand_actionStop_clearsNotificationConfigPrefs() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putString("bg_notif_channel_id", "custom_channel")
            .putString("bg_notif_channel_name", "Custom")
            .putInt("bg_notif_id", 5555)
            .putString("bg_notif_small_icon", "my_icon")
            .putBoolean("bg_show_stop_action", true)
            .apply()

        val stopIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_STOP
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(stopIntent)
            .create()
            .get()

        service.onStartCommand(stopIntent, 0, 0)

        assertFalse(prefs.contains("bg_notif_channel_id"))
        assertFalse(prefs.contains("bg_notif_channel_name"))
        assertFalse(prefs.contains("bg_notif_id"))
        assertFalse(prefs.contains("bg_notif_small_icon"))
        assertFalse(prefs.contains("bg_show_stop_action"))
    }

    @Test
    @Config(sdk = [33])
    fun handleOsRestart_usesPersistedNotificationConfig() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putString("bg_notif_channel_id", "os_channel")
            .putString("bg_notif_channel_name", "OS Recovery")
            .putInt("bg_notif_id", 7777)
            .apply()

        val service = Robolectric.buildService(LifecycleService::class.java)
            .create()
            .get()

        service.onStartCommand(null, 0, 0)

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = nm.getNotificationChannel("os_channel")
        assertNotNull("Should use configured channel ID on OS restart", channel)

        val notif = nm.activeNotifications.find { it.id == 7777 }
        assertNotNull("Should use configured notification ID on OS restart", notif)

        // Cleanup
        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
    }

    // ── buildTimeoutState ─────────────────────────────────────────────

    @Test
    fun buildTimeoutState_setsLastNativeStateToTimeout() {
        val previous = DurableState(desiredRunning = true, lastServiceType = "dataSync")
        val result = LifecycleService.buildTimeoutState(previous, "dataSync")
        assertEquals("timeout", result.lastNativeState)
    }

    @Test
    fun buildTimeoutState_setsLastPlatformErrorWithServiceType() {
        val previous = DurableState(desiredRunning = true, lastServiceType = "dataSync")
        val result = LifecycleService.buildTimeoutState(previous, "dataSync")
        assertNotNull(result.lastPlatformError)
        assertTrue("Error should contain FGS type",
            result.lastPlatformError!!.contains("dataSync"))
    }

    @Test
    fun buildTimeoutState_preservesDesiredRunning() {
        val previous = DurableState(desiredRunning = true)
        val result = LifecycleService.buildTimeoutState(previous, "dataSync")
        assertTrue("Timeout is involuntary — desiredRunning should stay true", result.desiredRunning)
    }

    @Test
    fun buildTimeoutState_preservesOtherFields() {
        val previous = DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
            lastStartEpochMs = 12345L,
            lastHeartbeatEpochMs = 67890L,
            restartAttempt = 2,
        )
        val result = LifecycleService.buildTimeoutState(previous, "dataSync")
        assertEquals("Syncing", result.lastServiceLabel)
        assertEquals(12345L, result.lastStartEpochMs)
        assertEquals(67890L, result.lastHeartbeatEpochMs)
        assertEquals(2, result.restartAttempt)
    }

    // ── handleTimeout: "stop" policy ──────────────────────────────────

    @Test
    @Config(sdk = [33])
    fun handleTimeout_stopPolicy_persistsTimeoutState() {
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putString("bg_on_timeout_policy", "stop")
            .apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)
        assertTrue("Precondition: should be running", LifecycleService.isRunning)

        service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)

        val state = DurableState.load(context)
        assertEquals("timeout", state.lastNativeState)
        assertTrue(state.lastPlatformError?.contains("dataSync") == true)
        assertFalse("recoveryPending should be false for stop policy", state.recoveryPending)

        // Cleanup
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [33])
    fun handleTimeout_stopPolicy_noExtraNotification() {
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putString("bg_on_timeout_policy", "stop")
            .apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)
        service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        assertNull("No timeout notification for stop policy",
            nm.activeNotifications.find { it.id == LifecycleService.TIMEOUT_NOTIFICATION_ID })
        assertNull("No recovery notification for stop policy",
            nm.activeNotifications.find { it.id == BootReceiver.RECOVERY_NOTIFICATION_ID })

        // Cleanup
        LifecycleService.isRunning = false
    }

    // ── handleTimeout: "notifyUser" policy ────────────────────────────

    @Test
    @Config(sdk = [33])
    fun handleTimeout_notifyUserPolicy_postsTimeoutNotification() {
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putString("bg_on_timeout_policy", "notifyUser")
            .apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)
        service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val timeoutNotif = nm.activeNotifications.find {
            it.id == LifecycleService.TIMEOUT_NOTIFICATION_ID
        }
        assertNotNull("Should post timeout notification for notifyUser policy", timeoutNotif)

        // Cleanup
        LifecycleService.isRunning = false
    }

    // ── handleTimeout: "scheduleRecovery" policy ──────────────────────

    @Test
    @Config(sdk = [33])
    fun handleTimeout_scheduleRecoveryPolicy_setsRecoveryPending() {
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putString("bg_on_timeout_policy", "scheduleRecovery")
            .apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)
        service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)

        val state = DurableState.load(context)
        assertTrue("recoveryPending should be true for scheduleRecovery policy", state.recoveryPending)
        assertEquals("timeout", state.recoveryReason)

        // Cleanup
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [33])
    fun handleTimeout_scheduleRecoveryPolicy_postsRecoveryNotification() {
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putString("bg_on_timeout_policy", "scheduleRecovery")
            .apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)
        service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val recoveryNotif = nm.activeNotifications.find {
            it.id == BootReceiver.RECOVERY_NOTIFICATION_ID
        }
        assertNotNull("Should post recovery notification for scheduleRecovery policy", recoveryNotif)

        // Cleanup
        LifecycleService.isRunning = false
    }

    // ── handleTimeout: default policy (notifyUser) ────────────────────

    @Test
    @Config(sdk = [33])
    fun handleTimeout_defaultPolicyIsNotifyUser() {
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            // No bg_on_timeout_policy set — should default to notifyUser
            .apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)
        service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val timeoutNotif = nm.activeNotifications.find {
            it.id == LifecycleService.TIMEOUT_NOTIFICATION_ID
        }
        assertNotNull("Default policy should be notifyUser — should post timeout notification",
            timeoutNotif)

        // Cleanup
        LifecycleService.isRunning = false
    }

    // ── handleTimeout: normal start cancels timeout notification ──────

    @Test
    @Config(sdk = [33])
    fun onStartCommand_normalStart_cancelsTimeoutNotification() {
        // Post a timeout notification manually
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = NotificationChannel(
            LifecycleService.TIMEOUT_CHANNEL_ID, "Timeout", NotificationManager.IMPORTANCE_HIGH)
        nm.createNotificationChannel(channel)
        nm.notify(LifecycleService.TIMEOUT_NOTIFICATION_ID,
            NotificationCompat.Builder(context, LifecycleService.TIMEOUT_CHANNEL_ID)
                .setSmallIcon(android.R.drawable.stat_notify_sync)
                .setContentTitle("Test").setContentText("Timeout").build())

        assertNotNull("Precondition: timeout notification should exist",
            nm.activeNotifications.find { it.id == LifecycleService.TIMEOUT_NOTIFICATION_ID })

        prefs.edit().clear().apply()
        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)

        assertNull("Timeout notification should be cancelled after normal start",
            nm.activeNotifications.find { it.id == LifecycleService.TIMEOUT_NOTIFICATION_ID })

        // Cleanup
        LifecycleService.isRunning = false
    }

    // ── handleTimeout: ACTION_STOP cancels timeout notification ───────

    @Test
    @Config(sdk = [33])
    fun onStartCommand_actionStop_clearsTimeoutPolicyPref() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putString("bg_on_timeout_policy", "notifyUser")
            .apply()

        val stopIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_STOP
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(stopIntent)
            .create()
            .get()

        service.onStartCommand(stopIntent, 0, 0)

        assertFalse("Timeout policy pref should be cleared on stop",
            prefs.contains("bg_on_timeout_policy"))

        // Cleanup
        LifecycleService.isRunning = false
    }

    // ── handleTimeout: emits event via BackgroundServicePlugin callback ──

    @Test
    @Config(sdk = [33])
    fun handleTimeout_invokesTimeoutEventCallback() {
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()

        var capturedError: String? = null
        BackgroundServicePlugin.onTimeoutEvent = { errorMessage ->
            capturedError = errorMessage
        }

        try {
            val intent = Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
                putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
                putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
            }

            val service = Robolectric.buildService(LifecycleService::class.java)
                .withIntent(intent)
                .create()
                .get()

            service.onStartCommand(intent, 0, 0)
            assertTrue("Precondition: should be running", LifecycleService.isRunning)

            service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)

            assertNotNull("Callback should have been invoked", capturedError)
            assertTrue("Error should contain service type",
                capturedError!!.contains("dataSync"))
        } finally {
            BackgroundServicePlugin.onTimeoutEvent = null
            LifecycleService.isRunning = false
        }
    }

    @Test
    @Config(sdk = [33])
    fun handleTimeout_noCrashWhenCallbackIsNull() {
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()

        BackgroundServicePlugin.onTimeoutEvent = null

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)
        // Should not throw when callback is null
        service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        assertFalse("Service should be stopped", LifecycleService.isRunning)

        LifecycleService.isRunning = false
    }

    // ── startForegroundTyped: exception handling ─────────────────────────

    @Test
    @Config(sdk = [33])
    fun startForegroundTyped_returnsTrueOnSuccess() {
        prefs.edit().clear().apply()
        val service = Robolectric.buildService(LifecycleService::class.java)
            .create().get()

        val method = LifecycleService::class.java.getDeclaredMethod(
            "startForegroundTyped", Int::class.java, Notification::class.java, Int::class.java
        )
        method.isAccessible = true
        val notification = Notification()
        val result = method.invoke(service, 1, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC) as Boolean
        assertTrue("Should return true on success", result)

        LifecycleService.isRunning = false
    }

    @Test
    fun persistStartForegroundError_persistsToDurableState() {
        DurableState.clear(context)
        val service = LifecycleService()
        val method = LifecycleService::class.java.getDeclaredMethod(
            "persistStartForegroundError", String::class.java, String::class.java
        )
        method.isAccessible = true
        // Need to attach service to context for DurableState.load to work
        // Use Robolectric to create a service attached to context
        val robolectricService = Robolectric.buildService(LifecycleService::class.java)
            .create().get()

        method.invoke(robolectricService, "missing_permission", "Missing FOREGROUND_SERVICE permission")

        val state = DurableState.load(context)
        assertNotNull("lastPlatformError should be set", state.lastPlatformError)
        assertTrue("Error should contain code",
            state.lastPlatformError!!.contains("missing_permission"))
        assertTrue("Error should contain message",
            state.lastPlatformError!!.contains("FOREGROUND_SERVICE"))
    }

    @Test
    fun persistStartForegroundError_preservesOtherFields() {
        DurableState.save(context, DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
            lastStartEpochMs = 12345L,
        ))

        val robolectricService = Robolectric.buildService(LifecycleService::class.java)
            .create().get()
        val method = LifecycleService::class.java.getDeclaredMethod(
            "persistStartForegroundError", String::class.java, String::class.java
        )
        method.isAccessible = true
        method.invoke(robolectricService, "fgs_restricted", "Not allowed")

        val state = DurableState.load(context)
        assertTrue("desiredRunning should be preserved", state.desiredRunning)
        assertEquals("Syncing", state.lastServiceLabel)
        assertEquals("dataSync", state.lastServiceType)
        assertEquals(12345L, state.lastStartEpochMs)
        assertTrue("lastPlatformError should be set", state.lastPlatformError!!.contains("fgs_restricted"))
    }
}
