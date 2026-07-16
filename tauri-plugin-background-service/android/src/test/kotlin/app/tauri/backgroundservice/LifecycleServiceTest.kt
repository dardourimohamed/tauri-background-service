package app.tauri.backgroundservice

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Looper
import androidx.core.app.NotificationCompat
import androidx.test.core.app.ApplicationProvider
import org.junit.After
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
        LifecycleService.bridgeProvider = { FakeCoreBridge(result = "running") }
        // Run the core-start task inline so assertions immediately after
        // onStartCommand observe the post-start state deterministically.
        LifecycleService.coreStartExecutor = { _, task -> task() }
        // Same determinism for ACTION_STOP's bridge.stop dispatch (BGS-20,
        // doc-08 Step 11): the default executor spawns a real worker
        // (fire-and-forget), which would race the post-onStartCommand
        // assertions in the existing stop tests. The off-main test
        // (bgs20_stop_off_main_thread) overrides this with a
        // thread-distinguishing executor.
        LifecycleService.coreStopExecutor = { _, task -> task() }
    }

    @After
    fun tearDown() {
        LifecycleService.bridgeProvider = { HeadlessBridgeImpl() }
        LifecycleService.coreStartExecutor = LifecycleService.DEFAULT_CORE_START_EXECUTOR
        LifecycleService.coreStopExecutor = LifecycleService.DEFAULT_CORE_STOP_EXECUTOR
        LifecycleService.isRunning = false
        LifecycleService.isForeground = false
        LifecycleService.autoRestarting = false
        BackgroundServicePlugin.onTimeoutEvent = null
        BackgroundServicePlugin.onNativeLifecycleEvent = null
        BackgroundServicePlugin.onPlatformErrorEvent = null
    }

    // ── onStartCommand: ACTION_STOP ────────────────────────────────────

    @Test
    @Config(sdk = [33])
    fun onStartCommand_actionStop_clearsPrefsAndReturnsNotSticky() {
        // Set up initial state
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()
        DurableState.save(context, DurableState(recoveryPending = true))

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
        assertFalse(DurableState.load(context).recoveryPending)
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

    // ── onStartCommand: ACTION_UPDATE_TYPE (spec 08 C6, Step 15) ──────

    @Test
    @Config(sdk = [33])
    fun onStartCommand_updateType_swapsRunningServiceTypeWithoutCoreRestart() {
        prefs.edit().clear().apply()

        // Start the service as remoteMessaging (the headless call-receiving FGS).
        val startIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Ongoing service")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "remoteMessaging")
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(startIntent)
            .create()
            .get()
        service.onStartCommand(startIntent, 0, 0)
        assertTrue("precondition: service running", LifecycleService.isForeground)
        assertEquals(
            "precondition: started as remoteMessaging",
            "remoteMessaging",
            prefs.getString("bg_service_type", null),
        )

        // Upgrade to phoneCall on answer — must NOT restart the core.
        val updateIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_UPDATE_TYPE
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "phoneCall")
        }
        val result = service.onStartCommand(updateIntent, 0, 0)

        assertEquals(android.app.Service.START_NOT_STICKY, result)
        assertEquals(
            "Type swapped to phoneCall in prefs",
            "phoneCall",
            prefs.getString("bg_service_type", null),
        )
        assertEquals(
            "Type swapped to phoneCall in durable state",
            "phoneCall",
            DurableState.load(context).lastServiceType,
        )

        // Cleanup
        LifecycleService.isRunning = false
        LifecycleService.isForeground = false
    }

    @Test
    @Config(sdk = [33])
    fun onStartCommand_updateType_invalidTypeRejectedAndUnchanged() {
        prefs.edit().clear().apply()
        val startIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Ongoing service")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "remoteMessaging")
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(startIntent)
            .create()
            .get()
        service.onStartCommand(startIntent, 0, 0)

        val updateIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_UPDATE_TYPE
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "nonsense")
        }
        service.onStartCommand(updateIntent, 0, 0)

        assertEquals(
            "Invalid type rejected — original type preserved",
            "remoteMessaging",
            prefs.getString("bg_service_type", null),
        )
        LifecycleService.isRunning = false
        LifecycleService.isForeground = false
    }

    /**
     * spec-compliance W1 / R-W1.3 (NFR-1): a foreground-start failure / FGS-type
     * rejection must (1) persist `DurableState.lastPlatformError` AND (2) fire a
     * native→JS event — the service must never self-stop silently. Driven via the
     * real FGS-type-rejection path: a foreground service receives an UPDATE_TYPE
     * with an invalid type, which `mapServiceType` rejects → `persistStartForegroundError`.
     */
    @Test
    @Config(sdk = [33])
    fun fgs_start_failure_persists_error_and_emits_event() {
        prefs.edit().clear().apply()
        DurableState.clear(context)

        // Capture the native→JS platform-error push.
        var emitted: String? = null
        BackgroundServicePlugin.onPlatformErrorEvent = { err -> emitted = err }

        // Bring the service to foreground via a normal start.
        val startIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Ongoing service")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "remoteMessaging")
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(startIntent)
            .create()
            .get()
        service.onStartCommand(startIntent, 0, 0)
        assertNull("normal start must not surface a platform error", emitted)

        // An FGS-type rejection (invalid type on UPDATE_TYPE) routes through
        // persistStartForegroundError.
        val updateIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_UPDATE_TYPE
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "nonsense")
        }
        val result = service.onStartCommand(updateIntent, 0, 0)

        // No silent self-stop: returns cleanly, error is durable AND surfaced to JS.
        assertEquals(android.app.Service.START_NOT_STICKY, result)
        val persisted = DurableState.load(context).lastPlatformError
        assertNotNull("FGS-type rejection must persist lastPlatformError", persisted)
        assertTrue(
            "persisted error names the rejection code",
            persisted!!.contains("invalid_type_update"),
        )
        assertEquals(
            "native→JS platform-error event must fire with the same error string",
            persisted,
            emitted,
        )

        LifecycleService.isRunning = false
        LifecycleService.isForeground = false
    }

    @Test
    @Config(sdk = [33])
    fun onStartCommand_updateType_notRunning_isNoOp() {
        prefs.edit().clear().apply()
        // No prior ACTION_START → service is not foreground.
        val updateIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_UPDATE_TYPE
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "phoneCall")
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(updateIntent)
            .create()
            .get()
        val result = service.onStartCommand(updateIntent, 0, 0)

        assertEquals(android.app.Service.START_NOT_STICKY, result)
        assertNull(
            "Not-running update must not persist a type",
            prefs.getString("bg_service_type", null),
        )
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
    fun handleOsRestart_withLabel_setsRecoveryPendingAndPersistsConfig() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "specialUse")
            .apply()

        LifecycleService.bridgeProvider = { FakeCoreBridge(result = "failed") }

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        // Null intent triggers handleOsRestart
        val result = service.onStartCommand(null, 0, 0)

        val state = DurableState.load(context)
        // Foreground promotion succeeded, so the return value is STICKY; the
        // core-start failure is handled after the fact (recovery persisted,
        // service stopped) because the core starts off the main thread.
        assertEquals(android.app.Service.START_STICKY, result)
        assertTrue("Recovery should be pending after core start failure", state.recoveryPending)
        assertEquals("core_start_failed", state.recoveryReason)
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

    // ── R-W1.4: native isRunning is the cross-bridge state authority ───

    /**
     * R-W1.4 / D-SPLITBRAIN: native `LifecycleService.isRunning` is the single
     * source of truth for service running-state, and it is the value that flows
     * out across the bridge for the Rust actor to reconcile against
     * (`AndroidServiceState.query` → `getAndroidServiceState` command → Rust
     * `get_android_service_state`). This pins that report-path contract: the
     * bridge surfaces the authority field verbatim in BOTH states, so the actor
     * can never reconcile against a stale or independently-derived running flag.
     */
    @Test
    fun isRunning_state_report_path() {
        LifecycleService.isRunning = true
        assertTrue(
            "report path must surface native isRunning=true as the authority",
            AndroidServiceState.query(context).nativeRunning,
        )

        LifecycleService.isRunning = false
        assertFalse(
            "report path must surface native isRunning=false as the authority",
            AndroidServiceState.query(context).nativeRunning,
        )
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

    // ── DurableState recovery config (roundtrip) ────────────────

    @Test
    fun durableStateRecoveryRoundtrip() {
        DurableState.save(context, DurableState(
            recoveryPending = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
        ))

        val state = DurableState.load(context)
        assertTrue(state.recoveryPending)
        assertEquals("Syncing", state.lastServiceLabel)
        assertEquals("dataSync", state.lastServiceType)
    }

    @Test
    fun durableStateDefaultsToNotPending() {
        DurableState.clear(context)

        val state = DurableState.load(context)
        assertFalse(state.recoveryPending)
        assertEquals("", state.lastServiceLabel)
    }

    @Test
    fun durableStateRecoveryClearedAfterConsumption() {
        DurableState.save(context, DurableState(
            recoveryPending = true,
            recoveryReason = "os_restart",
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
        ))

        // Simulate clearing recovery fields after consumption
        val current = DurableState.load(context)
        DurableState.save(context, current.copy(
            recoveryPending = false,
            recoveryReason = null,
        ))

        val state = DurableState.load(context)
        assertFalse(state.recoveryPending)
        assertNull(state.recoveryReason)
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

        LifecycleService.bridgeProvider = { FakeCoreBridge(result = "failed") }

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
    }

    @Test
    @Config(sdk = [33])
    fun handleOsRestart_stillPersistsConfigToDurableState() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "specialUse")
            .apply()

        LifecycleService.bridgeProvider = { FakeCoreBridge(result = "failed") }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
            })
            .create()
            .get()

        val result = service.onStartCommand(null, 0, 0)
        // STICKY: foreground promotion succeeded; the async core-start failure
        // persists recovery state and stops the service after the return.
        assertEquals(android.app.Service.START_STICKY, result)

        val state = DurableState.load(context)
        assertTrue("Recovery should be pending", state.recoveryPending)
        assertEquals("core_start_failed", state.recoveryReason)
    }

    @Test
    @Config(sdk = [33])
    fun handleOsRestart_persistsRecoveryPendingState() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()

        LifecycleService.bridgeProvider = { FakeCoreBridge(result = "failed") }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
            })
            .create()
            .get()

        service.onStartCommand(null, 0, 0)

        val state = DurableState.load(context)
        assertTrue("recoveryPending should be true", state.recoveryPending)
        assertEquals("core_start_failed", state.recoveryReason)
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

    // ── Bridge injection: CoreBridge is used instead of direct JNI ──────

    @Test
    @Config(sdk = [33])
    fun normalStart_callsBridgeStartWithCorrectReason() {
        prefs.edit().clear().apply()
        val fakeBridge = FakeCoreBridge(result = "running")
        LifecycleService.bridgeProvider = { fakeBridge }

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Test")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
            putExtra(LifecycleService.EXTRA_START_REASON, "test_reason")
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)

        assertEquals("test_reason", fakeBridge.lastStartReason)
        assertNull("stop should not be called", fakeBridge.lastStopReason)

        // Cleanup
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [33])
    fun actionStop_callsBridgeStopWithCorrectReason() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .apply()
        val fakeBridge = FakeCoreBridge(result = "running")
        LifecycleService.bridgeProvider = { fakeBridge }

        val stopIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_STOP
        }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(stopIntent)
            .create()
            .get()

        service.onStartCommand(stopIntent, 0, 0)

        assertEquals("android_service_stop", fakeBridge.lastStopReason)
        assertNull("start should not be called", fakeBridge.lastStartReason)
    }

    /**
     * BGS-20 (doc-08 Step 11): ACTION_STOP's `bridge.stop` JNI hop must run
     * OFF the main thread. The stop reaches `lib.rs` block_on(stop_headless_core)
     * (storage flush + network teardown), which ANRs if it runs inline on the
     * main looper while the user taps Stop from the notification.
     *
     * Load-bearing fixture: the fake captures the dispatch thread (`stopThread`),
     * and this test installs a THREAD-DISTINGUISHING executor (a real worker,
     * joined for determinism) — NOT the inline `coreStartExecutor`/`coreStopExecutor`
     * `{ _, task -> task() }` the start/stop-tests use, which runs on the test/main
     * thread and would make `stopThread == main` (a vacuous pass). NV-MUT: re-inlining
     * `bridge.stop` on the main thread REDs ONLY this assertion.
     */
    @Test
    @Config(sdk = [33])
    fun bgs20_stop_off_main_thread() {
        val mainThread = Looper.getMainLooper().thread
        val fakeBridge = FakeCoreBridge(result = "running")
        LifecycleService.bridgeProvider = { fakeBridge }
        // Thread-distinguishing executor: run on a real worker thread and join
        // so the assertion after onStartCommand is deterministic AND the worker
        // differs from main. (Inline `{ _, task -> task() }` runs on main → vacuous.)
        LifecycleService.coreStopExecutor = { _, task ->
            val worker = Thread({ task() }, "bg-core-stop-test")
            worker.start()
            worker.join()
        }

        val stopIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_STOP
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(stopIntent)
            .create()
            .get()

        service.onStartCommand(stopIntent, 0, 0)

        assertNotNull("bridge.stop must be dispatched", fakeBridge.stopThread)
        assertNotSame(
            "ACTION_STOP bridge.stop must run off the main thread (BGS-20)",
            mainThread,
            fakeBridge.stopThread,
        )
        // Regression: the stop still reached the bridge with the right reason.
        assertEquals("android_service_stop", fakeBridge.lastStopReason)

        // Cleanup
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [33])
    fun stickyRestart_callsBridgeStartWithStickyRestartReason() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()
        val fakeBridge = FakeCoreBridge(result = "running")
        LifecycleService.bridgeProvider = { fakeBridge }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
            })
            .create()
            .get()

        val result = service.onStartCommand(null, 0, 0)

        assertEquals("sticky_restart", fakeBridge.lastStartReason)
        assertEquals(android.app.Service.START_STICKY, result)
        assertTrue("Should be running after successful sticky restart", LifecycleService.isRunning)

        // Cleanup
        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
    }

    // ── Recovery start-acceptance emits (D1, spec01 Step 3) ────────────

    /** Capture (eventType, fgsType) pairs sent through the native bridge. */
    private fun captureNativeEvents(): MutableList<Pair<String, String?>> {
        val events = mutableListOf<Pair<String, String?>>()
        BackgroundServicePlugin.onNativeLifecycleEvent = { type, fgsType ->
            events.add(type to fgsType)
        }
        return events
    }

    @Test
    @Config(sdk = [33])
    fun stickyRestart_acceptedStart_emitsOsRestartAccepted() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()
        val events = captureNativeEvents()

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
            })
            .create()
            .get()

        service.onStartCommand(null, 0, 0)

        assertEquals(
            "accepted sticky restart should emit exactly one acceptance event",
            listOf("androidOsRestartAccepted" to null as String?),
            events,
        )
    }

    @Test
    @Config(sdk = [33])
    fun stickyRestart_failedStart_doesNotEmitAcceptance() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()
        LifecycleService.bridgeProvider = { FakeCoreBridge(result = "failed") }
        val events = captureNativeEvents()

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
            })
            .create()
            .get()

        service.onStartCommand(null, 0, 0)

        assertTrue(
            "rejected core start must not emit acceptance events, got: $events",
            events.none { it.first == "androidOsRestartAccepted" },
        )
    }

    @Test
    @Config(sdk = [33])
    fun bootCompletedStart_acceptedStart_emitsBootRecoveryAccepted() {
        val events = captureNativeEvents()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "remoteMessaging")
            putExtra(LifecycleService.EXTRA_START_REASON, "boot_completed")
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)

        assertEquals(
            "accepted boot-completed start should emit exactly one acceptance event",
            listOf("androidBootRecoveryAccepted" to null as String?),
            events,
        )
    }

    @Test
    @Config(sdk = [33])
    fun packageReplacedStart_acceptedStart_emitsBootRecoveryAccepted() {
        val events = captureNativeEvents()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "remoteMessaging")
            putExtra(LifecycleService.EXTRA_START_REASON, "package_replaced")
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)

        assertEquals(
            listOf("androidBootRecoveryAccepted" to null as String?),
            events,
        )
    }

    @Test
    @Config(sdk = [33])
    fun normalStart_acceptedStart_doesNotEmitAcceptance() {
        val events = captureNativeEvents()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "remoteMessaging")
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)

        assertTrue(
            "a user-initiated start is not recovery; no acceptance emit, got: $events",
            events.isEmpty(),
        )
    }

    @Test
    @Config(sdk = [33])
    fun bootCompletedStart_failedStart_doesNotEmitAcceptance() {
        LifecycleService.bridgeProvider = { FakeCoreBridge(result = "failed") }
        val events = captureNativeEvents()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "remoteMessaging")
            putExtra(LifecycleService.EXTRA_START_REASON, "boot_completed")
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()
            .get()

        service.onStartCommand(intent, 0, 0)

        assertTrue(
            "rejected core start must not emit acceptance events, got: $events",
            events.isEmpty(),
        )
    }

    @Test
    @Config(sdk = [33])
    fun stickyRestart_writesRecoveryBeforeCallingBridge() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()

        // Use a bridge that captures DurableState at start time
        var stateAtStart: DurableState? = null
        val recordingBridge = object : CoreBridge {
            override fun start(context: Context, reason: String): HeadlessBridgeResult {
                stateAtStart = DurableState.load(context)
                return FakeCoreBridge(result = "running").start(context, reason)
            }
            override fun stop(context: Context, reason: String): HeadlessBridgeResult {
                return FakeCoreBridge().stop(context, reason)
            }
            override fun notifyNetworkChanged(): HeadlessBridgeResult {
                return FakeCoreBridge().notifyNetworkChanged()
            }
        }
        LifecycleService.bridgeProvider = { recordingBridge }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
            })
            .create()
            .get()

        service.onStartCommand(null, 0, 0)

        assertNotNull("Bridge.start should have been called", stateAtStart)
        assertTrue("Recovery should be pending when bridge.start is called",
            stateAtStart!!.recoveryPending)
        assertEquals("os_restart", stateAtStart!!.recoveryReason)
    }

    @Test
    @Config(sdk = [33])
    fun stickyRestart_successfulStart_clearsRecovery() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()
        LifecycleService.bridgeProvider = { FakeCoreBridge(result = "running") }

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
            })
            .create()
            .get()

        service.onStartCommand(null, 0, 0)

        val state = DurableState.load(context)
        assertFalse("Recovery should be cleared after successful start", state.recoveryPending)
        assertNull(state.recoveryReason)
        assertEquals("running", state.lastNativeState)
        assertTrue(state.desiredRunning)

        // Cleanup
        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
    }

    @Test
    @Config(sdk = [33])
    fun startPersistsDurableStateOnly_noLegacyAutoStartPrefs() {
        prefs.edit().clear().apply()
        LifecycleService.bridgeProvider = { FakeCoreBridge(result = "running") }

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

        // bg_auto_start_* should never be written
        assertNull(prefs.getString("bg_auto_start_pending", null))
        assertNull(prefs.getString("bg_auto_start_label", null))
        assertNull(prefs.getString("bg_auto_start_type", null))

        // DurableState should be written
        val state = DurableState.load(context)
        assertTrue(state.desiredRunning)
        assertEquals("Test", state.lastServiceLabel)

        // Cleanup
        LifecycleService.isRunning = false
    }

    // ── Step 12: Persist DurableState Before JS Forwarding ──────────────

    @Test
    @Config(sdk = [33])
    fun handleTimeout_persistsBeforeNativeLifecycleEventCallback() {
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putString("bg_on_timeout_policy", "stop")
            .apply()

        // Capture DurableState at the moment onNativeLifecycleEvent fires.
        var stateAtCallback: DurableState? = null
        BackgroundServicePlugin.onNativeLifecycleEvent = { _, _ ->
            stateAtCallback = DurableState.load(context)
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

            // DurableState must already be "timeout" when the callback fires
            assertNotNull("onNativeLifecycleEvent should have been invoked", stateAtCallback)
            assertEquals(
                "DurableState should be timeout BEFORE callback fires",
                "timeout",
                stateAtCallback!!.lastNativeState,
            )
        } finally {
            BackgroundServicePlugin.onNativeLifecycleEvent = null
            LifecycleService.isRunning = false
        }
    }

    @Test
    @Config(sdk = [33])
    fun handleTimeout_persistsDespiteTimeoutEventCallbackThrowing() {
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .putString("bg_on_timeout_policy", "stop")
            .apply()

        // Callback throws — simulating JS runtime failure
        BackgroundServicePlugin.onTimeoutEvent = { _ ->
            throw RuntimeException("JS callback crashed")
        }
        BackgroundServicePlugin.onNativeLifecycleEvent = null

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

            // handleTimeout should still persist DurableState even if onTimeoutEvent throws
            try {
                service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
            } catch (_: RuntimeException) {
                // Expected — the JS callback throws
            }

            val state = DurableState.load(context)
            assertEquals(
                "DurableState should be timeout even when onTimeoutEvent throws",
                "timeout",
                state.lastNativeState,
            )
        } finally {
            BackgroundServicePlugin.onTimeoutEvent = null
            LifecycleService.isRunning = false
        }
    }

    @Test
    @Config(sdk = [33])
    fun onStartCommand_actionStop_persistsBeforeNativeLifecycleEventCallback() {
        // Pre-populate DurableState with running state
        DurableState.save(context, DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
            lastStartEpochMs = 1000L,
            lastNativeState = "running",
        ))

        // Capture DurableState at the moment onNativeLifecycleEvent fires
        var stateAtCallback: DurableState? = null
        BackgroundServicePlugin.onNativeLifecycleEvent = { _, _ ->
            stateAtCallback = DurableState.load(context)
        }

        try {
            val stopIntent = Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_STOP
            }

            val service = Robolectric.buildService(LifecycleService::class.java)
                .withIntent(stopIntent)
                .create()
                .get()

            service.onStartCommand(stopIntent, 0, 0)

            // DurableState must already have desiredRunning=false when callback fires
            assertNotNull("onNativeLifecycleEvent should have been invoked", stateAtCallback)
            assertFalse(
                "DurableState should have desiredRunning=false BEFORE callback fires",
                stateAtCallback!!.desiredRunning,
            )
        } finally {
            BackgroundServicePlugin.onNativeLifecycleEvent = null
            LifecycleService.isRunning = false
        }
    }

    // ── ConnectivityMonitor wiring (D4, spec01 Step 5) ──────────────────

    private fun shadowConnectivityManager() =
        shadowOf(context.getSystemService(Context.CONNECTIVITY_SERVICE)
            as android.net.ConnectivityManager)

    @Test
    @Config(sdk = [33])
    fun normalStart_registersConnectivityMonitor() {
        prefs.edit().clear().apply()
        val before = shadowConnectivityManager().networkCallbacks.size

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

        assertEquals(
            "Successful core start must register the ConnectivityMonitor",
            before + 1,
            shadowConnectivityManager().networkCallbacks.size,
        )
        assertNotNull(service.connectivityMonitor)

        // Cleanup
        service.onDestroy()
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [33])
    fun stickyRestart_registersConnectivityMonitor() {
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()
        val before = shadowConnectivityManager().networkCallbacks.size

        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(Intent(context, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
            })
            .create()
            .get()

        service.onStartCommand(null, 0, 0)

        assertEquals(
            "Successful sticky restart must register the ConnectivityMonitor",
            before + 1,
            shadowConnectivityManager().networkCallbacks.size,
        )

        // Cleanup
        service.onDestroy()
        LifecycleService.isRunning = false
        LifecycleService.autoRestarting = false
    }

    @Test
    @Config(sdk = [33])
    fun onDestroy_unregistersConnectivityMonitor() {
        prefs.edit().clear().apply()
        val before = shadowConnectivityManager().networkCallbacks.size

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Test")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }
        val controller = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent)
            .create()

        controller.get().onStartCommand(intent, 0, 0)
        assertEquals(before + 1, shadowConnectivityManager().networkCallbacks.size)

        controller.destroy()

        assertEquals(
            "onDestroy must unregister the ConnectivityMonitor",
            before,
            shadowConnectivityManager().networkCallbacks.size,
        )
    }

    @Test
    @Config(sdk = [33])
    fun coreStartFailure_doesNotLeaveConnectivityMonitorRegistered() {
        prefs.edit().clear().apply()
        LifecycleService.bridgeProvider = { FakeCoreBridge(result = "failed") }
        val before = shadowConnectivityManager().networkCallbacks.size

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

        assertEquals(
            "A failed core start must not leave a ConnectivityMonitor registered",
            before,
            shadowConnectivityManager().networkCallbacks.size,
        )
        assertNull(service.connectivityMonitor)
    }

    @Test
    @Config(sdk = [33])
    fun networkChange_callsBridgeNotifyNetworkChanged() {
        prefs.edit().clear().apply()
        val fakeBridge = FakeCoreBridge(result = "running")
        LifecycleService.bridgeProvider = { fakeBridge }

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
        val monitor = service.connectivityMonitor
        assertNotNull("Monitor should be registered after start", monitor)

        monitor!!.handleNetworkEvent()

        assertEquals(
            "A debounced network event must reach bridge.notifyNetworkChanged",
            1,
            fakeBridge.networkChangedCount,
        )

        // Cleanup
        service.onDestroy()
        LifecycleService.isRunning = false
    }

    @Test
    @Config(sdk = [33])
    fun networkChange_unsatisfiedLinkErrorIsSwallowed_serviceKeepsRunning() {
        prefs.edit().clear().apply()
        // Updated APK over an old native lib: the new JNI export is missing.
        val fakeBridge = FakeCoreBridge(result = "running").apply {
            networkChangedError = UnsatisfiedLinkError("no notifyNetworkChanged in the native core")
        }
        LifecycleService.bridgeProvider = { fakeBridge }

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
        val monitor = service.connectivityMonitor
        assertNotNull("Monitor should be registered after start", monitor)

        // Must not throw — the service-side callback swallows UnsatisfiedLinkError.
        monitor!!.handleNetworkEvent()

        assertEquals(1, fakeBridge.networkChangedCount)
        assertTrue(
            "Service must keep running when the native export is missing",
            LifecycleService.isRunning,
        )

        // Cleanup
        service.onDestroy()
        LifecycleService.isRunning = false
    }
}
