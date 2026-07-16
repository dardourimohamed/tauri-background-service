package app.tauri.backgroundservice

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import androidx.annotation.VisibleForTesting

/**
 * Native ring Answer/Decline → Rust control plane (spec 08 / M-NATIVE-1, Step 9).
 *
 * The injectable seam that routes a notification/Telecom call action to the
 * in-process headless Core. Default = [RealCallActionDispatcher] (JNI). The
 * Robolectric host gate swaps in a fake to assert the action→core dispatch
 * (`answer_call`/`reject_call`) WITHOUT loading the native lib — the masked seam
 * the prior activity-launcher intents never reached.
 */
interface CallActionDispatcher {
    fun answerCall(context: Context, callId: String)
    fun rejectCall(context: Context, callId: String)
    fun endCall(context: Context, callId: String)
}

/**
 * Production dispatcher: routes to the in-process headless Core via the JNI
 * bridge ([HeadlessCoreBridge.performCallAction]). The Core runs in this process
 * under the call FGS — it received the offer that rang the device — so the hop
 * reaches the same Core with **no webview loaded**, while the device is locked.
 * A failure surfaces as a logged diagnostic + terminal reason (non-payload).
 */
class RealCallActionDispatcher : CallActionDispatcher {
    override fun answerCall(context: Context, callId: String) = dispatch(callId, "answer")
    override fun rejectCall(context: Context, callId: String) = dispatch(callId, "reject")
    override fun endCall(context: Context, callId: String) = dispatch(callId, "end")

    private fun dispatch(callId: String, action: String) {
        val result = HeadlessCoreBridge.performCallAction(callId, action)
        if (!result.ok) {
            Log.w(TAG, "callAction '$action' for $callId did not reach core: ${result.message}")
        }
    }

    companion object {
        private const val TAG = "CallActionDispatcher"
    }
}

/**
 * Process-level holder for the active [CallActionDispatcher].
 *
 * A [BroadcastReceiver] is framework-instantiated (no DI), so the injectable
 * seam lives here — mirrors `BackgroundServicePlugin`'s companion-level
 * `onTimeoutEvent` injection. The Robolectric gate overrides [dispatcher] with a
 * fake; production keeps the JNI [RealCallActionDispatcher].
 */
object CallActionDispatch {
    @Volatile
    var dispatcher: CallActionDispatcher = RealCallActionDispatcher()
}

/**
 * Receives the notification Answer/Decline broadcast and drives the Rust control
 * plane (M-NATIVE-1, Step 9).
 *
 * This is the load-bearing native→core binding: the notification's Answer/Decline
 * `PendingIntent`s ([IncomingCallNotifier.callActionPendingIntent]) are now
 * `getBroadcast` intents targeting this receiver (previously `getActivity`
 * launchers that merely opened the app). `onReceive` runs while the device is
 * locked / the webview is closed, reads the carried `call_id` + action, dispatches
 * to the core (`answer_call`/`reject_call`) via the injectable seam, then cancels
 * the ring.
 */
class CallActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val callId = intent.getStringExtra(IncomingCallNotifier.EXTRA_CALL_ID) ?: return
        val action = intent.getStringExtra(IncomingCallNotifier.EXTRA_CALL_ACTION) ?: return

        if (action != IncomingCallNotifier.ACTION_ANSWER && action != IncomingCallNotifier.ACTION_DECLINE) {
            Log.w(TAG, "unknown call action '$action' (callId=$callId)")
            return
        }

        // BGS-20 (doc-08 Step 11): move the call-action JNI hop OFF the main
        // looper. `BroadcastReceiver.onReceive` runs on the main thread; the
        // dispatcher hop reaches HeadlessCoreBridge.performCallAction → lib.rs
        // block_on(call_action) (incl. a fresh QUIC dial), which ANRs if it runs
        // inline while the user taps Answer/Decline from the lock-screen
        // notification. goAsync() acquires the broadcast's PendingResult so the
        // system keeps the receiver alive while a worker runs the JNI;
        // PendingResult.finish() is called exactly once in the finally below
        // (and on exception, so it is never leaked). Mirrors LifecycleService's
        // coreStopExecutor discipline (Step 11 Task 1). The notification-answer→
        // Telecom bridge (markCallActive) + ring cancel stay inside the same
        // goAsync block for a consistent answer path; the off-main test asserts
        // only the dispatcher thread.
        //
        // The action→core routing remains the masked seam (Step 9, AC5): no-op
        // answerCall/rejectCall and the routing test goes RED while
        // load_registersSelfManagedPhoneAccount stays GREEN.
        val pendingResult = pendingResultOrNoop()
        actionExecutor("sila-call-action") {
            try {
                when (action) {
                    IncomingCallNotifier.ACTION_ANSWER -> {
                        CallActionDispatch.dispatcher.answerCall(context, callId)
                        // M-NATIVE-3 (Step 11): bridge the notification-answer
                        // (our primary answer surface) to the live Telecom
                        // connection so it goes ACTIVE. No-op when no self-managed
                        // connection is live for this call.
                        SilaConnectionService.markCallActive(callId)
                    }
                    IncomingCallNotifier.ACTION_DECLINE -> CallActionDispatch.dispatcher.rejectCall(context, callId)
                }
                // The ring is consumed by the action — clear the CallStyle
                // notification.
                IncomingCallNotifier.cancel(context, callId)
                Log.i(TAG, "dispatched '$action' for callId=$callId")
            } catch (t: Throwable) {
                Log.e(TAG, "call action dispatch failed for callId=$callId action='$action'", t)
            } finally {
                pendingResult?.finish()
            }
        }
    }

    /**
     * goAsync() can return null or throw when the broadcast has already been
     * finalized (a second call, or direct unit-test invocation with no framework
     * PendingResult). Treat that as "no async lifecycle to manage" — the dispatch
     * still runs on the executor; finish() is a guarded no-op for null.
     */
    private fun pendingResultOrNoop(): BroadcastReceiver.PendingResult? =
        runCatching { goAsync() }.getOrNull()

    companion object {
        private const val TAG = "CallActionReceiver"

        /** Explicit broadcast action carried by the notification Answer/Decline intents. */
        const val ACTION_CALL_ACTION = "app.tauri.backgroundservice.CALL_ACTION"

        /**
         * BGS-20 (doc-08 Step 11): runs the call-action dispatch on a worker so
         * the JNI hop stays off the main looper. Default spawns a real thread
         * (fire-and-forget, like LifecycleService.coreStopExecutor); tests inject
         * an inline executor for determinism, except the off-main test which
         * installs a thread-distinguishing executor.
         */
        internal val DEFAULT_ACTION_EXECUTOR: (String, () -> Unit) -> Unit = { name, task ->
            Thread({ task() }, name).start()
        }

        @VisibleForTesting
        internal var actionExecutor: (String, () -> Unit) -> Unit = DEFAULT_ACTION_EXECUTOR
    }
}
