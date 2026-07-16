package app.tauri.backgroundservice

import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.app.Notification
import androidx.test.core.app.ApplicationProvider
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import java.io.File

/**
 * BGS-19 (doc-08 Step 16 Task 2) — Kotlin notifier localization.
 *
 * The Kotlin-owned notification labels (message reply / mark-read action titles,
 * boot-recovery body) were hard-coded English literals; ar/fr users got English on
 * the window-closed surface. T2 wraps them with a lookup driven by the Rust-persisted
 * `locale.json` store (the same store T1's `locale_store::locale_lookup` reads).
 *
 * These tests drive the LIVE notification seams (`showMessageNotification`,
 * `postRecoveryNotification`) and assert the localized output. NV-MUT: revert a call
 * site to its English literal while `locale=ar` ⇒ the ar test REDs (expects the
 * localized string, gets English).
 */
@RunWith(RobolectricTestRunner::class)
class Bgs19NotifierLocalizationTest {
    private lateinit var context: Context
    private lateinit var realDataDir: String
    private var savedDataDir: String? = null

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
        // Rust writes `{app_data}/data/locale.json` (AppData/"data" == applicationInfo.dataDir/"data"
        // per HeadlessBridge.dataDir). Point applicationInfo.dataDir at a temp dir so the test
        // can stage locale.json deterministically. Save/restore the real value.
        savedDataDir = context.applicationInfo.dataDir
        realDataDir = context.applicationInfo.dataDir
        context.applicationInfo.dataDir = createTempDir(prefix = "bgs19-locale-").absolutePath
    }

    @After
    fun tearDown() {
        context.applicationInfo.dataDir = savedDataDir ?: realDataDir
    }

    @Test
    @Config(sdk = [34])
    fun replyAction_localized_underArabic() {
        writeLocale("ar")
        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = 55001,
            chatId = "chat-ar",
            messageId = "msg-ar",
            title = "Alice",
            body = "مرحبة",
            routeUri = "bg-service://chat?chat_id=chat-ar&message_id=msg-ar",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = Intent(Intent.ACTION_MAIN).setPackage(context.packageName),
        )

        val notification = shadowOf(
            context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager,
        ).getNotification(ActionableMessageNotifier.chatTagFor("chat-ar"), 55001)
        assertNotNull(notification)
        val replyAction = notification!!.actions!![0]
        // Arabic "Reply" ⇒ "رد" (matches NotificationStrings ar table).
        assertEquals("رد", replyAction.title.toString())
        // The RemoteInput label is localized too.
        assertEquals("رد", replyAction.remoteInputs!![0].label.toString())
        // "Mark as read" action title also localized.
        assertEquals("ضع علامة كمقروء", notification.actions!![1].title.toString())
    }

    @Test
    @Config(sdk = [34])
    fun replyAction_localized_underFrench() {
        writeLocale("fr")
        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = 55002,
            chatId = "chat-fr",
            messageId = "msg-fr",
            title = "Alice",
            body = "bonjour",
            routeUri = "bg-service://chat?chat_id=chat-fr&message_id=msg-fr",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = Intent(Intent.ACTION_MAIN).setPackage(context.packageName),
        )

        val notification = shadowOf(
            context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager,
        ).getNotification(ActionableMessageNotifier.chatTagFor("chat-fr"), 55002)
        assertNotNull(notification)
        assertEquals("Répondre", notification!!.actions!![0].title.toString())
        assertEquals("Marquer comme lu", notification.actions!![1].title.toString())
    }

    @Test
    @Config(sdk = [34])
    fun replyAction_englishWhenNoLocaleStore() {
        // No locale.json staged ⇒ default English (fallback must not regress).
        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = 55003,
            chatId = "chat-en",
            messageId = "msg-en",
            title = "Alice",
            body = "hello",
            routeUri = "bg-service://chat?chat_id=chat-en&message_id=msg-en",
            smallIcon = android.R.drawable.sym_def_app_icon,
            launchIntent = Intent(Intent.ACTION_MAIN).setPackage(context.packageName),
        )

        val notification = shadowOf(
            context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager,
        ).getNotification(ActionableMessageNotifier.chatTagFor("chat-en"), 55003)
        assertNotNull(notification)
        assertEquals("Reply", notification!!.actions!![0].title.toString())
        assertEquals("Mark as read", notification.actions!![1].title.toString())
    }

    @Test
    @Config(sdk = [34])
    fun recoveryBody_localized_underArabic() {
        writeLocale("ar")
        BootReceiver.postRecoveryNotification(context, "App")

        val notification = shadowOf(
            context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager,
        ).getNotification(BootReceiver.RECOVERY_NOTIFICATION_ID)
        assertNotNull(notification)
        val text = notification!!.extras.getCharSequence(Notification.EXTRA_TEXT).toString()
        // Arabic "Tap to resume: {label}" ⇒ "اضغط للاستئناف: App".
        assertEquals("اضغط للاستئناف: App", text)
    }

    @Test
    @Config(sdk = [34])
    fun recoveryBody_englishWhenNoLocaleStore() {
        BootReceiver.postRecoveryNotification(context, "App")
        val notification = shadowOf(
            context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager,
        ).getNotification(BootReceiver.RECOVERY_NOTIFICATION_ID)
        assertNotNull(notification)
        val text = notification!!.extras.getCharSequence(Notification.EXTRA_TEXT).toString()
        assertEquals("Tap to resume: App", text)
    }

    /** Stage a `locale.json` (`{"locale":<code>}`) at `{dataDir}/data/locale.json`. */
    private fun writeLocale(code: String) {
        val dir = File(context.applicationInfo.dataDir, "data")
        dir.mkdirs()
        File(dir, "locale.json").writeText("{\"locale\": \"$code\"}")
    }
}
