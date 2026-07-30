package app.tauri.backgroundservice

import android.content.Context
import android.content.ContextWrapper
import android.content.Intent
import androidx.test.core.app.ApplicationProvider
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import java.io.File

/**
 * AND-08: focused edge-fix tests.
 *
 * - `MessageNotificationActionReceiver` detects a missing notification id via
 *   `hasExtra` (not the `Int.MIN_VALUE` sentinel), so a legitimate id equal to
 *   `Int.MIN_VALUE` is no longer silently dropped.
 * - `ConnectivityMonitor` drops the retained dead-thread `Handler` when
 *   `registerNetworkCallback` fails, so a later trailing-edge fire cannot post
 *   onto a looper that never runs.
 * - `HeadlessBridge.dataDir` falls back to `filesDir` when
 *   `applicationInfo.dataDir` is null.
 */
@RunWith(RobolectricTestRunner::class)
class And08EdgeFixesTest {

    private lateinit var context: Context

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
        // Run the receiver dispatch inline so post-onReceive state is deterministic.
        MessageNotificationActionReceiver.actionExecutor = { _, task -> task() }
    }

    @After
    fun tearDown() {
        MessageNotificationActionReceiver.actionExecutor =
            MessageNotificationActionReceiver.DEFAULT_ACTION_EXECUTOR
        MessageNotificationActionDispatch.dispatcher = RealMessageNotificationActionDispatcher()
    }

    private fun baseIntent(notificationId: Int? = null): Intent =
        Intent().apply {
            putExtra(ActionableMessageNotifier.EXTRA_CHAT_ID, "chat-1")
            putExtra(ActionableMessageNotifier.EXTRA_MESSAGE_ID, "msg-1")
            putExtra(ActionableMessageNotifier.EXTRA_ACTION, ActionableMessageNotifier.ACTION_MARK_READ)
            if (notificationId != null) {
                putExtra(ActionableMessageNotifier.EXTRA_NOTIFICATION_ID, notificationId)
            }
        }

    @Test
    fun messageNotificationReceiver_missingIdExtra_returnsEarlyWithoutDispatch() {
        val fake = FakeMessageNotificationActionDispatcher()
        MessageNotificationActionDispatch.dispatcher = fake

        MessageNotificationActionReceiver().onReceive(context, baseIntent(notificationId = null))

        assertTrue(
            "a broadcast without EXTRA_NOTIFICATION_ID must be dropped before dispatch",
            fake.markedRead.isEmpty(),
        )
    }

    @Test
    fun messageNotificationReceiver_idEqualToIntMinValue_isDispatchedNotDropped() {
        // AND-08 regression: the old `getIntExtra(.., Int.MIN_VALUE)` sentinel
        // collided with a legitimate id of Int.MIN_VALUE and silently dropped it.
        val fake = FakeMessageNotificationActionDispatcher()
        MessageNotificationActionDispatch.dispatcher = fake

        MessageNotificationActionReceiver().onReceive(context, baseIntent(notificationId = Int.MIN_VALUE))

        assertEquals(
            "a legitimate id of Int.MIN_VALUE must reach the dispatcher (no sentinel collision)",
            listOf(Int.MIN_VALUE),
            fake.markedRead.map { it.notificationId },
        )
    }

    @Test
    fun connectivityMonitor_failedRegistration_dropsRetainedDeadHandler() {
        // Force the failure path: a context whose ConnectivityManager resolves to
        // null makes registerNetworkCallback throw NPE inside register().
        val failingContext = object : ContextWrapper(context) {
            override fun getSystemService(name: String): Any? =
                if (name == Context.CONNECTIVITY_SERVICE) null
                else super.getSystemService(name)
        }
        val monitor = ConnectivityMonitor(failingContext, onNetworkChanged = {})

        monitor.register()

        assertFalse(
            "AND-08: a failed registration must drop the retained dead-thread Handler",
            monitor.backgroundHandlerRetained(),
        )
        // And no live looper was registered.
        assertFalse(monitor.backgroundLooper() != null)
    }

    @Test
    fun headlessBridge_dataDir_fallsBackToFilesDirWhenDataDirNull() {
        val info = context.applicationInfo
        val saved = info.dataDir
        try {
            info.dataDir = null
            val resolved = HeadlessBridge.dataDir(context)
            assertEquals(
                "AND-08: when applicationInfo.dataDir is null, dataDir must fall back to filesDir",
                File(context.filesDir.absolutePath, "data").absolutePath,
                resolved.absolutePath,
            )
        } finally {
            info.dataDir = saved
        }
    }

    @Test
    fun headlessBridge_dataDir_usesApplicationDataDirWhenPresent() {
        val info = context.applicationInfo
        val saved = info.dataDir
        try {
            info.dataDir = "/tmp/bgs-test-data"
            val resolved = HeadlessBridge.dataDir(context)
            assertEquals(
                "when applicationInfo.dataDir is present it is honored",
                File("/tmp/bgs-test-data", "data").absolutePath,
                resolved.absolutePath,
            )
        } finally {
            info.dataDir = saved
        }
    }
}
