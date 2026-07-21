package app.tauri.backgroundservice

import android.util.Log
import java.util.ArrayDeque

/**
 * AND-05: a bounded, ordered, process-global queue for native lifecycle /
 * platform events emitted before the plugin's JS callback is attached.
 *
 * `LifecycleService` emits timeout / native-lifecycle / platform-error events
 * during boot and OS-restart recovery — paths that run with no Tauri webview and
 * therefore before [BackgroundServicePlugin.load] attaches its callback. Without
 * this queue those events were dropped (`callback?.invoke(...)` was a no-op), so
 * the Rust actor never learned the service timed out or recovered headlessly.
 *
 * The plugin drains the queue exactly once, in insertion order, when `load()`
 * attaches the callbacks. Bounded (drop-oldest + log on overflow) so a runaway
 * emitter cannot exhaust memory if the plugin never attaches.
 *
 * Process-global on purpose: Android services/receivers are process singletons,
 * and the boot/timeout emitters and the plugin share one process. Mirrors the
 * existing process-static callback slots on [BackgroundServicePlugin].
 */
internal sealed class QueuedNativeEvent {
    data class Lifecycle(val type: String, val fgsType: String?) : QueuedNativeEvent()
    data class Timeout(val errorMessage: String) : QueuedNativeEvent()
    data class PlatformError(val error: String) : QueuedNativeEvent()
}

internal object NativeEventQueue {
    private const val TAG = "NativeEventQueue"
    private const val MAX_EVENTS = 64
    private val lock = Any()
    private val queue = ArrayDeque<QueuedNativeEvent>()

    val size: Int get() = synchronized(lock) { queue.size }

    fun enqueue(event: QueuedNativeEvent) {
        synchronized(lock) {
            if (queue.size >= MAX_EVENTS) {
                val dropped = queue.pollFirst()
                Log.w(TAG, "queue full ($MAX_EVENTS); dropping oldest event: $dropped")
            }
            queue.addLast(event)
        }
    }

    /** Remove and return all queued events in insertion order; clears the queue. */
    fun drainAndClear(): List<QueuedNativeEvent> = synchronized(lock) {
        val out = queue.toList()
        queue.clear()
        out
    }

    /** Test isolation: clear without returning the contents. */
    fun resetForTest() {
        synchronized(lock) { queue.clear() }
    }
}
