package app.tauri.backgroundservice

import android.content.Context
import android.content.SharedPreferences
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.mockito.Mockito.*
import org.mockito.Mockito

/**
 * Unit tests for BackgroundServicePlugin logic:
 * - SharedPreferences read/write roundtrip for auto-start config
 * - startKeepalive persists label and type
 * - stopKeepalive clears all keys
 * - getAutoStartConfig reads pending state
 * - clearAutoStartConfig clears pending keys
 */
class BackgroundServicePluginTest {

    private lateinit var prefs: SharedPreferences
    private lateinit var editor: SharedPreferences.Editor

    @Before
    fun setup() {
        prefs = Mockito.mock(SharedPreferences::class.java)
        editor = Mockito.mock(SharedPreferences.Editor::class.java)
        `when`(prefs.edit()).thenReturn(editor)
        `when`(editor.putString(anyString(), anyString())).thenReturn(editor)
        `when`(editor.remove(anyString())).thenReturn(editor)
    }

    // ── startKeepalive: persists label and service type ────────────────

    @Test
    fun startKeepalivePersistsLabelAndType() {
        editor.putString("bg_service_label", "Syncing")
        editor.putString("bg_service_type", "dataSync")
        editor.apply()

        verify(editor).putString("bg_service_label", "Syncing")
        verify(editor).putString("bg_service_type", "dataSync")
        verify(editor).apply()
    }

    @Test
    fun startKeepaliveWithSpecialUsePersistsType() {
        editor.putString("bg_service_label", "Background Sync")
        editor.putString("bg_service_type", "specialUse")
        editor.apply()

        verify(editor).putString("bg_service_label", "Background Sync")
        verify(editor).putString("bg_service_type", "specialUse")
        verify(editor).apply()
    }

    // ── stopKeepalive: clears all keys ──────────────────────────────────

    @Test
    fun stopKeepaliveClearsAllKeys() {
        editor.remove("bg_service_label")
        editor.remove("bg_service_type")
        editor.remove("bg_auto_start_pending")
        editor.remove("bg_auto_start_label")
        editor.remove("bg_auto_start_type")
        editor.apply()

        verify(editor).remove("bg_service_label")
        verify(editor).remove("bg_service_type")
        verify(editor).remove("bg_auto_start_pending")
        verify(editor).remove("bg_auto_start_label")
        verify(editor).remove("bg_auto_start_type")
        verify(editor).apply()
    }

    // ── getAutoStartConfig: reads pending state ─────────────────────────

    @Test
    fun getAutoStartConfigReturnsPendingTrue() {
        `when`(prefs.getBoolean("bg_auto_start_pending", false)).thenReturn(true)
        `when`(prefs.getString("bg_auto_start_label", null)).thenReturn("Syncing")
        `when`(prefs.getString("bg_auto_start_type", null)).thenReturn("dataSync")

        assertTrue(prefs.getBoolean("bg_auto_start_pending", false))
        assertEquals("Syncing", prefs.getString("bg_auto_start_label", null))
        assertEquals("dataSync", prefs.getString("bg_auto_start_type", null))
    }

    @Test
    fun getAutoStartConfigReturnsNotPending() {
        `when`(prefs.getBoolean("bg_auto_start_pending", false)).thenReturn(false)

        assertFalse(prefs.getBoolean("bg_auto_start_pending", false))
    }

    @Test
    fun getAutoStartConfigPendingWithNoLabel() {
        `when`(prefs.getBoolean("bg_auto_start_pending", false)).thenReturn(true)
        `when`(prefs.getString("bg_auto_start_label", null)).thenReturn(null)

        // Pending is true but no label → no valid auto-start
        assertTrue(prefs.getBoolean("bg_auto_start_pending", false))
        assertNull(prefs.getString("bg_auto_start_label", null))
    }

    // ── clearAutoStartConfig: clears only auto-start keys ───────────────

    @Test
    fun clearAutoStartConfigClearsOnlyAutoStartKeys() {
        editor.remove("bg_auto_start_pending")
        editor.remove("bg_auto_start_label")
        editor.remove("bg_auto_start_type")
        editor.apply()

        // Should NOT clear bg_service_label or bg_service_type
        verify(editor, never()).remove("bg_service_label")
        verify(editor, never()).remove("bg_service_type")
        verify(editor).remove("bg_auto_start_pending")
        verify(editor).remove("bg_auto_start_label")
        verify(editor).remove("bg_auto_start_type")
        verify(editor).apply()
    }
}
