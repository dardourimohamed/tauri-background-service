package app.tauri.backgroundservice

import android.content.Context
import org.json.JSONObject

class FakeCoreBridge(
    result: String = "running",
) : CoreBridge {

    var lastStartReason: String? = null
        private set

    var lastStopReason: String? = null
        private set

    /**
     * BGS-20 (doc-08 Step 11): the thread on which [stop] ran. Existing fakes
     * captured only call args, which left an "off main" assertion with nothing
     * to pin — this field is the load-bearing capture that makes
     * `bgs20_stop_off_main_thread` non-vacuous (see
     * `planner-android-off-main-thread-ac-thread-capture-and-cross-class-enumeration`).
     */
    var stopThread: Thread? = null
        private set

    var networkChangedCount = 0
        private set

    /** When set, [notifyNetworkChanged] throws it (e.g. UnsatisfiedLinkError). */
    var networkChangedError: Throwable? = null

    private val startResult: HeadlessBridgeResult = when (result) {
        "running", "setup_idle", "locked_idle" -> {
            val json = JSONObject().apply {
                put("ok", true)
                put("state", result)
            }.toString()
            HeadlessBridgeResult(ok = true, state = result, message = null, recoverable = true, rawJson = json)
        }
        "failed" -> {
            val json = JSONObject().apply {
                put("ok", false)
                put("state", "failed")
                put("message", "FakeCoreBridge configured failure")
                put("recoverable", true)
            }.toString()
            HeadlessBridgeResult(ok = false, state = "failed", message = "FakeCoreBridge configured failure", recoverable = true, rawJson = json)
        }
        else -> throw IllegalArgumentException("Unknown FakeCoreBridge state: $result. Use: running, setup_idle, locked_idle, failed")
    }

    override fun start(context: Context, reason: String): HeadlessBridgeResult {
        lastStartReason = reason
        return startResult
    }

    override fun stop(context: Context, reason: String): HeadlessBridgeResult {
        stopThread = Thread.currentThread()
        lastStopReason = reason
        val json = JSONObject().apply {
            put("ok", true)
            put("state", "stopped")
        }.toString()
        return HeadlessBridgeResult(ok = true, state = "stopped", message = null, recoverable = true, rawJson = json)
    }

    override fun notifyNetworkChanged(): HeadlessBridgeResult {
        networkChangedCount++
        networkChangedError?.let { throw it }
        val json = JSONObject().apply {
            put("ok", true)
            put("throttled", false)
            put("endpointNudged", true)
            put("peersFlushed", 0)
            put("controlFlushed", true)
        }.toString()
        return HeadlessBridgeResult(ok = true, state = "running", message = null, recoverable = true, rawJson = json)
    }
}
