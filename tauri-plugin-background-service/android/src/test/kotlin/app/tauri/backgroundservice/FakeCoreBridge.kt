package app.tauri.backgroundservice

import android.content.Context
import org.json.JSONObject

class FakeCoreBridge private constructor(
    private val startResult: HeadlessBridgeResult,
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

    constructor(result: String = "running") : this(startResultFor(result))

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

    companion object {
        // AND-03: the success states remain as named fixtures for the existing
        // CoreBridgeTest cases, but they now set `ok=true` and rely on
        // `accepted = ok` (state is opaque diagnostics, never the accept gate).
        private fun startResultFor(result: String): HeadlessBridgeResult = when (result) {
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

        /**
         * AND-03: build a fake whose start result carries an arbitrary
         * `ok`/`state` pair, so tests can assert that `ok=true,state="degraded"`
         * is accepted (and `ok=false` rejected) — `state` is opaque diagnostics,
         * never the accept discriminator. Mirrors a host core that returns an
         * unknown-but-successful state.
         */
        fun okState(ok: Boolean, state: String, recoverable: Boolean = true): FakeCoreBridge {
            val json = JSONObject().apply {
                put("ok", ok)
                put("state", state)
                put("recoverable", recoverable)
            }.toString()
            return FakeCoreBridge(HeadlessBridgeResult(ok, state, null, recoverable, json))
        }
    }
}
