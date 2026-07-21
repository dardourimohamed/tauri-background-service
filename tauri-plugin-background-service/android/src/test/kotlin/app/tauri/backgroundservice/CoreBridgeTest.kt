package app.tauri.backgroundservice

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class CoreBridgeTest {

    private lateinit var context: Context

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
    }

    @Test
    fun `FakeCoreBridge returns running state`() {
        val bridge = FakeCoreBridge(result = "running")
        val result = bridge.start(context, "test")
        assertEquals("running", result.state)
        assertTrue(result.ok)
        assertTrue(result.accepted)
    }

    @Test
    fun `FakeCoreBridge returns setup_idle state`() {
        val bridge = FakeCoreBridge(result = "setup_idle")
        val result = bridge.start(context, "test")
        assertEquals("setup_idle", result.state)
        assertTrue(result.ok)
        assertTrue(result.accepted)
    }

    @Test
    fun `FakeCoreBridge returns locked_idle state`() {
        val bridge = FakeCoreBridge(result = "locked_idle")
        val result = bridge.start(context, "test")
        assertEquals("locked_idle", result.state)
        assertTrue(result.ok)
        assertTrue(result.accepted)
    }

    @Test
    fun `FakeCoreBridge returns failed state`() {
        val bridge = FakeCoreBridge(result = "failed")
        val result = bridge.start(context, "test")
        assertEquals("failed", result.state)
        assertFalse(result.ok)
        assertFalse(result.accepted)
    }

    @Test
    fun `FakeCoreBridge stop returns success by default`() {
        val bridge = FakeCoreBridge()
        val result = bridge.stop(context, "test_stop")
        assertTrue(result.ok)
    }

    @Test
    fun `FakeCoreBridge records last start reason`() {
        val bridge = FakeCoreBridge(result = "running")
        bridge.start(context, "sticky_restart")
        assertEquals("sticky_restart", bridge.lastStartReason)
    }

    @Test
    fun `FakeCoreBridge records last stop reason`() {
        val bridge = FakeCoreBridge()
        bridge.stop(context, "android_service_stop")
        assertEquals("android_service_stop", bridge.lastStopReason)
    }

    @Test
    fun `FakeCoreBridge notifyNetworkChanged returns ok and counts calls`() {
        val bridge = FakeCoreBridge(result = "running")
        assertEquals(0, bridge.networkChangedCount)

        val result = bridge.notifyNetworkChanged()

        assertTrue(result.ok)
        assertEquals(1, bridge.networkChangedCount)
    }

    @Test
    fun `FakeCoreBridge notifyNetworkChanged throws configured error`() {
        val bridge = FakeCoreBridge(result = "running").apply {
            networkChangedError = UnsatisfiedLinkError("missing native")
        }

        try {
            bridge.notifyNetworkChanged()
            fail("Expected configured UnsatisfiedLinkError to propagate")
        } catch (_: UnsatisfiedLinkError) {
            // expected
        }
        assertEquals("The attempt must still be counted", 1, bridge.networkChangedCount)
    }

    @Test
    fun `HeadlessBridge networkChanged fails gracefully without native lib`() {
        // On the JVM there is no native core library, so ensureLoaded() fails; the wrapper
        // must return the same load-failure shape as start/stop, never throw.
        val result = HeadlessBridge.networkChanged()
        assertFalse(result.ok)
        assertEquals("failed", result.state)
        assertTrue(
            "Should report native_library_load_failed, got: ${result.rawJson}",
            result.rawJson.contains("native_library_load_failed"),
        )
    }

    // ── AND-03: `ok` is the accept discriminator; `state` is opaque ────

    @Test
    fun `ok true with unknown state degraded is accepted`() {
        // A host core returning an unknown-but-successful state must be accepted:
        // `accepted` follows `ok`, not a hardcoded state allowlist.
        val bridge = FakeCoreBridge.okState(ok = true, state = "degraded")
        val result = bridge.start(context, "test")
        assertTrue("ok=true must be accepted regardless of state", result.ok)
        assertTrue("ok=true,state=degraded must be accepted (AND-03)", result.accepted)
        assertEquals("state is opaque diagnostics, still surfaced", "degraded", result.state)
    }

    @Test
    fun `ok false is rejected even with a success-like state`() {
        val bridge = FakeCoreBridge.okState(ok = false, state = "running")
        val result = bridge.start(context, "test")
        assertFalse("ok=false must be rejected regardless of state", result.ok)
        assertFalse("ok=false must be rejected even if state=='running' (AND-03)", result.accepted)
    }

    @Test
    fun `fromJson treats ok as the accept discriminator`() {
        // The wire parser must drive `accepted` from `ok` only.
        val degraded = HeadlessBridgeResult.fromJson(
            """{"ok":true,"state":"degraded","recoverable":true}"""
        )
        assertTrue(degraded.ok)
        assertTrue("ok=true,state=degraded accepted via fromJson (AND-03)", degraded.accepted)

        val failed = HeadlessBridgeResult.fromJson(
            """{"ok":false,"state":"failed","message":"boom","recoverable":true}"""
        )
        assertFalse(failed.ok)
        assertFalse("ok=false rejected via fromJson (AND-03)", failed.accepted)
    }
}
