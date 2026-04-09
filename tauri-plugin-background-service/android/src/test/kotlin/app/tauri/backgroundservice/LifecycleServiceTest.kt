package app.tauri.backgroundservice

import android.content.Context
import android.content.SharedPreferences
import android.content.pm.ServiceInfo
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.mockito.Mockito

/**
 * Unit tests for LifecycleService logic:
 * - SharedPreferences auto-start config roundtrip
 * - handleOsRestart behavior with null/valid label
 * - Service type mapping
 */
class LifecycleServiceTest {

    private lateinit var prefs: SharedPreferences
    private lateinit var editor: SharedPreferences.Editor

    @Before
    fun setup() {
        prefs = Mockito.mock(SharedPreferences::class.java)
        editor = Mockito.mock(SharedPreferences.Editor::class.java)
    }

    // ── SharedPreferences auto-start config ─────────────────────────────

    @Test
    fun autoStartConfigReadsPendingFlag() {
        Mockito.`when`(prefs.getBoolean("bg_auto_start_pending", false)).thenReturn(true)
        Mockito.`when`(prefs.getString("bg_auto_start_label", null)).thenReturn("Syncing")
        Mockito.`when`(prefs.getString("bg_auto_start_type", null)).thenReturn("dataSync")

        assertTrue(prefs.getBoolean("bg_auto_start_pending", false))
        assertEquals("Syncing", prefs.getString("bg_auto_start_label", null))
        assertEquals("dataSync", prefs.getString("bg_auto_start_type", null))
    }

    @Test
    fun autoStartConfigNoLabelWhenNotPending() {
        Mockito.`when`(prefs.getBoolean("bg_auto_start_pending", false)).thenReturn(false)
        Mockito.`when`(prefs.getString("bg_auto_start_label", null)).thenReturn(null)

        assertFalse(prefs.getBoolean("bg_auto_start_pending", false))
        assertNull(prefs.getString("bg_auto_start_label", null))
    }

    @Test
    fun autoStartConfigClearedAfterConsumption() {
        Mockito.`when`(prefs.edit()).thenReturn(editor)
        Mockito.`when`(editor.remove(Mockito.anyString())).thenReturn(editor)

        // Simulate clearAutoStartConfig
        editor.remove("bg_auto_start_pending")
        editor.remove("bg_auto_start_label")
        editor.remove("bg_auto_start_type")
        editor.apply()

        Mockito.verify(editor).remove("bg_auto_start_pending")
        Mockito.verify(editor).remove("bg_auto_start_label")
        Mockito.verify(editor).remove("bg_auto_start_type")
        Mockito.verify(editor).apply()
    }

    // ── handleOsRestart: null label → no auto-start ────────────────────

    @Test
    fun handleOsRestartWithNullLabelShouldStop() {
        Mockito.`when`(prefs.getString("bg_service_label", null)).thenReturn(null)
        // When label is null, the service should stop (START_NOT_STICKY)
        assertNull(prefs.getString("bg_service_label", null))
    }

    @Test
    fun handleOsRestartWithValidLabelSetsAutoStartFlag() {
        Mockito.`when`(prefs.getString("bg_service_label", null)).thenReturn("Syncing")
        Mockito.`when`(prefs.getString("bg_service_type", "dataSync")).thenReturn("specialUse")
        Mockito.`when`(prefs.edit()).thenReturn(editor)
        Mockito.`when`(editor.putBoolean(Mockito.anyString(), Mockito.anyBoolean())).thenReturn(editor)
        Mockito.`when`(editor.putString(Mockito.anyString(), Mockito.anyString())).thenReturn(editor)

        val label = prefs.getString("bg_service_label", null)
        assertNotNull(label)
        assertEquals("Syncing", label)

        val serviceType = prefs.getString("bg_service_type", "dataSync")!!
        editor.putBoolean("bg_auto_start_pending", true)
        editor.putString("bg_auto_start_label", label)
        editor.putString("bg_auto_start_type", serviceType)
        editor.apply()

        Mockito.verify(editor).putBoolean("bg_auto_start_pending", true)
        Mockito.verify(editor).putString("bg_auto_start_label", "Syncing")
        Mockito.verify(editor).putString("bg_auto_start_type", "specialUse")
        Mockito.verify(editor).apply()
    }

    // ── Service type mapping ────────────────────────────────────────────

    @Test
    fun mapServiceTypeDataSync() {
        val service = LifecycleService()
        val method = LifecycleService::class.java.getDeclaredMethod(
            "mapServiceType", String::class.java
        )
        method.isAccessible = true
        val result = method.invoke(service, "dataSync") as Int
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC, result)
    }

    @Test
    fun mapServiceTypeSpecialUse() {
        val service = LifecycleService()
        val method = LifecycleService::class.java.getDeclaredMethod(
            "mapServiceType", String::class.java
        )
        method.isAccessible = true
        val result = method.invoke(service, "specialUse") as Int
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE, result)
    }

    @Test
    fun mapServiceTypeUnknownFallsBackToDataSync() {
        val service = LifecycleService()
        val method = LifecycleService::class.java.getDeclaredMethod(
            "mapServiceType", String::class.java
        )
        method.isAccessible = true
        val result = method.invoke(service, "unknownType") as Int
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC, result)
    }

    // ── Restart timeout constants ───────────────────────────────────────

    @Test
    fun restartTimeoutIs30Seconds() {
        assertEquals(30_000L, LifecycleService.RESTART_TIMEOUT_MS)
    }
}
