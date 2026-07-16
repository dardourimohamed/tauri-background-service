package app.tauri.backgroundservice

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import androidx.annotation.VisibleForTesting

/**
 * Debounced active-network watcher feeding `Core::on_network_changed` (D4).
 *
 * Registers a [ConnectivityManager.NetworkCallback] for INTERNET-capable
 * networks and invokes [onNetworkChanged] on a dedicated background handler
 * thread — never the main looper (REQ-13; the bridge JNI call blocks on the
 * Rust runtime, F2-class deadlock risk on the main thread).
 *
 * Capability callbacks are noisy (signal, DNS and captive-portal changes). We
 * only notify Core when the *active* network fingerprint changes: network id,
 * validated/internet capability, or transport set. The leading-edge debounce
 * remains a second guard for real transition bursts.
 */
class ConnectivityMonitor(
    private val context: Context,
    private val onNetworkChanged: () -> Unit,
    private val debounceMs: Long = DEFAULT_DEBOUNCE_MS,
    private val clock: () -> Long = SystemClock::elapsedRealtime,
    // Nullable so the production default (postDelayed on backgroundHandler) can
    // be resolved against the instance at call time: Kotlin forbids referencing
    // an instance member like backgroundHandler in a constructor-parameter
    // default. Tests inject a non-null captor; production leaves this null and
    // [scheduleTrailingFire] falls back to the background handler.
    private val scheduler: ((delayMs: Long, action: () -> Unit) -> Unit)? = null,
) {
    companion object {
        private const val TAG = "ConnectivityMonitor"
        const val DEFAULT_DEBOUNCE_MS = 2_000L
        // CROSS-DOC: must exceed core/src/network_changed.rs
        // NETWORK_CHANGE_THROTTLE_WINDOW (5 s) so a trailing nudge clears
        // Core's rate limit instead of being re-throttled and dropped.
        private const val CORE_NETWORK_THROTTLE_WINDOW_MS = 5_000L
        private const val TRAILING_SLACK_MS = 250L
    }

    private val lock = Any()
    private var lastFireAtMs: Long? = null
    private var lastNetworkFingerprint: String? = null
    private var handlerThread: HandlerThread? = null
    private var backgroundHandler: Handler? = null
    private var callback: ConnectivityManager.NetworkCallback? = null
    // Generation guard for the single pending trailing fire. A fresh
    // leading-edge fire, a newer suppression, or unregister() bumps this so a
    // previously scheduled trailing action no-ops — a burst of distinct
    // fingerprints yields at most one trailing fire.
    private var trailingGeneration: Int = 0

    fun register() {
        synchronized(lock) {
            if (callback != null) return
            val thread = HandlerThread("sila-connectivity").apply { start() }
            val handler = Handler(thread.looper)
            // The scheduler default posts the trailing fire onto this handler;
            // capture it as a field so the default can reach it.
            backgroundHandler = handler
            // minSdk 24 predates the Handler-taking registerNetworkCallback
            // overload (API 26), so each event is posted onto our own
            // background handler instead.
            val cb = object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) {
                    handler.post { handleActiveNetworkChange() }
                }

                override fun onLost(network: Network) {
                    handler.post { handleActiveNetworkChange() }
                }

                override fun onCapabilitiesChanged(
                    network: Network,
                    networkCapabilities: NetworkCapabilities,
                ) {
                    handler.post { handleActiveNetworkChange() }
                }
            }
            val request = NetworkRequest.Builder()
                .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                .build()
            try {
                connectivityManager().registerNetworkCallback(request, cb)
            } catch (e: Exception) {
                Log.w(TAG, "registerNetworkCallback failed: ${e.message}")
                thread.quitSafely()
                return
            }
            handlerThread = thread
            callback = cb
            // Establish a baseline. Registering while a stable network is
            // already active must not make the first routine capability update
            // look like a connectivity recovery.
            lastNetworkFingerprint = activeNetworkFingerprint()
        }
    }

    fun unregister() {
        synchronized(lock) {
            callback?.let {
                try {
                    connectivityManager().unregisterNetworkCallback(it)
                } catch (e: Exception) {
                    Log.w(TAG, "unregisterNetworkCallback failed: ${e.message}")
                }
            }
            callback = null
            handlerThread?.quitSafely()
            handlerThread = null
            backgroundHandler = null
            // Invalidate any pending trailing fire so it cannot deliver a
            // nudge after the monitor is torn down.
            trailingGeneration++
        }
    }

    /**
     * Leading-edge debounce gate: fire unless a fire already happened within
     * [debounceMs]; suppressed events do not extend the window. Runs on the
     * monitor's background handler thread in production.
     */
    @VisibleForTesting
    internal fun handleNetworkEvent(fingerprint: String = "test-network") {
        val now = clock()
        synchronized(lock) {
            if (lastNetworkFingerprint == fingerprint) return
            lastNetworkFingerprint = fingerprint
            val last = lastFireAtMs
            if (last != null && now - last < debounceMs) {
                // Trailing edge: a NEW fingerprint was suppressed inside the
                // leading-edge debounce window. Schedule a single deferred
                // fire so the settled network state reaches Core instead of
                // being recorded-but-never-fired. Timed to land AFTER Core's
                // NETWORK_CHANGE_THROTTLE_WINDOW (see network_changed.rs) so
                // the trailing nudge is not itself re-throttled and dropped.
                val fireAt = last + maxOf(debounceMs, CORE_NETWORK_THROTTLE_WINDOW_MS + TRAILING_SLACK_MS)
                val generation = ++trailingGeneration
                scheduleTrailingFire(fireAt - now) { fireTrailing(generation) }
                return
            }
            // A fresh leading-edge fire supersedes any pending trailing fire
            // (a burst of distinct fingerprints yields at most one trailing).
            trailingGeneration++
            lastFireAtMs = now
        }
        fireFromNetworkChange()
    }

    /**
     * The deferred trailing fire. Runs on the background handler thread in
     * production (or whatever thread a test-injected scheduler dispatches on).
     * No-ops once a newer trailing schedule, a fresh leading fire, or
     * unregister() has bumped [trailingGeneration] past [generation].
     */
    private fun fireTrailing(generation: Int) {
        synchronized(lock) {
            if (trailingGeneration != generation) return
            lastFireAtMs = clock()
        }
        fireFromNetworkChange()
    }

    private fun fireFromNetworkChange() {
        // Exact marker consumed by scripts/verify-android.sh (D5): a fire only
        // means recipients were nudged/attempted, not that messages flushed.
        Log.i(TAG, "network changed -> notifyNetworkChanged")
        onNetworkChanged()
    }

    /**
     * Dispatches the trailing fire through the injected [scheduler] when one
     * was supplied (tests capture the action), otherwise through the background
     * handler's postDelayed (production).
     */
    private fun scheduleTrailingFire(delayMs: Long, action: () -> Unit) {
        val injected = scheduler
        if (injected != null) {
            injected(delayMs, action)
        } else {
            backgroundHandler?.postDelayed(action, delayMs)
        }
    }

    private fun handleActiveNetworkChange() {
        handleNetworkEvent(activeNetworkFingerprint())
    }

    /**
     * Only data that materially changes reachability participates in the
     * fingerprint. This intentionally excludes RSSI, bandwidth, DNS and other
     * capability values which can update many times per minute.
     */
    private fun activeNetworkFingerprint(): String {
        val manager = connectivityManager()
        val network = manager.activeNetwork ?: return "offline"
        val capabilities = manager.getNetworkCapabilities(network)
        val validated = capabilities?.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED) == true
        val internet = capabilities?.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) == true
        val transports = listOf(
            NetworkCapabilities.TRANSPORT_WIFI to "wifi",
            NetworkCapabilities.TRANSPORT_CELLULAR to "cellular",
            NetworkCapabilities.TRANSPORT_ETHERNET to "ethernet",
            NetworkCapabilities.TRANSPORT_VPN to "vpn",
        ).filter { capabilities?.hasTransport(it.first) == true }
            .joinToString(",") { it.second }
        return "${network}|validated=$validated|internet=$internet|transports=$transports"
    }

    @VisibleForTesting
    internal fun backgroundLooper(): Looper? = handlerThread?.looper

    private fun connectivityManager(): ConnectivityManager =
        context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
}
