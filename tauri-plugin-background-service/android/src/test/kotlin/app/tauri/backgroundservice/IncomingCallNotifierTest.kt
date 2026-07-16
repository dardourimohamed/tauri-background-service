package app.tauri.backgroundservice

import android.app.NotificationManager
import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/**
 * JVM tests for [IncomingCallNotifier] (spec 08 C6, Step 15):
 * - `canUseFullScreenIntent` API-level branches (the F4 gate).
 * - showIncomingCall posts to the dedicated call channel.
 * - F4 fallback: when the full-screen-intent grant is missing, the posted
 *   notification carries no full-screen intent (it still rings via the channel).
 */
@RunWith(RobolectricTestRunner::class)
class IncomingCallNotifierTest {

    private lateinit var context: Context

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
    }

    @Test
    @Config(sdk = [24])
    fun canUseFullScreenIntent_belowQ_false() {
        // Pre-API-29 the concept does not exist.
        assertFalse(IncomingCallNotifier.canUseFullScreenIntent(context))
    }

    @Test
    @Config(sdk = [33])
    fun canUseFullScreenIntent_api29to33_true() {
        // API 29–33: auto-granted when the permission is declared.
        assertTrue(IncomingCallNotifier.canUseFullScreenIntent(context))
    }

    @Test
    fun notificationIdFor_isStableAndNonNegative() {
        val id1 = IncomingCallNotifier.notificationIdFor("call-abc")
        val id2 = IncomingCallNotifier.notificationIdFor("call-abc")
        val idOther = IncomingCallNotifier.notificationIdFor("call-xyz")
        assertEquals("Same call id → same notification id", id1, id2)
        assertNotEquals("Different call id → different notification id", id1, idOther)
        assertTrue("Notification ids must be positive", id1 > 0)
        assertTrue(
            "Notification id must sit above the bg-service ids (no clash)",
            id1 >= IncomingCallNotifier.CALL_NOTIFICATION_BASE,
        )
    }

    @Test
    @Config(sdk = [34])
    fun showIncomingCall_postsToCallChannel() {
        IncomingCallNotifier.showIncomingCall(
            context = context,
            callId = "call-1",
            callerName = "Alice",
            isVideo = false,
            smallIcon = android.R.drawable.stat_notify_sync,
            launchIntent = null,
            useFullScreenIntent = true,
        )

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val shadowNm = shadowOf(nm)
        val notif = shadowNm.getNotification(IncomingCallNotifier.notificationIdFor("call-1"))
        assertNotNull("Incoming call notification should be posted", notif)
        assertEquals(NotificationHelper.CALL_CHANNEL_ID, notif!!.channelId)
        assertNotNull("FSI granted → full-screen intent attached", notif.fullScreenIntent)

        // The dedicated channel must have been created.
        val channel = nm.getNotificationChannel(NotificationHelper.CALL_CHANNEL_ID)
        assertNotNull("Call channel created on show", channel)
    }

    @Test
    @Config(sdk = [34])
    fun showIncomingCall_f4Fallback_hasNoFullScreenIntent() {
        // F4: full-screen-intent grant revoked → ring via channel, no FSI.
        IncomingCallNotifier.showIncomingCall(
            context = context,
            callId = "call-2",
            callerName = "Bob",
            isVideo = true,
            smallIcon = android.R.drawable.stat_notify_sync,
            launchIntent = null,
            useFullScreenIntent = false,
        )

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val notif = shadowOf(nm).getNotification(IncomingCallNotifier.notificationIdFor("call-2"))
        assertNotNull(notif)
        assertNull("F4 fallback: no full-screen intent", notif!!.fullScreenIntent)
        assertEquals(
            "Fallback still uses the ringing call channel",
            NotificationHelper.CALL_CHANNEL_ID,
            notif.channelId,
        )
    }

    // ── M-NATIVE-1 (Step 9): Answer/Decline actions are broadcasts to the core ──

    @Test
    @Config(sdk = [34])
    fun callAction_answer_isBroadcastToCallActionReceiver() {
        // The masked seam: the Answer action must be a getBroadcast PendingIntent
        // targeting CallActionReceiver (runs locked / webview-absent), NOT a
        // getActivity launcher that merely opens the app. RED on the prior
        // getActivity form; GREEN once switched to getBroadcast.
        val pi = IncomingCallNotifier.callActionPendingIntent(
            context, "call-answer-1", IncomingCallNotifier.ACTION_ANSWER, null,
        )
        val shadow = shadowOf(pi)
        assertTrue(
            "Answer action must be a broadcast (reaches the core headlessly), not an activity launcher",
            shadow.isBroadcastIntent,
        )
        val saved = shadow.savedIntent
        assertEquals(
            CallActionReceiver::class.java.name,
            saved.component?.className,
        )
        assertEquals("call-answer-1", saved.getStringExtra(IncomingCallNotifier.EXTRA_CALL_ID))
        assertEquals(
            IncomingCallNotifier.ACTION_ANSWER,
            saved.getStringExtra(IncomingCallNotifier.EXTRA_CALL_ACTION),
        )
    }

    @Test
    @Config(sdk = [34])
    fun callAction_decline_isBroadcastToCallActionReceiver() {
        val pi = IncomingCallNotifier.callActionPendingIntent(
            context, "call-decline-1", IncomingCallNotifier.ACTION_DECLINE, null,
        )
        val shadow = shadowOf(pi)
        assertTrue(
            "Decline action must be a broadcast, not an activity launcher",
            shadow.isBroadcastIntent,
        )
        val saved = shadow.savedIntent
        assertEquals(CallActionReceiver::class.java.name, saved.component?.className)
        assertEquals("call-decline-1", saved.getStringExtra(IncomingCallNotifier.EXTRA_CALL_ID))
        assertEquals(
            IncomingCallNotifier.ACTION_DECLINE,
            saved.getStringExtra(IncomingCallNotifier.EXTRA_CALL_ACTION),
        )
    }

    // ── BGS-07 (Step 10): headless full forwarder — message notif + ring cancel + timeout ──
    //
    // The prior test asserted the headless upcall as *ring-only* (a ring posts a
    // ring). That masked the BGS-07 gap: the webview-absent forwarder must ALSO
    // post a native message notification and CANCEL a stale/ended CallStyle ring
    // (an abandoned incoming call must not leave the full-screen notification up
    // forever). Rewritten to drive all three headless upcall targets.

    @Test
    @Config(sdk = [34])
    fun bgs07_headless_message_notif_and_ring_cancel() {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

        // (1) MESSAGE: the webview-absent forwarder posts a native message
        //     notification — a message arriving headlessly must not be silently
        //     stored with no notification (BGS-07). The Rust→Kotlin `showMessage`
        //     upcall resolves the process Application context and posts via
        //     ActionableMessageNotifier to the dedicated message channel.
        HeadlessCoreBridge.showMessage("chat-7", "msg-7", "Alice")
        val msgId = ActionableMessageNotifier.notificationIdFor("chat-7")
        val msgNotif = shadowOf(nm).getNotification(msgId)
        assertNotNull("Headless message upcall must post a message notification", msgNotif)
        assertEquals(
            "Headless message posts to the dedicated message channel",
            ActionableMessageNotifier.MESSAGE_CHANNEL_ID,
            msgNotif!!.channelId,
        )

        // (2) RING CANCEL on CallEnded: the forwarder cancels the CallStyle ring
        //     for an ended/abandoned call (dropped entirely by the ring-only
        //     forwarder — the exact gap BGS-07 closes).
        HeadlessCoreBridge.showIncomingCall("call-7", "Bob", false)
        val ringId = IncomingCallNotifier.notificationIdFor("call-7")
        assertNotNull("Ring posted before cancel", shadowOf(nm).getNotification(ringId))

        HeadlessCoreBridge.cancelIncomingCall("call-7")
        assertNull(
            "CallEnded must cancel the headless CallStyle ring",
            shadowOf(nm).getNotification(ringId),
        )
    }

    @Test
    @Config(sdk = [34])
    fun bgs07_ring_timeout_auto_cancels() {
        // Deterministic ring timeout (BGS-07): an abandoned incoming call whose
        // caller gives up before the forwarder sees a CallEnded must not ring
        // forever. Inject a manual scheduler that captures the scheduled action
        // instead of a wall-clock Handler, so the timeout fires deterministically
        // with NO wall-clock sleep.
        var captured: (() -> Unit)? = null
        var capturedDelay = -1L
        val original = IncomingCallNotifier.timeoutScheduler
        IncomingCallNotifier.timeoutScheduler =
            IncomingCallNotifier.TimeoutScheduler { delayMs, action ->
                capturedDelay = delayMs
                captured = action
                ({ captured = null })
            }
        try {
            IncomingCallNotifier.showIncomingCall(
                context = context,
                callId = "call-timeout",
                callerName = "Carol",
                isVideo = false,
                smallIcon = android.R.drawable.stat_notify_sync,
                launchIntent = null,
                useFullScreenIntent = true,
            )
            val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            val id = IncomingCallNotifier.notificationIdFor("call-timeout")
            assertNotNull("Ring posted", shadowOf(nm).getNotification(id))
            assertNotNull("showIncomingCall must arm a ring timeout", captured)
            assertEquals(
                "Ring timeout must be armed for the product ring duration",
                IncomingCallNotifier.RING_TIMEOUT_MS,
                capturedDelay,
            )

            // Fire the timeout deterministically (no wall-clock sleep).
            captured!!.invoke()
            assertNull(
                "Ring timeout must auto-cancel the abandoned CallStyle notification",
                shadowOf(nm).getNotification(id),
            )
        } finally {
            IncomingCallNotifier.timeoutScheduler = original
        }
    }

    @Test
    @Config(sdk = [34])
    fun cancel_removesPostedNotification() {
        IncomingCallNotifier.showIncomingCall(
            context = context,
            callId = "call-3",
            callerName = "Carol",
            isVideo = false,
            smallIcon = android.R.drawable.stat_notify_sync,
            launchIntent = null,
            useFullScreenIntent = true,
        )
        val id = IncomingCallNotifier.notificationIdFor("call-3")
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        assertNotNull(shadowOf(nm).getNotification(id))

        IncomingCallNotifier.cancel(context, "call-3")
        assertNull("Cancel removes the notification", shadowOf(nm).getNotification(id))
    }
}
