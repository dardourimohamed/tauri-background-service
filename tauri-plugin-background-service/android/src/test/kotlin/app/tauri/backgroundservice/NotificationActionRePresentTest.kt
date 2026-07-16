package app.tauri.backgroundservice

import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * NTF-04 (Step 7b): Robolectric tests for [handleNotificationActionResult] — the
 * re-present-vs-cancel APPLY layer. The Core result is injected via
 * [decideNotificationOutcome] (NO JNI: HeadlessBridge.performNotificationAction
 * is System.loadLibrary(the native core) and cannot load under Robolectric).
 *
 * AC3 (load-bearing): the re-presented notification MUST carry the original
 * replyText in its body. Anti-loop: a synthetic permanent code CANCELs (a
 * sentinel notification is removed), it does NOT re-present.
 */
@RunWith(RobolectricTestRunner::class)
class NotificationActionRePresentTest {
    private lateinit var context: Context
    private val notificationId = 44010

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
    }

    private fun shadowNotification(id: Int): Notification? {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        // Tag-agnostic retrieval. Step 9b posts under (chatTag, id), and Robolectric's
        // ShadowNotificationManager.getNotification(int) delegates to getNotification(null, id)
        // — TAG-SENSITIVE — so it returns null for a tag-keyed post. Query activeNotifications
        // by id so a cancel assertion is meaningful (not vacuously null).
        return nm.activeNotifications.firstOrNull { it.id == id }?.notification
    }

    private fun postSentinel() {
        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = notificationId,
            chatId = "chat-sentinel",
            messageId = "msg-sentinel",
            title = "sentinel",
            body = "sentinel-body",
            routeUri = "bg-service://chat?chat_id=chat-sentinel&message_id=msg-sentinel",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = null,
        )
        assertNotNull("sentinel must post before a cancel assertion", shadowNotification(notificationId))
    }

    @Test
    @Config(sdk = [34])
    fun recoverableRustFailure_rePresentsNotificationPreservingReplyText() {
        val result = HeadlessBridgeResult(
            ok = false,
            state = "failed",
            message = "core not running",
            recoverable = true,
            rawJson = """{"ok":false,"state":"failed","recoverable":true,"message":"core not running"}""",
        )
        val outcome = decideNotificationOutcome(result, replyText = "typed reply")

        handleNotificationActionResult(context, outcome, "chat-7", "msg-7", notificationId)

        // RE-POSTED (cancel did NOT fire): the notification is present. If the
        // apply layer had cancelled instead, the notification would be absent.
        val posted = shadowNotification(notificationId)
        assertNotNull("recoverable failure must RE-PRESENT, not cancel", posted)
        // AC3 (load-bearing): the re-presented notification carries replyText.
        assertEquals(
            "typed reply",
            posted!!.extras.getCharSequence(Notification.EXTRA_TEXT)?.toString(),
        )
    }

    @Test
    @Config(sdk = [34])
    fun success_cancels() {
        postSentinel()
        val result = HeadlessBridgeResult(
            ok = true,
            state = "running",
            message = null,
            recoverable = false,
            rawJson = """{"ok":true,"state":"running"}""",
        )

        handleNotificationActionResult(context, decideNotificationOutcome(result, "x"), "chat-sentinel", "m", notificationId)

        assertNull("success must CANCEL (dismiss the notification)", shadowNotification(notificationId))
    }

    @Test
    @Config(sdk = [34])
    fun permanentSyntheticCode_cancels_antiLoop() {
        postSentinel()
        // failure() hardcodes recoverable=true for a PERMANENT env failure.
        val result = HeadlessBridgeResult.failure("native_library_load_failed", "load err")

        handleNotificationActionResult(context, decideNotificationOutcome(result, "x"), "chat-sentinel", "m", notificationId)

        assertNull("synthetic permanent failure must CANCEL (anti-loop), not re-present", shadowNotification(notificationId))
    }

    @Test
    @Config(sdk = [34])
    fun permanentRustNotRecoverable_cancels() {
        postSentinel()
        // Rust empty-reply verdict: recoverable == false (headless_core.rs:313).
        val result = HeadlessBridgeResult(
            ok = false,
            state = "failed",
            message = "empty notification reply",
            recoverable = false,
            rawJson = """{"ok":false,"state":"failed","recoverable":false,"message":"empty notification reply"}""",
        )

        handleNotificationActionResult(context, decideNotificationOutcome(result, ""), "chat-sentinel", "m", notificationId)

        assertNull("Rust-permanent (not recoverable) must CANCEL", shadowNotification(notificationId))
    }
}
