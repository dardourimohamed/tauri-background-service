package app.tauri.backgroundservice

import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/**
 * AND-05: native lifecycle/platform events emitted before the plugin's `load()`
 * attaches its JS callback must be retained (bounded, ordered) and replayed
 * exactly once, in order, when the callback attaches — not dropped.
 *
 * Covers three layers:
 * - the [NativeEventQueue] FIFO + bound + exactly-once drain semantics;
 * - the [BackgroundServicePlugin] emit helpers (deliver-when-attached,
 *   enqueue-when-not);
 * - the [BackgroundServicePlugin.drainQueuedNativeEvents] replay through the
 *   now-attached callbacks in insertion order.
 */
@RunWith(RobolectricTestRunner::class)
class NativeEventQueueTest {

    @Before
    fun setup() {
        NativeEventQueue.resetForTest()
        BackgroundServicePlugin.onTimeoutEvent = null
        BackgroundServicePlugin.onNativeLifecycleEvent = null
        BackgroundServicePlugin.onPlatformErrorEvent = null
    }

    @After
    fun tearDown() {
        NativeEventQueue.resetForTest()
        BackgroundServicePlugin.onTimeoutEvent = null
        BackgroundServicePlugin.onNativeLifecycleEvent = null
        BackgroundServicePlugin.onPlatformErrorEvent = null
    }

    // ── NativeEventQueue: FIFO + exactly-once drain ────────────────────

    @Test
    fun queue_drainsInInsertionOrderAndClears() {
        NativeEventQueue.enqueue(QueuedNativeEvent.Lifecycle("a", null))
        NativeEventQueue.enqueue(QueuedNativeEvent.Timeout("t1"))
        NativeEventQueue.enqueue(QueuedNativeEvent.PlatformError("e1"))

        val drained = NativeEventQueue.drainAndClear()

        assertEquals(3, drained.size)
        assertEquals("a", (drained[0] as QueuedNativeEvent.Lifecycle).type)
        assertEquals("t1", (drained[1] as QueuedNativeEvent.Timeout).errorMessage)
        assertEquals("e1", (drained[2] as QueuedNativeEvent.PlatformError).error)
        assertEquals("drain must clear the queue", 0, NativeEventQueue.size)
    }

    @Test
    fun queue_doubleDrainYieldsSecondEmpty() {
        NativeEventQueue.enqueue(QueuedNativeEvent.Lifecycle("once", null))
        NativeEventQueue.drainAndClear()
        assertEquals("exactly-once: a second drain yields nothing", 0, NativeEventQueue.drainAndClear().size)
    }

    @Test
    fun queue_boundedDropsOldestOnOverflow() {
        // Fill to the bound; one more enqueue drops the oldest.
        repeat(NativeEventQueueTest.visibleBound()) {
            NativeEventQueue.enqueue(QueuedNativeEvent.Lifecycle("ev-$it", null))
        }
        assertEquals(NativeEventQueueTest.visibleBound(), NativeEventQueue.size)

        NativeEventQueue.enqueue(QueuedNativeEvent.Lifecycle("overflow", null))
        assertEquals("bound must hold (drop-oldest), not grow unbounded", NativeEventQueueTest.visibleBound(), NativeEventQueue.size)

        val drained = NativeEventQueue.drainAndClear()
        // The oldest ("ev-0") was dropped; "overflow" is present at the tail.
        assertTrue("overflow event must be retained", drained.any { (it as? QueuedNativeEvent.Lifecycle)?.type == "overflow" })
        assertFalse("oldest event must have been dropped on overflow", drained.any { (it as? QueuedNativeEvent.Lifecycle)?.type == "ev-0" })
    }

    // ── emit helpers: deliver when attached, enqueue when not ───────────

    @Test
    fun emitNativeLifecycleEvent_deliversWhenCallbackAttached() {
        val received = mutableListOf<Pair<String, String?>>()
        BackgroundServicePlugin.onNativeLifecycleEvent = { type, fgs -> received += type to fgs }

        BackgroundServicePlugin.emitNativeLifecycleEvent("androidTimeout", "remoteMessaging")

        assertEquals(listOf("androidTimeout" to "remoteMessaging"), received)
        assertEquals("nothing enqueued when the callback is attached", 0, NativeEventQueue.size)
    }

    @Test
    fun emitNativeLifecycleEvent_enqueuesWhenCallbackAbsent() {
        assertNull(BackgroundServicePlugin.onNativeLifecycleEvent)

        BackgroundServicePlugin.emitNativeLifecycleEvent("androidTimeout", "dataSync")

        assertEquals(1, NativeEventQueue.size)
        val drained = NativeEventQueue.drainAndClear()
        val ev = drained.single() as QueuedNativeEvent.Lifecycle
        assertEquals("androidTimeout", ev.type)
        assertEquals("dataSync", ev.fgsType)
    }

    @Test
    fun emitTimeoutAndPlatformError_enqueueWhenCallbacksAbsent() {
        BackgroundServicePlugin.emitTimeoutEvent("boom")
        BackgroundServicePlugin.emitPlatformErrorEvent("fail: x")

        val drained = NativeEventQueue.drainAndClear()
        assertEquals(2, drained.size)
        assertEquals("boom", (drained[0] as QueuedNativeEvent.Timeout).errorMessage)
        assertEquals("fail: x", (drained[1] as QueuedNativeEvent.PlatformError).error)
    }

    // ── drainQueuedNativeEvents: ordered replay through attached callbacks ─

    @Test
    fun drainQueuedNativeEvents_replaysInOrderExactlyOnce() {
        // Pre-load events as if LifecycleService emitted them before load().
        NativeEventQueue.enqueue(QueuedNativeEvent.Lifecycle("androidOsRestartAccepted", null))
        NativeEventQueue.enqueue(QueuedNativeEvent.Timeout("FGS timeout (type: remoteMessaging)"))

        val lifecycle = mutableListOf<Pair<String, String?>>()
        val timeouts = mutableListOf<String>()
        BackgroundServicePlugin.onNativeLifecycleEvent = { t, f -> lifecycle += t to f }
        BackgroundServicePlugin.onTimeoutEvent = { m -> timeouts += m }

        BackgroundServicePlugin.drainQueuedNativeEvents()

        assertEquals(
            "lifecycle event replayed once, in order",
            listOf("androidOsRestartAccepted" to null),
            lifecycle,
        )
        assertEquals(
            "timeout event replayed once, in order",
            listOf("FGS timeout (type: remoteMessaging)"),
            timeouts,
        )
        assertEquals("drain must clear the queue", 0, NativeEventQueue.size)

        // A second drain (e.g. a second load()) must not re-deliver.
        BackgroundServicePlugin.drainQueuedNativeEvents()
        assertEquals("exactly-once: no re-delivery on a second drain", 1, lifecycle.size)
        assertEquals(1, timeouts.size)
    }

    @Test
    fun drainQueuedNativeEvents_emptyQueueIsANoop() {
        val lifecycle = mutableListOf<Pair<String, String?>>()
        BackgroundServicePlugin.onNativeLifecycleEvent = { t, f -> lifecycle += t to f }
        BackgroundServicePlugin.drainQueuedNativeEvents() // queue is empty
        assertTrue(lifecycle.isEmpty())
    }

    // ── Integration: a pre-load emit survives until drain ───────────────

    @Test
    fun preLoadEmit_survivesUntilPluginLoadDrains() {
        // Stand-in for a boot/timeout emit that happens before load(): no
        // callback is attached, so it must be queued, then drained once.
        assertNull(BackgroundServicePlugin.onNativeLifecycleEvent)
        BackgroundServicePlugin.emitNativeLifecycleEvent("androidBootRecoveryAccepted", null)
        assertEquals(1, NativeEventQueue.size)

        // Plugin load() attaches the callback and drains.
        val received = mutableListOf<String>()
        BackgroundServicePlugin.onNativeLifecycleEvent = { t, _ -> received += t }
        BackgroundServicePlugin.drainQueuedNativeEvents()

        assertEquals(
            "the pre-load event must reach the callback exactly once via drain",
            listOf("androidBootRecoveryAccepted"),
            received,
        )
        assertEquals(0, NativeEventQueue.size)
    }

    companion object {
        /** Reflect the queue's internal bound so the overflow test tracks the SUT. */
        fun visibleBound(): Int {
            val f = NativeEventQueue.javaClass.getDeclaredField("MAX_EVENTS")
            f.isAccessible = true
            return f.getInt(NativeEventQueue)
        }
    }
}
