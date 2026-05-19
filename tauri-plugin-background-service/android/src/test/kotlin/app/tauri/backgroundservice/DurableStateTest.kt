package app.tauri.backgroundservice

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/**
 * Unit tests for DurableState:
 * - Round-trip: save → load produces identical state
 * - Defaults: fresh load returns correct default values
 * - Clear: removes all keys from SharedPreferences
 * - Nullable fields: null values survive round-trip
 * - Non-default values: all fields persist correctly
 */
@RunWith(RobolectricTestRunner::class)
class DurableStateTest {

    private lateinit var context: Context

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
        DurableState.clear(context)
    }

    // ── Defaults ──────────────────────────────────────────────────────────

    @Test
    fun load_defaults_areCorrect() {
        val state = DurableState.load(context)

        assertFalse(state.desiredRunning)
        assertEquals("", state.lastServiceLabel)
        assertEquals("dataSync", state.lastServiceType)
        assertEquals(0L, state.lastStartEpochMs)
        assertEquals(0L, state.lastHeartbeatEpochMs)
        assertEquals("unknown", state.lastNativeState)
        assertNull(state.lastPlatformError)
        assertEquals(0, state.restartAttempt)
        assertFalse(state.recoveryPending)
        assertNull(state.recoveryReason)
    }

    // ── Round-trip: all fields populated ──────────────────────────────────

    @Test
    fun saveLoad_roundTrip_allFieldsPopulated() {
        val original = DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing data",
            lastServiceType = "specialUse",
            lastStartEpochMs = 1716057600000L,
            lastHeartbeatEpochMs = 1716057660000L,
            lastNativeState = "running",
            lastPlatformError = "timeout",
            restartAttempt = 3,
            recoveryPending = true,
            recoveryReason = "boot_fgs_type_restricted",
        )

        DurableState.save(context, original)
        val loaded = DurableState.load(context)

        assertEquals(original, loaded)
    }

    // ── Round-trip: nullable fields are null ───────────────────────────────

    @Test
    fun saveLoad_roundTrip_nullableFieldsNull() {
        val original = DurableState(
            desiredRunning = true,
            lastServiceLabel = "Active",
            lastPlatformError = null,
            recoveryReason = null,
        )

        DurableState.save(context, original)
        val loaded = DurableState.load(context)

        assertEquals(original, loaded)
        assertNull(loaded.lastPlatformError)
        assertNull(loaded.recoveryReason)
    }

    // ── Clear ─────────────────────────────────────────────────────────────

    @Test
    fun clear_removesAllKeys() {
        val state = DurableState(
            desiredRunning = true,
            lastServiceLabel = "Sync",
            lastServiceType = "location",
            lastStartEpochMs = 12345L,
            lastHeartbeatEpochMs = 67890L,
            lastNativeState = "running",
            lastPlatformError = "error",
            restartAttempt = 5,
            recoveryPending = true,
            recoveryReason = "test",
        )

        DurableState.save(context, state)
        DurableState.clear(context)
        val loaded = DurableState.load(context)

        // After clear, should return defaults
        val defaults = DurableState()
        assertEquals(defaults, loaded)
    }

    @Test
    fun clear_onEmptyPrefs_isNoOp() {
        // Clear on already-empty prefs should not throw
        DurableState.clear(context)
        DurableState.clear(context)

        val loaded = DurableState.load(context)
        assertEquals(DurableState(), loaded)
    }

    // ── Overwrite ─────────────────────────────────────────────────────────

    @Test
    fun save_overwritesPreviousState() {
        val first = DurableState(
            desiredRunning = true,
            lastServiceLabel = "First",
            lastPlatformError = "old error",
        )
        val second = DurableState(
            desiredRunning = false,
            lastServiceLabel = "Second",
            lastPlatformError = null,
        )

        DurableState.save(context, first)
        DurableState.save(context, second)
        val loaded = DurableState.load(context)

        assertEquals(second, loaded)
        assertFalse(loaded.desiredRunning)
        assertEquals("Second", loaded.lastServiceLabel)
        assertNull(loaded.lastPlatformError)
    }

    // ── Specific field updates ────────────────────────────────────────────

    @Test
    fun saveLoad_heartbeatUpdatePreservesOtherFields() {
        val original = DurableState(
            desiredRunning = true,
            lastServiceLabel = "Syncing",
            lastServiceType = "dataSync",
            lastStartEpochMs = 1000L,
        )
        DurableState.save(context, original)

        val updated = original.copy(lastHeartbeatEpochMs = 2000L)
        DurableState.save(context, updated)

        val loaded = DurableState.load(context)
        assertEquals(2000L, loaded.lastHeartbeatEpochMs)
        assertTrue(loaded.desiredRunning)
        assertEquals("Syncing", loaded.lastServiceLabel)
        assertEquals("dataSync", loaded.lastServiceType)
        assertEquals(1000L, loaded.lastStartEpochMs)
    }

    @Test
    fun saveLoad_nativeStateValues() {
        val states = listOf("idle", "starting", "running", "stopping", "timeout", "expired", "recovering", "error")

        for (nativeState in states) {
            DurableState.clear(context)
            val state = DurableState(lastNativeState = nativeState)
            DurableState.save(context, state)
            val loaded = DurableState.load(context)
            assertEquals(nativeState, loaded.lastNativeState)
        }
    }

    // ── SharedPreferences file name ───────────────────────────────────────

    @Test
    fun usesCorrectSharedPreferencesFile() {
        DurableState.save(context, DurableState(desiredRunning = true))

        val prefs = context.getSharedPreferences("tauri_bg_service_state", Context.MODE_PRIVATE)
        assertTrue(prefs.getBoolean("desired_running", false))
    }
}
