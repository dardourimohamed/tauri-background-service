package app.tauri.backgroundservice

import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.service.notification.StatusBarNotification
import androidx.core.app.NotificationCompat
import androidx.core.app.RemoteInput
import androidx.test.core.app.ApplicationProvider
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
class ActionableMessageNotifierTest {
    private lateinit var context: Context

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
        // Run MessageNotificationActionReceiver's dispatch INLINE so its
        // direct-invoke tests (receiver_markRead / receiver_reply) observe the
        // post-dispatch state deterministically. The default spawns a real worker
        // (fire-and-forget) which would race the assertions; the off-main test
        // lives in Bgs20OffMainThreadTest and installs its own
        // thread-distinguishing executor (BGS-20, Step 11).
        MessageNotificationActionReceiver.actionExecutor = { _, task -> task() }
    }

    @After
    fun tearDown() {
        MessageNotificationActionReceiver.actionExecutor =
            MessageNotificationActionReceiver.DEFAULT_ACTION_EXECUTOR
        MessageNotificationActionDispatch.dispatcher = RealMessageNotificationActionDispatcher()
    }

    @Test
    @Config(sdk = [34])
    fun showMessageNotification_postsActionableMessageNotification() {
        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = 44001,
            chatId = "chat-1",
            messageId = "msg-1",
            title = "Alice",
            body = "hello",
            routeUri = "bg-service://chat?chat_id=chat-1&message_id=msg-1",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = Intent(Intent.ACTION_MAIN).setPackage(context.packageName),
        )

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val chatTag = "chat:chat-1"

        // NTF-13 (Step 9b): posted under the per-chat TAG (chatTag, id), not id-only.
        val messageSbn = sbnFor(nm, chatTag, 44001)
        assertNotNull(messageSbn)
        val notification = messageSbn!!.notification

        assertEquals(android.app.Notification.CATEGORY_MESSAGE, notification.category)
        assertEquals(2, notification.actions?.size ?: 0)
        // setGroup(chatTag)
        assertEquals(chatTag, notification.group)
        // MessagingStyle (latest-message-only)
        assertNotNull(
            NotificationCompat.MessagingStyle.extractMessagingStyleFromNotification(notification),
        )

        val contentIntent = shadowOf(notification.contentIntent).savedIntent
        assertEquals(Intent.ACTION_VIEW, contentIntent.action)
        assertEquals("bg-service://chat?chat_id=chat-1&message_id=msg-1", contentIntent.data.toString())
        assertTrue(contentIntent.categories?.contains(Intent.CATEGORY_BROWSABLE) == true)
        assertEquals(
            "bg-service://chat?chat_id=chat-1&message_id=msg-1",
            contentIntent.getStringExtra(ActionableMessageNotifier.EXTRA_ROUTE_URI),
        )

        val replyAction = notification.actions[0]
        val markReadAction = notification.actions[1]
        assertEquals("Reply", replyAction.title.toString())
        assertEquals("Mark as read", markReadAction.title.toString())
        assertTrue(shadowOf(replyAction.actionIntent).isBroadcastIntent)
        assertTrue(shadowOf(markReadAction.actionIntent).isBroadcastIntent)
        assertEquals(
            MessageNotificationActionReceiver::class.java.name,
            shadowOf(replyAction.actionIntent).savedIntent.component?.className,
        )
        assertEquals(
            MessageNotificationActionReceiver::class.java.name,
            shadowOf(markReadAction.actionIntent).savedIntent.component?.className,
        )

        // Per-chat group SUMMARY under (chatTag, SUMMARY_NOTIFICATION_ID), distinct from the message.
        val summarySbn = sbnFor(nm, chatTag, ActionableMessageNotifier.SUMMARY_NOTIFICATION_ID)
        assertNotNull(summarySbn)
        assertNotSame(notification, summarySbn!!.notification)
        assertTrue(NotificationCompat.isGroupSummary(summarySbn.notification))
    }

    @Test
    @Config(sdk = [34])
    fun showMessageNotification_twoSameChatPostsUpdateNotStack() {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val chatTag = "chat:chat-1"

        // Two messages in the same chat share the chat-stable id (Step 9a) and tag,
        // so the second post must REPLACE the first (update), not stack a second one.
        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = 44001,
            chatId = "chat-1",
            messageId = "msg-1",
            title = "Alice",
            body = "first",
            routeUri = "bg-service://chat?chat_id=chat-1&message_id=msg-1",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = null,
        )
        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = 44001,
            chatId = "chat-1",
            messageId = "msg-2",
            title = "Alice",
            body = "second",
            routeUri = "bg-service://chat?chat_id=chat-1&message_id=msg-2",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = null,
        )

        // Exactly ONE message notification under (chatTag, 44001) — replaced, not stacked.
        val messages = nm.activeNotifications.filter { it.tag == chatTag && it.id == 44001 }
        assertEquals(1, messages.size)
        // Latest body wins (MessagingStyle, latest-message-only).
        val style = NotificationCompat.MessagingStyle.extractMessagingStyleFromNotification(
            messages.single().notification,
        )
        assertNotNull(style)
        assertEquals("second", style!!.messages.last().text.toString())
        // Message + summary => exactly two active notifications for the chat.
        assertEquals(2, nm.activeNotifications.count { it.tag == chatTag })
    }

    @Test
    @Config(sdk = [34])
    fun showMessageNotification_differentChatsGetDifferentTags() {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = 44001,
            chatId = "chat-1",
            messageId = "msg-1",
            title = "Alice",
            body = "hi",
            routeUri = "bg-service://chat?chat_id=chat-1&message_id=msg-1",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = null,
        )
        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = 44002,
            chatId = "chat-2",
            messageId = "msg-9",
            title = "Bob",
            body = "yo",
            routeUri = "bg-service://chat?chat_id=chat-2&message_id=msg-9",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = null,
        )

        val tags = nm.activeNotifications.map { it.tag }.toSet()
        assertTrue(tags.contains("chat:chat-1"))
        assertTrue(tags.contains("chat:chat-2"))
        // Each chat's message + summary share that chat's distinct tag.
        assertEquals(2, nm.activeNotifications.count { it.tag == "chat:chat-1" })
        assertEquals(2, nm.activeNotifications.count { it.tag == "chat:chat-2" })
    }

    @Test
    @Config(sdk = [34])
    fun cancel_dismissesTagKeyedMessageNotification() {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val chatTag = "chat:chat-1"

        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = 44001,
            chatId = "chat-1",
            messageId = "msg-1",
            title = "Alice",
            body = "hello",
            routeUri = "bg-service://chat?chat_id=chat-1&message_id=msg-1",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = null,
        )
        assertNotNull(sbnFor(nm, chatTag, 44001))

        // NTF-13 (Step 9b repair): cancel is TAG-AWARE — it dismisses the message posted
        // under (chatTag, id). The pre-9b id-only nm.cancel(id) was a silent no-op on a
        // tag-keyed post (NotificationManager.cancel(int) delegates to cancel(null, id),
        // which is tag-sensitive); this overload cancels (chatTag, id) so a successful
        // reply/mark-read dismisses the message again. The per-chat SUMMARY
        // (chatTag, SUMMARY_NOTIFICATION_ID) is NOT canceled here: in production AOSP
        // auto-removes a setGroupSummary(true) summary once its last child is canceled, so
        // dismissing the message clears both; explicit cancel-all-by-tag (robust across
        // every Android version) is Step 9c's cancelChat scope.
        ActionableMessageNotifier.cancel(context, "chat-1", 44001)

        assertNull(sbnFor(nm, chatTag, 44001))
    }

    @Test
    @Config(sdk = [34])
    fun handleNotificationActionResult_cancelClearsWholeChatByTag_notJustTappedId() {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val chatTagA = "chat:chat-A"

        // chat-A: showMessageNotification posts BOTH the message (chat:chat-A, 44001) AND
        // the per-chat summary (chat:chat-A, SUMMARY_NOTIFICATION_ID=0).
        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = 44001,
            chatId = "chat-A",
            messageId = "m",
            title = "Alice",
            body = "hello",
            routeUri = "bg-service://chat?chat_id=chat-A&message_id=m",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = null,
        )
        // chat-B: proves the clear is tag-SCOPED, not cancelAll.
        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = 44002,
            chatId = "chat-B",
            messageId = "m",
            title = "Bob",
            body = "yo",
            routeUri = "bg-service://chat?chat_id=chat-B&message_id=m",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = null,
        )
        // Precondition: chat-A carries its message + summary (two notifications).
        assertEquals(2, nm.activeNotifications.count { it.tag == chatTagA })

        // NTF-13 (Step 9c): the Cancel branch clears the WHOLE chat by tag — message AND
        // summary — not just the tapped id. mark_messages_read marks the whole chat
        // (headless_core.rs:309); the Cancel outcome is shared by reply-success,
        // mark-read-success, and permanent-failure. Driving through the BRANCH (not
        // cancelChat directly) is load-bearing: reverting the branch to the 9b single-id
        // cancel leaves the summary (chatTag, 0) alive — Robolectric's flat store does NOT
        // emulate AOSP group-summary auto-removal — so none{tag==chatTagA} would RED,
        // pinning the summary dismissal (the sole load-bearing differentiator).
        handleNotificationActionResult(
            context,
            NotificationActionOutcome.Cancel,
            "chat-A",
            "m",
            44001,
        )

        // ZERO remain for chat-A — the message AND the summary are dismissed.
        assertTrue(
            "Cancel must clear the chat's message AND summary (tag-scoped, not just the tapped id)",
            nm.activeNotifications.none { it.tag == chatTagA },
        )
        // Cross-chat isolation: chat-B survives (proves tag-scoped, NOT cancelAll).
        assertTrue(
            "Cancel must NOT clear other chats (tag-scoped, not cancelAll)",
            nm.activeNotifications.any { it.tag == "chat:chat-B" },
        )
    }

    /** Tag+id keyed retrieval mirroring the production `nm.notify(tag, id, n)`. */
    private fun sbnFor(nm: NotificationManager, tag: String, id: Int): StatusBarNotification? =
        nm.activeNotifications.firstOrNull { it.tag == tag && it.id == id }

    @Test
    fun receiver_markReadDispatchesToCoreAction() {
        val fake = FakeMessageNotificationActionDispatcher()
        MessageNotificationActionDispatch.dispatcher = fake

        MessageNotificationActionReceiver().onReceive(
            context,
            Intent(MessageNotificationActionReceiver.ACTION_MESSAGE_NOTIFICATION).apply {
                putExtra(ActionableMessageNotifier.EXTRA_NOTIFICATION_ID, 77)
                putExtra(ActionableMessageNotifier.EXTRA_CHAT_ID, "chat-1")
                putExtra(ActionableMessageNotifier.EXTRA_MESSAGE_ID, "msg-1")
                putExtra(
                    ActionableMessageNotifier.EXTRA_ACTION,
                    ActionableMessageNotifier.ACTION_MARK_READ,
                )
            },
        )

        assertEquals(
            listOf(FakeMessageNotificationActionDispatcher.MarkReadCall("chat-1", "msg-1", 77)),
            fake.markedRead,
        )
        assertTrue(fake.replies.isEmpty())
    }

    @Test
    fun receiver_replyDispatchesTrimmedRemoteInput() {
        val fake = FakeMessageNotificationActionDispatcher()
        MessageNotificationActionDispatch.dispatcher = fake
        val intent = Intent(MessageNotificationActionReceiver.ACTION_MESSAGE_NOTIFICATION).apply {
            putExtra(ActionableMessageNotifier.EXTRA_NOTIFICATION_ID, 78)
            putExtra(ActionableMessageNotifier.EXTRA_CHAT_ID, "chat-2")
            putExtra(ActionableMessageNotifier.EXTRA_MESSAGE_ID, "msg-2")
            putExtra(ActionableMessageNotifier.EXTRA_ACTION, ActionableMessageNotifier.ACTION_REPLY)
        }
        val remoteInput = RemoteInput.Builder(ActionableMessageNotifier.REMOTE_INPUT_KEY).build()
        RemoteInput.addResultsToIntent(
            arrayOf(remoteInput),
            intent,
            Bundle().apply {
                putCharSequence(ActionableMessageNotifier.REMOTE_INPUT_KEY, "  hello back  ")
            },
        )

        MessageNotificationActionReceiver().onReceive(context, intent)

        assertEquals(
            listOf(
                FakeMessageNotificationActionDispatcher.ReplyCall(
                    "chat-2",
                    "msg-2",
                    78,
                    "hello back",
                ),
            ),
            fake.replies,
        )
        assertTrue(fake.markedRead.isEmpty())
    }
}
