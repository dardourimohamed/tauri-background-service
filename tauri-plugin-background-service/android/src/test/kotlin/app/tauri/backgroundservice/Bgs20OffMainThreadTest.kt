package app.tauri.backgroundservice

import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.os.Looper
import androidx.core.app.RemoteInput
import androidx.test.core.app.ApplicationProvider
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertNotNull
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * BGS-20 (doc-08 Step 11, Task 2): the BroadcastReceiver entry points must run
 * their JNI dispatch OFF the Android main looper.
 *
 * `BroadcastReceiver.onReceive` runs on the main thread; both receivers reach a
 * `HeadlessBridge` JNI export that does `block_on` (a fresh QUIC dial for
 * call actions; storage + network work for notification actions) — an ANR risk
 * exactly during headless use (lock-screen Answer/Decline/Reply/Mark-as-read).
 * The fix wraps each dispatch in `goAsync()` + an injected worker executor
 * (mirroring `LifecycleService.coreStopExecutor` from Task 1).
 *
 * Load-bearing fixture: the fakes capture `Thread.currentThread()` per dispatch
 * (`answerThread`/`rejectThread`, `replyThread`/`markReadThread`), and each test
 * installs a THREAD-DISTINGUISHING executor (a real worker, joined for
 * determinism) — NOT the inline `{ _, task -> task() }` the dispatch-tests use,
 * which runs on the test/main thread and would make the assertion vacuous.
 *
 * NV-MUT (two DISTINCT mutations): re-inline `goAsync`/executor in
 * `CallActionReceiver` only → only [bgs20_call_action_off_main_thread] REDs;
 * re-inline in `MessageNotificationActionReceiver` only → only
 * [bgs20_message_notification_reply_off_main_thread] REDs (the two receivers
 * are independent files, so each mutation discriminates cleanly).
 */
@RunWith(RobolectricTestRunner::class)
class Bgs20OffMainThreadTest {

    private lateinit var context: Context

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
    }

    @After
    fun tearDown() {
        CallActionReceiver.actionExecutor = CallActionReceiver.DEFAULT_ACTION_EXECUTOR
        MessageNotificationActionReceiver.actionExecutor =
            MessageNotificationActionReceiver.DEFAULT_ACTION_EXECUTOR
        CallActionDispatch.dispatcher = RealCallActionDispatcher()
        MessageNotificationActionDispatch.dispatcher = RealMessageNotificationActionDispatcher()
    }

    @Test
    @Config(sdk = [34])
    fun bgs20_call_action_off_main_thread() {
        val mainThread = Looper.getMainLooper().thread
        val fake = FakeCallActionDispatcher()
        CallActionDispatch.dispatcher = fake
        // Thread-distinguishing executor: run on a real worker + join so the
        // post-onReceive assertions are deterministic AND the worker differs
        // from main. (Inline `{ _, task -> task() }` runs on main → vacuous.)
        CallActionReceiver.actionExecutor = { _, task ->
            val worker = Thread({ task() }, "bg-call-action-test")
            worker.start()
            worker.join()
        }

        val answer = Intent(CallActionReceiver.ACTION_CALL_ACTION).apply {
            putExtra(IncomingCallNotifier.EXTRA_CALL_ID, "bgs20-ans")
            putExtra(IncomingCallNotifier.EXTRA_CALL_ACTION, IncomingCallNotifier.ACTION_ANSWER)
        }
        CallActionReceiver().onReceive(context, answer)

        val decline = Intent(CallActionReceiver.ACTION_CALL_ACTION).apply {
            putExtra(IncomingCallNotifier.EXTRA_CALL_ID, "bgs20-dec")
            putExtra(IncomingCallNotifier.EXTRA_CALL_ACTION, IncomingCallNotifier.ACTION_DECLINE)
        }
        CallActionReceiver().onReceive(context, decline)

        assertNotNull("ACTION_ANSWER must be dispatched", fake.answerThread)
        assertNotSame(
            "ACTION_ANSWER dispatch must run off the main thread (BGS-20)",
            mainThread,
            fake.answerThread,
        )
        assertEquals("answer routed for the right call", listOf("bgs20-ans"), fake.answered)

        assertNotNull("ACTION_DECLINE must be dispatched", fake.rejectThread)
        assertNotSame(
            "ACTION_DECLINE dispatch must run off the main thread (BGS-20)",
            mainThread,
            fake.rejectThread,
        )
        assertEquals("reject routed for the right call", listOf("bgs20-dec"), fake.rejected)
    }

    @Test
    @Config(sdk = [34])
    fun bgs20_message_notification_reply_off_main_thread() {
        val mainThread = Looper.getMainLooper().thread
        val fake = FakeMessageNotificationActionDispatcher()
        MessageNotificationActionDispatch.dispatcher = fake
        MessageNotificationActionReceiver.actionExecutor = { _, task ->
            val worker = Thread({ task() }, "bg-message-action-test")
            worker.start()
            worker.join()
        }

        val markRead = Intent(MessageNotificationActionReceiver.ACTION_MESSAGE_NOTIFICATION).apply {
            putExtra(ActionableMessageNotifier.EXTRA_NOTIFICATION_ID, 201)
            putExtra(ActionableMessageNotifier.EXTRA_CHAT_ID, "bgs20-chat-1")
            putExtra(ActionableMessageNotifier.EXTRA_MESSAGE_ID, "bgs20-msg-1")
            putExtra(ActionableMessageNotifier.EXTRA_ACTION, ActionableMessageNotifier.ACTION_MARK_READ)
        }
        MessageNotificationActionReceiver().onReceive(context, markRead)

        // ACTION_REPLY with a RemoteInput results bundle (mirrors
        // ActionableMessageNotifierTest's reply fixture — exercises the same
        // RemoteInput.getResultsFromIntent path that the reply branch reads).
        val reply = Intent(MessageNotificationActionReceiver.ACTION_MESSAGE_NOTIFICATION).apply {
            putExtra(ActionableMessageNotifier.EXTRA_NOTIFICATION_ID, 202)
            putExtra(ActionableMessageNotifier.EXTRA_CHAT_ID, "bgs20-chat-2")
            putExtra(ActionableMessageNotifier.EXTRA_MESSAGE_ID, "bgs20-msg-2")
            putExtra(ActionableMessageNotifier.EXTRA_ACTION, ActionableMessageNotifier.ACTION_REPLY)
        }
        val remoteInput = RemoteInput.Builder(ActionableMessageNotifier.REMOTE_INPUT_KEY).build()
        RemoteInput.addResultsToIntent(
            arrayOf(remoteInput),
            reply,
            Bundle().apply {
                putCharSequence(ActionableMessageNotifier.REMOTE_INPUT_KEY, "hello back")
            },
        )
        MessageNotificationActionReceiver().onReceive(context, reply)

        assertNotNull("ACTION_MARK_READ must be dispatched", fake.markReadThread)
        assertNotSame(
            "ACTION_MARK_READ dispatch must run off the main thread (BGS-20)",
            mainThread,
            fake.markReadThread,
        )
        assertNotNull("ACTION_REPLY must be dispatched", fake.replyThread)
        assertNotSame(
            "ACTION_REPLY dispatch must run off the main thread (BGS-20)",
            mainThread,
            fake.replyThread,
        )
    }
}
