package app.tauri.backgroundservice

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import androidx.annotation.VisibleForTesting
import androidx.core.app.RemoteInput

interface MessageNotificationActionDispatcher {
    fun markRead(context: Context, chatId: String, messageId: String, notificationId: Int)
    fun reply(
        context: Context,
        chatId: String,
        messageId: String,
        notificationId: Int,
        replyText: String,
    )
}

class RealMessageNotificationActionDispatcher : MessageNotificationActionDispatcher {
    override fun markRead(context: Context, chatId: String, messageId: String, notificationId: Int) {
        dispatch(context, ActionableMessageNotifier.ACTION_MARK_READ, chatId, messageId, notificationId, "")
    }

    override fun reply(
        context: Context,
        chatId: String,
        messageId: String,
        notificationId: Int,
        replyText: String,
    ) {
        dispatch(context, ActionableMessageNotifier.ACTION_REPLY, chatId, messageId, notificationId, replyText)
    }

    private fun dispatch(
        context: Context,
        action: String,
        chatId: String,
        messageId: String,
        notificationId: Int,
        replyText: String,
    ) {
        val result = HeadlessCoreBridge.performNotificationAction(context, action, chatId, messageId, replyText)
        if (!result.ok) {
            Log.w(TAG, "notification action '$action' failed for chat=$chatId message=$messageId: ${result.message}")
        }
        // NTF-04 (Step 7b): on a RECOVERABLE failure (locked/dead Core a retry may
        // satisfy) RE-PRESENT the notification preserving replyText — otherwise a
        // reply typed while the Core is locked/dead is LOST SILENTLY. Cancel only
        // on success or permanent failure (see decideNotificationOutcome's anti-loop
        // `code` discriminator; the JNI bridge call stays here so the apply layer is
        // Robolectric-testable without loading sila_lib).
        handleNotificationActionResult(
            context,
            decideNotificationOutcome(result, replyText),
            chatId,
            messageId,
            notificationId,
        )
    }

    companion object {
        private const val TAG = "MessageNotificationAction"
    }
}

object MessageNotificationActionDispatch {
    @Volatile
    var dispatcher: MessageNotificationActionDispatcher = RealMessageNotificationActionDispatcher()
}

class MessageNotificationActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val chatId = intent.getStringExtra(ActionableMessageNotifier.EXTRA_CHAT_ID) ?: return
        val messageId = intent.getStringExtra(ActionableMessageNotifier.EXTRA_MESSAGE_ID) ?: return
        val action = intent.getStringExtra(ActionableMessageNotifier.EXTRA_ACTION) ?: return
        val notificationId = intent.getIntExtra(ActionableMessageNotifier.EXTRA_NOTIFICATION_ID, Int.MIN_VALUE)
        if (notificationId == Int.MIN_VALUE) return

        // BGS-20 (doc-08 Step 11): a SINGLE goAsync() wraps BOTH the reply and
        // mark_read branches (one PendingResult, one executor dispatch covering
        // whichever branch the action selects). Both reach
        // HeadlessCoreBridge.performNotificationAction → lib.rs block_on
        // (notification_action) — the same ANR class as reply (the Rust export
        // handles both); BGS-20 evidence named only reply but markRead is in
        // scope. Only the dispatcher.* call moves off main; the RemoteInput
        // extraction stays in the reply branch. PendingResult.finish() runs
        // exactly once in the finally (and on exception, so it is never leaked).
        val pendingResult = pendingResultOrNoop()
        actionExecutor("sila-message-action") {
            try {
                when (action) {
                    ActionableMessageNotifier.ACTION_MARK_READ -> {
                        MessageNotificationActionDispatch.dispatcher.markRead(
                            context,
                            chatId,
                            messageId,
                            notificationId,
                        )
                    }
                    ActionableMessageNotifier.ACTION_REPLY -> {
                        val input = RemoteInput.getResultsFromIntent(intent)
                            ?.getCharSequence(ActionableMessageNotifier.REMOTE_INPUT_KEY)
                            ?.toString()
                            ?.trim()
                            .orEmpty()
                        if (input.isNotEmpty()) {
                            MessageNotificationActionDispatch.dispatcher.reply(
                                context,
                                chatId,
                                messageId,
                                notificationId,
                                input,
                            )
                        }
                    }
                    else -> Log.w(TAG, "unknown message notification action '$action'")
                }
            } catch (t: Throwable) {
                Log.e(TAG, "message notification action dispatch failed (action='$action')", t)
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
        private const val TAG = "MessageNotificationReceiver"
        const val ACTION_MESSAGE_NOTIFICATION = "app.tauri.backgroundservice.MESSAGE_NOTIFICATION"

        /**
         * BGS-20 (doc-08 Step 11): runs the reply/mark_read dispatch on a worker
         * so the JNI hop stays off the main looper. Default spawns a real thread
         * (fire-and-forget); tests inject an inline executor for determinism,
         * except the off-main test which installs a thread-distinguishing one.
         */
        internal val DEFAULT_ACTION_EXECUTOR: (String, () -> Unit) -> Unit = { name, task ->
            Thread({ task() }, name).start()
        }

        @VisibleForTesting
        internal var actionExecutor: (String, () -> Unit) -> Unit = DEFAULT_ACTION_EXECUTOR
    }
}
