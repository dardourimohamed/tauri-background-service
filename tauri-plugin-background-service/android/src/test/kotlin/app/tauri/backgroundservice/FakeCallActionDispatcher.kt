package app.tauri.backgroundservice

import android.content.Context

/**
 * Test double for [CallActionDispatcher] (M-NATIVE-1, Step 9).
 *
 * Records the `call_id`s routed to `answer_call` / `reject_call` / `end_call` so
 * the Robolectric gate can assert the action→core dispatch WITHOUT loading the
 * native lib — the masked seam the prior activity-launcher intents never reached.
 *
 * BGS-20 (doc-08 Step 11): each dispatch also captures the THREAD it ran on
 * (`answerThread` / `rejectThread` / `endThread`). The pre-existing fields
 * recorded only call args, which left an "off main" assertion with nothing to
 * pin — these captures are the load-bearing fixture that makes
 * `bgs20_call_action_off_main_thread` non-vacuous (see
 * `planner-android-off-main-thread-ac-thread-capture-and-cross-class-enumeration`).
 */
class FakeCallActionDispatcher : CallActionDispatcher {
    val answered = mutableListOf<String>()
    val rejected = mutableListOf<String>()
    val ended = mutableListOf<String>()

    var answerThread: Thread? = null
        private set
    var rejectThread: Thread? = null
        private set
    var endThread: Thread? = null
        private set

    override fun answerCall(context: Context, callId: String) {
        answerThread = Thread.currentThread()
        answered += callId
    }

    override fun rejectCall(context: Context, callId: String) {
        rejectThread = Thread.currentThread()
        rejected += callId
    }

    override fun endCall(context: Context, callId: String) {
        endThread = Thread.currentThread()
        ended += callId
    }
}
