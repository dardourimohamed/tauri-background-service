package app.tauri.backgroundservice

import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.util.Log
import androidx.annotation.VisibleForTesting
import androidx.core.app.NotificationCompat
import androidx.core.app.Person
import androidx.core.app.RemoteInput

object ActionableMessageNotifier {
    private const val TAG = "ActionableMessageNotifier"
    const val MESSAGE_CHANNEL_ID = "messages"

    const val EXTRA_NOTIFICATION_ID = "sila.notification_id"
    const val EXTRA_CHAT_ID = "sila.chat_id"
    const val EXTRA_MESSAGE_ID = "sila.message_id"
    const val EXTRA_ACTION = "sila.notification_action"
    const val EXTRA_ROUTE_URI = "sila.route_uri"
    const val REMOTE_INPUT_KEY = "sila.reply_text"

    const val ACTION_REPLY = "reply"
    const val ACTION_MARK_READ = "mark_read"

    /** Base id for headless per-chat message notifications (BGS-07, Step 10). */
    const val MESSAGE_NOTIFICATION_BASE = 9200

    /**
     * Stable, non-negative per-chat message notification id (BGS-07, Step 10).
     * Coalesces per chat so successive headless messages replace rather than
     * stack. Sits above the call-notification range so the two never clash.
     */
    fun notificationIdFor(chatId: String): Int {
        val hash = chatId.hashCode()
        return MESSAGE_NOTIFICATION_BASE + (if (hash < 0) hash.inv() else hash)
    }

    // NTF-13 (Step 9b): id of the per-chat group SUMMARY notification, posted
    // under (chatTag, SUMMARY_NOTIFICATION_ID). Fixed at 0 — collision-safe vs
    // the chat-stable MESSAGE id, which Step 9a's `notification_id_for`
    // (`((hash & 0x3fff_ffff) as i32) + 10_000`, tauri/src/event_bridge.rs) keeps
    // >= 10_000. (chatTag, 0) is unique per chat because chatTag is per-chat.
    const val SUMMARY_NOTIFICATION_ID = 0

    fun showMessageNotification(
        context: Context,
        notificationId: Int,
        chatId: String,
        messageId: String,
        title: String,
        body: String,
        routeUri: String,
        smallIcon: Int,
        launchIntent: Intent?,
    ) {
        // BGS-19 (doc-08 Step 16 T2): channel display name + description are
        // localized from the Rust-persisted locale store (default "en").
        val locale = LocaleStore.load(context)
        NotificationHelper.ensureChannel(
            context,
            MESSAGE_CHANNEL_ID,
            NotificationStrings.lookup("channel_messages", locale),
            NotificationManager.IMPORTANCE_DEFAULT,
            description = NotificationStrings.lookup("channel_messages_desc", locale),
            showBadge = true,
        )

        // NTF-13 (Step 9b): stable per-chat TAG. Two messages in the same chat
        // share one chat-stable id (Step 9a, Rust `chat_only_key`) AND one tag,
        // so a later post REPLACES the earlier one (update, not stack); 9c
        // cancels the whole chat by this tag. Derived locally from chatId — no
        // Rust tag plumbing. Shared tag + distinct ids (message vs summary) also
        // let 9c's single tag-filter catch both the message and the summary.
        val chatTag = chatTagFor(chatId)

        // MessagingStyle (NTF-13): conversation presentation,
        // latest-message-only. ActionableMessageNotifier is stateless (no
        // message store), so the style holds the single current message —
        // sufficient for the per-chat replace intent. A real conversation
        // history is a Step 11/13 carry-forward (needs a message store, which
        // would also un-suppress the summary under replace).
        val sender = Person.Builder().setName(title).build()
        val style = NotificationCompat.MessagingStyle(sender)
            .setConversationTitle(title)
            .addMessage(body, System.currentTimeMillis(), sender)

        val notification = NotificationCompat.Builder(context, MESSAGE_CHANNEL_ID)
            .setStyle(style)
            .setSmallIcon(smallIcon)
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .setAutoCancel(true)
            .setOnlyAlertOnce(true)
            .setGroup(chatTag)
            .setContentIntent(openPendingIntent(context, notificationId, routeUri, launchIntent))
            .addAction(replyAction(context, notificationId, chatId, messageId, routeUri, smallIcon))
            .addAction(markReadAction(context, notificationId, chatId, messageId, routeUri, smallIcon))
            .build()

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        // Post the MESSAGE under (chatTag, notificationId); post a per-chat
        // SUMMARY under (chatTag, SUMMARY_NOTIFICATION_ID). Shared tag +
        // distinct ids => same-chat messages replace, and 9c cancels both by tag.
        nm.notify(chatTag, notificationId, notification)
        nm.notify(
            chatTag,
            SUMMARY_NOTIFICATION_ID,
            groupSummaryNotification(context, chatTag, title, smallIcon),
        )
        Log.i(TAG, "showMessageNotification: id=$notificationId tag=$chatTag chatId=$chatId messageId=$messageId")
    }

    /**
     * NTF-13 (Step 9b repair): dismiss the chat's MESSAGE notification. Step 9b posts the
     * message under (chatTag, notificationId); NotificationManager.cancel(int) is
     * tag-SENSITIVE (it delegates to cancel(null, id)), so the id-only cancel that predated
     * the tag-keyed posting silently no-opped on a (chatTag, id) post — a successful
     * reply/mark-read stopped dismissing the notification. This overload cancels by
     * (chatTag, id), restoring the dismissal.
     *
     * The per-chat SUMMARY (chatTag, SUMMARY_NOTIFICATION_ID) is intentionally NOT canceled
     * here: in production AOSP auto-removes a setGroupSummary(true) summary once its last
     * child is canceled, so dismissing the message clears both. Explicit cancel-all-by-tag
     * (cancels the summary too, robust across every Android version) is [cancelChat]'s scope
     * (Step 9c) — call [cancelChat] from a shared branch that should clear the whole chat.
     */
    fun cancel(context: Context, chatId: String, notificationId: Int) {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.cancel(chatTagFor(chatId), notificationId)
    }

    /**
     * NTF-13 (Step 9c): dismiss ALL of a chat's notifications — the MESSAGE and the per-chat
     * SUMMARY — by enumerating this app's OWN active notifications that share the chat's tag
     * and canceling each through [cancel]. mark-read is the motivating case
     * (headless_core.rs `mark_read` => core.mark_messages_read(chat_id) marks the WHOLE
     * chat), but the Cancel branch is shared by reply-success and permanent-failure too;
     * clearing the chat's own <=1 message + summary in all those cases is intentional.
     *
     * This makes the SUMMARY dismissal EXPLICIT and version-robust: production AOSP
     * auto-removes a setGroupSummary(true) summary once its last child is canceled, but that
     * behavior is NOT guaranteed across OEM skins; cancelChat clears both unconditionally.
     * getActiveNotifications is the NotificationManager INSTANCE method (API 23; minSdk 24
     * >= 23) returning ONLY this app's OWN posted notifications (no
     * NotificationListenerService / BIND_NOTIFICATION_LISTENER permission), so this is
     * tag-scoped, never a cancelAll-equivalent across chats. The exact `== chatTag` match on
     * "chat:$chatId" avoids prefix collision; `filter` materializes before the
     * forEach-cancel so the iteration is CME-safe. Delegates each
     * per-(tag,id) dismissal to [cancel], keeping [cancel] a LIVE prod primitive (no orphan).
     */
    fun cancelChat(context: Context, chatId: String) {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val chatTag = chatTagFor(chatId)
        nm.activeNotifications
            .filter { it.tag == chatTag }
            .forEach { cancel(context, chatId, it.id) }
    }

    /** NTF-13: stable per-chat tag derived from chatId ("chat:<chatId>"). */
    @VisibleForTesting
    internal fun chatTagFor(chatId: String): String = "chat:$chatId"

    /**
     * NTF-13: per-chat group summary, posted under (chatTag, SUMMARY_NOTIFICATION_ID)
     * so it shares the chat's tag. Under AOSP, with only one non-summary child per
     * chat (same-chat messages replace via the shared chat-stable id), the summary
     * is visually suppressed; it is retained because (a) it satisfies the Step 9
     * group-summary AC, (b) it is the cancel target 9c dismisses by tag, and
     * (c) it is reachable (nm.notify IS called) — not dead code.
     */
    private fun groupSummaryNotification(
        context: Context,
        chatTag: String,
        title: String,
        smallIcon: Int,
    ): android.app.Notification =
        NotificationCompat.Builder(context, MESSAGE_CHANNEL_ID)
            .setSmallIcon(smallIcon)
            .setContentTitle(title)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .setGroup(chatTag)
            .setGroupSummary(true)
            .setAutoCancel(true)
            .build()

    private fun openPendingIntent(
        context: Context,
        notificationId: Int,
        routeUri: String,
        launchIntent: Intent?,
    ): PendingIntent {
        val base = launchIntent ?: context.packageManager.getLaunchIntentForPackage(context.packageName)
            ?: Intent()
        val intent = Intent(base).apply {
            action = Intent.ACTION_VIEW
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
            data = Uri.parse(routeUri)
            addCategory(Intent.CATEGORY_BROWSABLE)
            putExtra(EXTRA_ROUTE_URI, routeUri)
        }
        return PendingIntent.getActivity(
            context,
            notificationId,
            intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
    }

    private fun replyAction(
        context: Context,
        notificationId: Int,
        chatId: String,
        messageId: String,
        routeUri: String,
        smallIcon: Int,
    ): NotificationCompat.Action {
        val intent = actionIntent(context, notificationId, chatId, messageId, routeUri, ACTION_REPLY)
        val pendingIntent = PendingIntent.getBroadcast(
            context,
            notificationId + ACTION_REPLY.hashCode(),
            intent,
            PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        // BGS-19 (doc-08 Step 16 T2): reply action label localized (default "Reply").
        val replyLabel = NotificationStrings.lookup("reply", LocaleStore.load(context))
        val remoteInput = RemoteInput.Builder(REMOTE_INPUT_KEY)
            .setLabel(replyLabel)
            .build()
        return NotificationCompat.Action.Builder(smallIcon, replyLabel, pendingIntent)
            .addRemoteInput(remoteInput)
            .build()
    }

    private fun markReadAction(
        context: Context,
        notificationId: Int,
        chatId: String,
        messageId: String,
        routeUri: String,
        smallIcon: Int,
    ): NotificationCompat.Action {
        val intent = actionIntent(context, notificationId, chatId, messageId, routeUri, ACTION_MARK_READ)
        val pendingIntent = PendingIntent.getBroadcast(
            context,
            notificationId + ACTION_MARK_READ.hashCode(),
            intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        // BGS-19 (doc-08 Step 16 T2): mark-read action label localized.
        val markReadLabel = NotificationStrings.lookup("mark_as_read", LocaleStore.load(context))
        return NotificationCompat.Action.Builder(smallIcon, markReadLabel, pendingIntent).build()
    }

    private fun actionIntent(
        context: Context,
        notificationId: Int,
        chatId: String,
        messageId: String,
        routeUri: String,
        action: String,
    ): Intent {
        return Intent(context, MessageNotificationActionReceiver::class.java).apply {
            this.action = MessageNotificationActionReceiver.ACTION_MESSAGE_NOTIFICATION
            putExtra(EXTRA_NOTIFICATION_ID, notificationId)
            putExtra(EXTRA_CHAT_ID, chatId)
            putExtra(EXTRA_MESSAGE_ID, messageId)
            putExtra(EXTRA_ROUTE_URI, routeUri)
            putExtra(EXTRA_ACTION, action)
        }
    }
}
