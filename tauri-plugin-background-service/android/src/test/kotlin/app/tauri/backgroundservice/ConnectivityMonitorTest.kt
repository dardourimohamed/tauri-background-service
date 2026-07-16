package app.tauri.backgroundservice

import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.NetworkInfo
import android.os.Looper
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.shadows.ShadowLog
import org.robolectric.shadows.ShadowNetwork
import org.robolectric.shadows.ShadowNetworkCapabilities
import org.robolectric.shadows.ShadowNetworkInfo

/**
 * Unit tests for ConnectivityMonitor:
 * - leading-edge debounce with an injected fake clock
 * - register/unregister bracket the NetworkCallback lifecycle
 * - debounced fires run on the background handler thread, never the main looper
 * - logcat marker contract for the D5 verify-android.sh script
 */
@RunWith(RobolectricTestRunner::class)
class ConnectivityMonitorTest {

    private lateinit var context: Context

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
    }

    private fun connectivityManager(): ConnectivityManager =
        context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager

    // ── Debounce: fake clock ───────────────────────────────────────────

    @Test
    fun debounce_burstOfFiveCallbacksWithin500ms_firesExactlyOnce() {
        var now = 0L
        var fires = 0
        val monitor = ConnectivityMonitor(
            context = context,
            onNetworkChanged = { fires++ },
            debounceMs = 2_000,
            clock = { now },
        )

        for (t in longArrayOf(0, 100, 200, 350, 500)) {
            now = t
            monitor.handleNetworkEvent("network-$t")
        }

        assertEquals("Burst within the debounce window must fire once", 1, fires)
    }

    @Test
    fun debounce_twoCallbacksThreeSecondsApart_firesTwice() {
        var now = 0L
        var fires = 0
        val monitor = ConnectivityMonitor(
            context = context,
            onNetworkChanged = { fires++ },
            debounceMs = 2_000,
            clock = { now },
        )

        now = 0
        monitor.handleNetworkEvent("wifi")
        now = 3_000
        monitor.handleNetworkEvent("cellular")

        assertEquals("Callbacks spaced past the window must each fire", 2, fires)
    }

    @Test
    fun debounce_suppressedEventsDoNotExtendTheWindow() {
        var now = 0L
        var fires = 0
        val monitor = ConnectivityMonitor(
            context = context,
            onNetworkChanged = { fires++ },
            debounceMs = 2_000,
            clock = { now },
        )

        now = 0
        monitor.handleNetworkEvent("wifi") // fires
        now = 1_900
        monitor.handleNetworkEvent("cellular") // suppressed — must NOT push the window out
        now = 2_100
        monitor.handleNetworkEvent("ethernet") // 2100 - 0 >= 2000: fires

        assertEquals("A suppressed event must not extend the debounce window", 2, fires)
    }

    @Test
    fun debounce_defaultWindowIs2000ms() {
        assertEquals(2_000L, ConnectivityMonitor.DEFAULT_DEBOUNCE_MS)
    }

    @Test
    fun identicalCapabilityUpdatesNeverWakeCoreAgain() {
        var now = 0L
        var fires = 0
        val monitor = ConnectivityMonitor(
            context = context,
            onNetworkChanged = { fires++ },
            clock = { now },
        )

        monitor.handleNetworkEvent("wifi|validated=true")
        now = 5_000
        repeat(100) { monitor.handleNetworkEvent("wifi|validated=true") }

        assertEquals("Equivalent capability churn must be ignored", 1, fires)
    }

    // ── Trailing edge (BGS-24) ────────────────────────────────────────

    @Test
    fun bgs24_trailing_edge_fires() {
        var now = 0L
        var fires = 0
        var capturedTrailing: (() -> Unit)? = null
        var capturedDelay: Long? = null
        val monitor = ConnectivityMonitor(
            context = context,
            onNetworkChanged = { fires++ },
            debounceMs = 2_000,
            clock = { now },
            // Capture BOTH the trailing action and the delay it was scheduled
            // with. register() is never called, so the production postDelayed
            // default is moot. The delay is the load-bearing observable: it
            // must land the fire AFTER Core's 5 s throttle window
            // (network_changed.rs NETWORK_CHANGE_THROTTLE_WINDOW), not at the
            // naive lastFire + debounce which Core would re-throttle and drop.
            scheduler = { delayMs, action -> capturedDelay = delayMs; capturedTrailing = action },
        )

        // t0: the first distinct fingerprint fires immediately on the leading edge.
        now = 0
        monitor.handleNetworkEvent("wifi")
        assertEquals("first distinct fingerprint must fire immediately", 1, fires)

        // t500: a NEW fingerprint inside the 2 s debounce window is suppressed
        // by the leading edge — but it must schedule exactly one trailing fire
        // timed to clear Core's 5 s throttle (lastFire 0 + max(2000, 5250)).
        now = 500
        monitor.handleNetworkEvent("cellular")
        assertNotNull("a suppressed NEW fingerprint must schedule a trailing fire", capturedTrailing)
        assertEquals("the trailing fire must not run inside the debounce window", 1, fires)
        // PIN the load-bearing timing invariant: the trailing must be scheduled
        // to land AFTER Core's 5 s throttle window, at the absolute fire time
        // lastFire(0) + max(debounce 2000, CORE_WINDOW 5000 + SLACK 250) = 5250,
        // NOT at the naive lastFire + debounce (2000) that Core's throttle would
        // re-throttle and drop. The scheduled delay is fireAt - nowAtSuppression
        // = 5250 - 500 = 4750. This discriminates the maxOf(...) timing from a
        // regressed `debounceMs` timing (delay 1500 -> 500 + 1500 = 2000 != 5250).
        val expectedFireAt = 0L + maxOf(2_000L, 5_000L + 250L)
        assertEquals(
            "trailing must land past Core's 5s throttle, not at the naive debounce",
            expectedFireAt,
            now + capturedDelay!!,
        )

        // Advance to the scheduled trailing time and invoke the captured
        // action. No third handleNetworkEvent — the fire originates from the
        // trailing mechanism alone (non-placebo).
        now = 5_250
        capturedTrailing!!()
        assertEquals("the trailing fire must deliver the settled state to Core", 2, fires)
    }

    @Test
    fun bgs24_burst_coalesces_to_one_trailing() {
        var now = 0L
        var fires = 0
        val scheduled = mutableListOf<() -> Unit>()
        val delays = mutableListOf<Long>()
        val monitor = ConnectivityMonitor(
            context = context,
            onNetworkChanged = { fires++ },
            debounceMs = 2_000,
            clock = { now },
            scheduler = { delayMs, action -> scheduled += action; delays += delayMs },
        )

        // A burst of distinct suppressed fingerprints inside one debounce
        // window: each suppression schedules (mirrors production, where each
        // queues a postDelayed runnable), but only the LATEST may deliver.
        now = 0
        monitor.handleNetworkEvent("wifi") // leading fire
        now = 100
        monitor.handleNetworkEvent("cellular") // suppressed -> schedules trailing (gen N)
        now = 200
        monitor.handleNetworkEvent("ethernet") // suppressed -> schedules trailing (gen N+1)
        assertEquals("each suppressed fingerprint schedules a trailing action", 2, scheduled.size)
        assertEquals("no trailing has fired yet", 1, fires)
        // Mirror the timing pin: even in a burst, every scheduled trailing must
        // land past Core's 5 s throttle. The latest (the only one that may
        // deliver) is timed at lastFire 0 + max(2000, 5250) = 5250 absolute ->
        // delay 5250 - 200 = 5050.
        assertEquals(
            "the latest trailing in a burst must also clear Core's 5s throttle",
            0L + maxOf(2_000L, 5_000L + 250L),
            200L + delays.last(),
        )

        now = 5_250
        scheduled[0].invoke() // stale (gen N) — must no-op
        assertEquals("a superseded trailing action must not fire", 1, fires)
        scheduled[1].invoke() // latest (gen N+1) — fires
        assertEquals("only the latest trailing in a burst may deliver", 2, fires)
    }

    @Test
    fun bgs24_leading_fire_cancels_pending_trailing() {
        var now = 0L
        var fires = 0
        var capturedTrailing: (() -> Unit)? = null
        val monitor = ConnectivityMonitor(
            context = context,
            onNetworkChanged = { fires++ },
            debounceMs = 2_000,
            clock = { now },
            scheduler = { _, action -> capturedTrailing = action },
        )

        now = 0
        monitor.handleNetworkEvent("wifi") // leading fire
        now = 500
        monitor.handleNetworkEvent("cellular") // suppressed -> schedules trailing
        assertNotNull("a trailing must be pending", capturedTrailing)

        // A fresh leading-edge fire past the window supersedes the pending
        // trailing (a burst yields at most one trailing).
        now = 3_000
        monitor.handleNetworkEvent("ethernet") // 3000 - 0 >= 2000 -> leading fire
        assertEquals("the fresh leading fire must fire", 2, fires)

        // The previously scheduled trailing is now stale and must no-op.
        now = 5_250
        capturedTrailing!!()
        assertEquals("a trailing superseded by a fresh leading fire must not fire", 2, fires)
    }

    // ── register / unregister ──────────────────────────────────────────

    @Test
    fun register_addsNetworkCallback_andUnregisterRemovesIt() {
        val shadowCm = shadowOf(connectivityManager())
        val before = shadowCm.networkCallbacks.size

        val monitor = ConnectivityMonitor(context, onNetworkChanged = {})
        monitor.register()
        assertEquals("register() must add one NetworkCallback", before + 1, shadowCm.networkCallbacks.size)

        monitor.unregister()
        assertEquals("unregister() must remove the NetworkCallback", before, shadowCm.networkCallbacks.size)
    }

    @Test
    fun register_isIdempotent() {
        val shadowCm = shadowOf(connectivityManager())
        val before = shadowCm.networkCallbacks.size

        val monitor = ConnectivityMonitor(context, onNetworkChanged = {})
        monitor.register()
        monitor.register()
        assertEquals("Double register must not add a second callback", before + 1, shadowCm.networkCallbacks.size)

        monitor.unregister()
        monitor.unregister()
        assertEquals(before, shadowCm.networkCallbacks.size)
    }

    // ── Threading: fires off the main looper ──────────────────────────

    @Test
    fun networkCallback_firesOnBackgroundHandlerThread_neverMainLooper() {
        var firedOnLooper: Looper? = null
        val monitor = ConnectivityMonitor(
            context = context,
            onNetworkChanged = { firedOnLooper = Looper.myLooper() },
        )
        monitor.register()
        try {
            // Drive a real active-network transition AFTER register() so the
            // active fingerprint differs from the register-time baseline. Without
            // an actual change, the G8 capability-dedup gate correctly suppresses
            // the event (see `identicalCapabilityUpdatesNeverWakeCoreAgain`) and
            // the threading assertions below could never run.
            val shadowCm = shadowOf(connectivityManager())
            val network = ShadowNetwork.newInstance(42)
            val wifiInfo = ShadowNetworkInfo.newInstance(
                NetworkInfo.DetailedState.CONNECTED,
                ConnectivityManager.TYPE_WIFI, 0, true, NetworkInfo.State.CONNECTED,
            )
            shadowCm.addNetwork(network, wifiInfo)
            shadowCm.setActiveNetworkInfo(wifiInfo)
            val caps = ShadowNetworkCapabilities.newInstance()
            shadowOf(caps).addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            shadowOf(caps).addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            shadowCm.setNetworkCapabilities(network, caps)

            val callback = shadowCm.networkCallbacks.last()
            callback.onAvailable(network)

            val background = monitor.backgroundLooper()
            assertNotNull("Monitor must own a background looper after register()", background)
            shadowOf(background).idle()

            assertNotNull("Debounced fire must have run", firedOnLooper)
            assertNotSame(
                "Debounced fire must never run on the main looper (REQ-13/F2)",
                Looper.getMainLooper(), firedOnLooper,
            )
            assertSame("Debounced fire must run on the monitor's handler thread", background, firedOnLooper)
        } finally {
            monitor.unregister()
        }
    }

    // ── Logcat marker (Step 8 verify-android.sh contract) ─────────────

    @Test
    fun debouncedFire_logsTheVerifyScriptMarker() {
        ShadowLog.clear()
        var now = 0L
        val monitor = ConnectivityMonitor(
            context = context,
            onNetworkChanged = {},
            clock = { now },
        )

        monitor.handleNetworkEvent()

        val marker = ShadowLog.getLogs().find {
            it.tag == "ConnectivityMonitor" && it.msg == "network changed -> notifyNetworkChanged"
        }
        assertNotNull(
            "Each debounced fire must log the exact marker " +
                "'ConnectivityMonitor: network changed -> notifyNetworkChanged'",
            marker,
        )
    }
}
