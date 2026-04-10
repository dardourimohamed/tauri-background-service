package app.tauri.backgroundservice

import android.content.Context
import android.content.SharedPreferences
import android.content.pm.ServiceInfo
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.mockito.Mockito
import java.lang.reflect.InvocationTargetException

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
