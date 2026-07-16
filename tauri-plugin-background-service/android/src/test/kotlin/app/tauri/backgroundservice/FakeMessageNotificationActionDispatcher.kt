package app.tauri.backgroundservice

import android.content.Context

class FakeMessageNotificationActionDispatcher : MessageNotificationActionDispatcher {
    data class MarkReadCall(val chatId: String, val messageId: String, val notificationId: Int)
    data class ReplyCall(
        val chatId: String,
        val messageId: String,
        val notificationId: Int,
        val replyText: String,
    )

    val markedRead = mutableListOf<MarkReadCall>()
    val replies = mutableListOf<ReplyCall>()

    /** BGS-20 (doc-08 Step 11): the thread each dispatch ran on — the
     *  load-bearing capture for the off-main assertion (the existing fields
     *  recorded only call args, leaving "off main" with nothing to pin). */
    var markReadThread: Thread? = null
        private set
    var replyThread: Thread? = null
        private set

    override fun markRead(context: Context, chatId: String, messageId: String, notificationId: Int) {
        markReadThread = Thread.currentThread()
        markedRead += MarkReadCall(chatId, messageId, notificationId)
    }

    override fun reply(
        context: Context,
        chatId: String,
        messageId: String,
        notificationId: Int,
        replyText: String,
    ) {
        replyThread = Thread.currentThread()
        replies += ReplyCall(chatId, messageId, notificationId, replyText)
    }
}
