package app.tauri.backgroundservice

import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
class NotificationHelperTest {

    private lateinit var context: Context

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
    }

    @Test
    fun notificationIconResolver_prefersStatIconOverSyncFallback() {
        // NTF-15: with the ic_stat_sila drawable shipped, the no-arg resolve()
        // reaches L17 (resolveNamed("ic_stat_sila")) and returns it instead of
        // falling through to the launcher / sync fallback.
        val expected = context.resources.getIdentifier("ic_stat_sila", "drawable", context.packageName)
        assertTrue("ic_stat_sila drawable should exist", expected != 0)

        val resolved = NotificationIconResolver.resolve(context)

        assertEquals(expected, resolved)
        assertTrue(resolved != android.R.drawable.stat_notify_sync)
    }

    @Test
    fun notificationIconResolver_configuredPathWinsOverStatFallback() {
        // NTF-15: the CONFIGURED path (BackgroundServicePlugin.resolveSmallIcon +
        // LifecycleService.notifSmallIcon) consults resolveNamed(configuredName)
        // at L16 BEFORE the L17 "ic_stat_sila" fallback. This is why the config
        // flip (ic_launcher -> ic_stat_sila) is load-bearing: with ic_stat_sila
        // present at L17, configuring "ic_launcher" must STILL resolve to
        // ic_launcher — proving L16 is consulted first and would otherwise keep
        // returning the launcher for the plugin's main notification surfaces.
        val launcher = context.resources.getIdentifier("ic_launcher", "drawable", context.packageName)
        assertTrue("test launcher icon fixture should exist", launcher != 0)
        val statIcon = context.resources.getIdentifier("ic_stat_sila", "drawable", context.packageName)
        assertTrue("ic_stat_sila drawable should exist", statIcon != 0)

        // L16 production path post-flip (BSP + LifecycleService configure ic_stat_sila):
        assertEquals(statIcon, NotificationIconResolver.resolve(context, "ic_stat_sila"))
        // L16 wins over L17: ic_stat_sila IS present at L17, yet configuring
        // "ic_launcher" resolves to ic_launcher, not ic_stat_sila.
        assertEquals(launcher, NotificationIconResolver.resolve(context, "ic_launcher"))
    }

    // ── ensureChannel: API-level guards ───────────────────────────────

    @Test
    @Config(sdk = [24])
    fun ensureChannel_api24_skipsWithoutCrash() {
        NotificationHelper.ensureChannel(
            context, "test_channel", "Test", NotificationManager.IMPORTANCE_LOW
        )
    }

    @Test
    @Config(sdk = [25])
    fun ensureChannel_api25_skipsWithoutCrash() {
        NotificationHelper.ensureChannel(
            context, "test_channel", "Test", NotificationManager.IMPORTANCE_LOW
        )
    }

    @Test
    @Config(sdk = [26])
    fun ensureChannel_api26_createsChannel() {
        NotificationHelper.ensureChannel(
            context, "test_channel", "Test Channel", NotificationManager.IMPORTANCE_LOW
        )
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = nm.getNotificationChannel("test_channel")
        assertNotNull("Channel should be created on API 26", channel)
        assertEquals("test_channel", channel!!.id)
        assertEquals(NotificationManager.IMPORTANCE_LOW, channel.importance)
    }

    @Test
    @Config(sdk = [26])
    fun ensureChannel_api26_withDescriptionAndBadge() {
        NotificationHelper.ensureChannel(
            context, "desc_channel", "Desc Channel", NotificationManager.IMPORTANCE_HIGH,
            description = "A described channel",
            showBadge = true,
        )
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = nm.getNotificationChannel("desc_channel")
        assertNotNull(channel)
        assertEquals("A described channel", channel.description)
        assertEquals(NotificationManager.IMPORTANCE_HIGH, channel.importance)
    }

    // ── buildForegroundNotification ───────────────────────────────────

    @Test
    @Config(sdk = [33])
    fun buildForegroundNotification_hasCorrectContent() {
        val notification = NotificationHelper.buildForegroundNotification(
            context = context,
            channelId = "bg_keepalive",
            title = "Sila",
            text = "Service running",
            smallIcon = android.R.drawable.stat_notify_sync,
            pendingIntent = null,
            showStopAction = false,
            stopActionIntent = null,
        )

        val extras = notification.extras
        assertEquals("Sila", extras.getString(Notification.EXTRA_TITLE))
        assertEquals("Service running", extras.getString(Notification.EXTRA_TEXT))
    }

    @Test
    @Config(sdk = [33])
    fun buildForegroundNotification_withStopAction_hasAction() {
        val stopIntent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_STOP
        }
        val notification = NotificationHelper.buildForegroundNotification(
            context = context,
            channelId = "bg_keepalive",
            title = "Sila",
            text = "Running",
            smallIcon = android.R.drawable.stat_notify_sync,
            pendingIntent = null,
            showStopAction = true,
            stopActionIntent = stopIntent,
        )

        assertTrue("Should have stop action", notification.actions.isNotEmpty())
    }

    @Test
    @Config(sdk = [33])
    fun buildForegroundNotification_withoutStopAction_hasNoActions() {
        val notification = NotificationHelper.buildForegroundNotification(
            context = context,
            channelId = "bg_keepalive",
            title = "Sila",
            text = "Running",
            smallIcon = android.R.drawable.stat_notify_sync,
            pendingIntent = null,
            showStopAction = false,
            stopActionIntent = null,
        )

        assertTrue("Should have no actions", notification.actions.isNullOrEmpty())
    }

    // ── buildTimeoutNotification ──────────────────────────────────────

    @Test
    @Config(sdk = [33])
    fun buildTimeoutNotification_hasCorrectContent() {
        val notification = NotificationHelper.buildTimeoutNotification(
            context = context,
            channelId = "bg_service_timeout",
            title = "Sila",
            text = "Background service timed out: Syncing",
            smallIcon = android.R.drawable.stat_notify_sync,
            pendingIntent = null,
        )

        val extras = notification.extras
        assertEquals("Sila", extras.getString(Notification.EXTRA_TITLE))
        assertEquals("Background service timed out: Syncing", extras.getString(Notification.EXTRA_TEXT))
    }

    // ── buildRecoveryNotification ─────────────────────────────────────

    @Test
    @Config(sdk = [33])
    fun buildRecoveryNotification_hasCorrectContent() {
        val notification = NotificationHelper.buildRecoveryNotification(
            context = context,
            channelId = "bg_service_recovery",
            title = "Sila",
            text = "Tap to resume: Syncing",
            smallIcon = android.R.drawable.stat_notify_sync,
            pendingIntent = null,
        )

        val extras = notification.extras
        assertEquals("Sila", extras.getString(Notification.EXTRA_TITLE))
        assertEquals("Tap to resume: Syncing", extras.getString(Notification.EXTRA_TEXT))
    }

    // ── ensureCallChannel (spec 08 C6, Step 15) ───────────────────────

    @Test
    @Config(sdk = [24])
    fun ensureCallChannel_api24_skipsWithoutCrash() {
        NotificationHelper.ensureCallChannel(context)
    }

    @Test
    @Config(sdk = [34])
    fun ensureCallChannel_createsRingtoneChannel() {
        NotificationHelper.ensureCallChannel(context)
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = nm.getNotificationChannel(NotificationHelper.CALL_CHANNEL_ID)
        assertNotNull("Call channel should be created", channel)
        assertEquals(NotificationHelper.CALL_CHANNEL_ID, channel!!.id)
        assertEquals(NotificationManager.IMPORTANCE_HIGH, channel.importance)
        assertEquals(
            "Call channel must use the RINGTONE audio usage",
            android.media.AudioAttributes.USAGE_NOTIFICATION_RINGTONE,
            channel.audioAttributes.usage,
        )
        assertTrue("Call channel must vibrate", channel.shouldVibrate())
        assertEquals(Notification.VISIBILITY_PUBLIC, channel.lockscreenVisibility)
    }

    // ── buildIncomingCallNotification (spec 08 C6, Step 15) ───────────

    @Test
    @Config(sdk = [33])
    fun buildIncomingCallNotification_fallback_hasCallCategoryAndActions() {
        // API 33: pre-CallStyle. Heads-up fallback with Answer/Decline actions.
        val notification = NotificationHelper.buildIncomingCallNotification(
            context = context,
            channelId = NotificationHelper.CALL_CHANNEL_ID,
            callerName = "Alice",
            isVideo = true,
            smallIcon = android.R.drawable.stat_notify_sync,
            fullScreenIntent = null,
            answerIntent = android.app.PendingIntent.getActivity(
                context, 0, Intent(context, LifecycleService::class.java),
                android.app.PendingIntent.FLAG_IMMUTABLE or android.app.PendingIntent.FLAG_UPDATE_CURRENT,
            ),
            declineIntent = android.app.PendingIntent.getActivity(
                context, 1, Intent(context, LifecycleService::class.java),
                android.app.PendingIntent.FLAG_IMMUTABLE or android.app.PendingIntent.FLAG_UPDATE_CURRENT,
            ),
            useFullScreenIntent = false,
        )

        assertEquals(Notification.CATEGORY_CALL, notification.category)
        assertEquals(NotificationHelper.CALL_CHANNEL_ID, notification.channelId)
        assertEquals(2, notification.actions?.size ?: 0)
    }

    @Test
    @Config(sdk = [34])
    fun buildIncomingCallNotification_api34_usesCallStyleWithoutCrash() {
        // API 34+: Notification.CallStyle. Assert it builds + carries the channel.
        // CallStyle requires non-null answer/decline intents.
        val pi = android.app.PendingIntent.getActivity(
            context, 0, Intent(context, LifecycleService::class.java),
            android.app.PendingIntent.FLAG_IMMUTABLE or android.app.PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification = NotificationHelper.buildIncomingCallNotification(
            context = context,
            channelId = NotificationHelper.CALL_CHANNEL_ID,
            callerName = "Bob",
            isVideo = false,
            smallIcon = android.R.drawable.stat_notify_sync,
            fullScreenIntent = null,
            answerIntent = pi,
            declineIntent = pi,
            useFullScreenIntent = false,
        )
        assertEquals(NotificationHelper.CALL_CHANNEL_ID, notification.channelId)
        // CallStyle stores the caller Person under EXTRA_CALL_PERSON.
        val caller = notification.extras.get(Notification.EXTRA_CALL_PERSON)
        assertNotNull("CallStyle should carry the caller Person", caller)
    }

    @Test
    @Config(sdk = [34])
    fun buildIncomingCallNotification_api34_attachesFullScreenIntentWhenGranted() {
        val fsi = android.app.PendingIntent.getActivity(
            context, 7, Intent(context, LifecycleService::class.java),
            android.app.PendingIntent.FLAG_IMMUTABLE or android.app.PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val withFsi = NotificationHelper.buildIncomingCallNotification(
            context = context,
            channelId = NotificationHelper.CALL_CHANNEL_ID,
            callerName = "Bob",
            isVideo = false,
            smallIcon = android.R.drawable.stat_notify_sync,
            fullScreenIntent = fsi,
            answerIntent = null,
            declineIntent = null,
            useFullScreenIntent = true,
        )
        assertNotNull("FSI granted → full-screen intent attached", withFsi.fullScreenIntent)

        val withoutFsi = NotificationHelper.buildIncomingCallNotification(
            context = context,
            channelId = NotificationHelper.CALL_CHANNEL_ID,
            callerName = "Bob",
            isVideo = false,
            smallIcon = android.R.drawable.stat_notify_sync,
            fullScreenIntent = fsi,
            answerIntent = null,
            declineIntent = null,
            useFullScreenIntent = false,
        )
        assertNull("FSI revoked → no full-screen intent (F4 fallback)", withoutFsi.fullScreenIntent)
    }
}
