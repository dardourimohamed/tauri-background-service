package app.tauri.backgroundservice

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class AndroidServiceStateTest {

    private lateinit var context: Context

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
        // Reset static state
        LifecycleService.isRunning = false
        LifecycleService.isForeground = false
        DurableState.clear(context)
        // Clear bg_service prefs
        context.getSharedPreferences("bg_service", Context.MODE_PRIVATE)
            .edit().clear().commit()
    }

    @After
    fun teardown() {
        LifecycleService.isRunning = false
        LifecycleService.isForeground = false
        DurableState.clear(context)
    }

    // ── AC1: Service State Query When Running ──────────────────────────────

    @Test
    fun query_whenRunning_returnsAllFieldsPopulated() {
        LifecycleService.isRunning = true
        LifecycleService.isForeground = true

        val durableState = DurableState(
            desiredRunning = true,
            lastServiceLabel = "App BG",
            lastServiceType = "remoteMessaging",
            lastNativeState = "running",
        )
        DurableState.save(context, durableState)

        context.getSharedPreferences("bg_service", Context.MODE_PRIVATE).edit()
            .putInt("bg_notif_id", 9001)
            .putString("bg_notif_channel_id", "bg_service")
            .commit()

        val state = AndroidServiceState.query(context)

        assertTrue("nativeRunning should be true", state.nativeRunning)
        assertTrue("nativeForeground should be true", state.nativeForeground)
        assertTrue("desiredRunning should be true", state.desiredRunning)
        assertEquals("running", state.durableState)
        assertEquals("App BG", state.serviceLabel)
        assertEquals("remoteMessaging", state.foregroundServiceType)
        assertEquals(9001, state.notificationId!!)
        assertEquals("bg_service", state.notificationChannelId)
        assertFalse("recoveryPending should be false", state.recoveryPending)
        assertNull("recoveryReason should be null", state.recoveryReason)
        assertNull("lastPlatformError should be null", state.lastPlatformError)
        assertNotNull("dataDir should be non-null", state.dataDir)
        assertTrue("dataDir should be non-empty", state.dataDir.isNotEmpty())
    }

    // ── AC2: Service State Query When Stopped ──────────────────────────────

    @Test
    fun query_whenStopped_returnsNativeNotRunning_noCrash() {
        LifecycleService.isRunning = false
        LifecycleService.isForeground = false
        DurableState.clear(context)

        val state = AndroidServiceState.query(context)

        assertFalse("nativeRunning should be false", state.nativeRunning)
        assertFalse("nativeForeground should be false", state.nativeForeground)
        assertFalse("desiredRunning should default to false", state.desiredRunning)
        assertEquals("unknown", state.durableState)
        assertNull("serviceLabel should be null when empty", state.serviceLabel)
        assertNotNull("foregroundServiceType should be populated from DurableState default", state.foregroundServiceType)
        assertNotNull("dataDir should still be populated", state.dataDir)
    }

    // ── AC3: Serialization Round-Trip ──────────────────────────────────────

    @Test
    fun serialization_roundTrip_preservesAllFields() {
        val original = AndroidServiceState(
            nativeRunning = true,
            nativeForeground = true,
            desiredRunning = true,
            durableState = "running",
            serviceLabel = "App BG",
            foregroundServiceType = "remoteMessaging",
            notificationId = 9001,
            notificationChannelId = "bg_service",
            recoveryPending = false,
            recoveryReason = null,
            lastPlatformError = null,
            dataDir = "/data/data/com.example.app",
        )

        val json = original.toJSON()
        val restored = AndroidServiceState.fromJSON(json)

        assertEquals(original, restored)
    }

    @Test
    fun serialization_roundTrip_withNullOptionals() {
        val original = AndroidServiceState(
            nativeRunning = false,
            nativeForeground = false,
            desiredRunning = false,
            durableState = "unknown",
            serviceLabel = null,
            foregroundServiceType = null,
            notificationId = null,
            notificationChannelId = null,
            recoveryPending = false,
            recoveryReason = null,
            lastPlatformError = null,
            dataDir = "/data/data/com.example.app",
        )

        val json = original.toJSON()
        val restored = AndroidServiceState.fromJSON(json)

        assertEquals(original, restored)
    }

    @Test
    fun serialization_roundTrip_withRecoveryPending() {
        val original = AndroidServiceState(
            nativeRunning = true,
            nativeForeground = false,
            desiredRunning = true,
            durableState = "core_start_failed",
            serviceLabel = "App",
            foregroundServiceType = "remoteMessaging",
            notificationId = null,
            notificationChannelId = null,
            recoveryPending = true,
            recoveryReason = "os_restart",
            lastPlatformError = "fgs_restricted: not allowed",
            dataDir = "/data/data/com.example.app",
        )

        val json = original.toJSON()
        val restored = AndroidServiceState.fromJSON(json)

        assertEquals(original, restored)
        assertTrue("recoveryPending should be true", restored.recoveryPending)
        assertEquals("os_restart", restored.recoveryReason)
        assertEquals("fgs_restricted: not allowed", restored.lastPlatformError)
    }

    // ── LifecycleService.isForeground flag ────────────────────────────────

    @Test
    fun isForeground_defaultsToFalse() {
        assertFalse(LifecycleService.isForeground)
    }

    @Test
    fun isForeground_canBeSet() {
        LifecycleService.isForeground = true
        assertTrue(LifecycleService.isForeground)
        LifecycleService.isForeground = false
        assertFalse(LifecycleService.isForeground)
    }
}
