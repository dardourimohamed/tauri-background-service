package app.tauri.backgroundservice

import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import androidx.test.core.app.ApplicationProvider
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import java.io.File

/**
 * BGS-19 (doc-08 Step 16 Task A) — LifecycleService FGS notification localization.
 *
 * The two remaining hard-coded English surfaces in [LifecycleService] were the FGS
 * *restart* foreground notification body (`handleOsRestart` → `buildNotification`) and
 * the *timeout* notification body (`postTimeoutNotification`). ar/fr users got English
 * on both. Task A wraps them with a [NotificationStrings] lookup driven by the
 * Rust-persisted `locale.json` store (the same store T2's notifier labels read).
 *
 * These tests drive the LIVE notification seams — `onStartCommand(null)` (which
 * dispatches to `handleOsRestart`) and `handleTimeout` (the public entry that calls
 * `postTimeoutNotification` under the `notifyUser` policy) — and assert the localized
 * **body** (`Notification.EXTRA_TEXT` / contentText). The body is the load-bearing
 * observable: Android notification *channels* are immutable once created, so a channel
 * display name localizes only on first creation and is NOT a reliable assertion target
 * (see [Bgs19NotifierLocalizationTest] for the channel-localization caveat). The
 * channel-name/description table values are locked separately (pure-JVM) in
 * [NotificationStringsTest].
 *
 * NV-MUT: revert a call site to its English literal while `locale=ar`/`fr` ⇒ the
 * localized test REDs (expects the localized body, gets English). The two call sites
 * (restart body, timeout body) are pinned by disjoint tests.
 */
@RunWith(RobolectricTestRunner::class)
class Bgs19LifecycleStringsTest {
    private lateinit var context: Context
    private lateinit var prefs: android.content.SharedPreferences
    private lateinit var realDataDir: String
    private var savedDataDir: String? = null

    @Before
    fun setup() {
        context = ApplicationProvider.getApplicationContext()
        prefs = context.getSharedPreferences("bg_service", Context.MODE_PRIVATE)
        // Mirror LifecycleServiceTest: inject a happy-path bridge and run the
        // core-start/stop tasks inline so post-onStartCommand state is deterministic.
        LifecycleService.bridgeProvider = { FakeCoreBridge(result = "running") }
        LifecycleService.coreStartExecutor = { _, task -> task() }
        LifecycleService.coreStopExecutor = { _, task -> task() }
        // Point applicationInfo.dataDir at a temp dir so locale.json can be staged
        // deterministically (Rust writes {app_data}/data/locale.json; the Kotlin
        // LocaleStore reads applicationInfo.dataDir/data — same path). Save/restore.
        savedDataDir = context.applicationInfo.dataDir
        realDataDir = context.applicationInfo.dataDir
        context.applicationInfo.dataDir = createTempDir(prefix = "bgs19-life-").absolutePath
    }

    @After
    fun tearDown() {
        context.applicationInfo.dataDir = savedDataDir ?: realDataDir
        LifecycleService.bridgeProvider = { HeadlessCoreBridgeImpl() }
        LifecycleService.coreStartExecutor = LifecycleService.DEFAULT_CORE_START_EXECUTOR
        LifecycleService.coreStopExecutor = LifecycleService.DEFAULT_CORE_STOP_EXECUTOR
        LifecycleService.isRunning = false
        LifecycleService.isForeground = false
        LifecycleService.autoRestarting = false
        BackgroundServicePlugin.onTimeoutEvent = null
        BackgroundServicePlugin.onNativeLifecycleEvent = null
        BackgroundServicePlugin.onPlatformErrorEvent = null
    }

    // ── Restart foreground notification body (handleOsRestart) ──────────

    @Test
    @Config(sdk = [34])
    fun restartBody_localized_underArabic() {
        writeLocale("ar")
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()

        val service = Robolectric.buildService(LifecycleService::class.java).create().get()
        // Null intent ⇒ handleOsRestart ⇒ startForeground with the "restarting" body.
        service.onStartCommand(null, 0, 0)

        val body = foregroundBody()
        // Arabic "Restarting..." ⇒ "جارٍ إعادة التشغيل...".
        assertEquals("جارٍ إعادة التشغيل...", body)
    }

    @Test
    @Config(sdk = [34])
    fun restartBody_localized_underFrench() {
        writeLocale("fr")
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()

        val service = Robolectric.buildService(LifecycleService::class.java).create().get()
        service.onStartCommand(null, 0, 0)

        assertEquals("Redémarrage...", foregroundBody())
    }

    @Test
    @Config(sdk = [34])
    fun restartBody_englishWhenNoLocaleStore() {
        // No locale.json staged ⇒ default English (fallback must not regress).
        prefs.edit()
            .putString("bg_service_label", "Syncing")
            .putString("bg_service_type", "dataSync")
            .apply()

        val service = Robolectric.buildService(LifecycleService::class.java).create().get()
        service.onStartCommand(null, 0, 0)

        assertEquals("Restarting...", foregroundBody())
    }

    // ── Timeout notification body (postTimeoutNotification) ─────────────

    @Test
    @Config(sdk = [34])
    fun timeoutBody_localized_underArabic() {
        writeLocale("ar")
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_on_timeout_policy", "notifyUser")
            .apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent).create().get()
        service.onStartCommand(intent, 0, 0)
        service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)

        // Arabic "Background service timed out: {label}" with label="Syncing".
        assertEquals("انتهت مهلة خدمة الخلفية: Syncing", timeoutBody())
    }

    @Test
    @Config(sdk = [34])
    fun timeoutBody_localized_underFrench() {
        writeLocale("fr")
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_on_timeout_policy", "notifyUser")
            .apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent).create().get()
        service.onStartCommand(intent, 0, 0)
        service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)

        assertEquals("Le service en arrière-plan a expiré : Syncing", timeoutBody())
    }

    @Test
    @Config(sdk = [34])
    fun timeoutBody_englishWhenNoLocaleStore() {
        // No locale.json staged ⇒ default English (fallback must not regress).
        prefs.edit().clear().apply()
        DurableState.clear(context)
        prefs.edit()
            .putString("bg_on_timeout_policy", "notifyUser")
            .apply()

        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, "Syncing")
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "dataSync")
        }
        val service = Robolectric.buildService(LifecycleService::class.java)
            .withIntent(intent).create().get()
        service.onStartCommand(intent, 0, 0)
        service.handleTimeout(ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)

        assertEquals("Background service timed out: Syncing", timeoutBody())
    }

    // ── helpers ─────────────────────────────────────────────────────────

    /** The body (contentText / EXTRA_TEXT) of the FGS foreground notification. */
    private fun foregroundBody(): String {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val sbn = nm.activeNotifications.find { it.id == LifecycleService.NOTIF_ID }
        assertNotNull("FGS restart notification should be posted at NOTIF_ID", sbn)
        return sbn!!.notification.extras.getCharSequence(Notification.EXTRA_TEXT).toString()
    }

    /** The body (contentText / EXTRA_TEXT) of the timeout notification. */
    private fun timeoutBody(): String {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val sbn = shadowOf(nm).getNotification(LifecycleService.TIMEOUT_NOTIFICATION_ID)
        assertNotNull("timeout notification should be posted at TIMEOUT_NOTIFICATION_ID", sbn)
        return sbn!!.extras.getCharSequence(Notification.EXTRA_TEXT).toString()
    }

    /** Stage a `locale.json` (`{"locale":<code>}`) at `{dataDir}/data/locale.json`. */
    private fun writeLocale(code: String) {
        val dir = File(context.applicationInfo.dataDir, "data")
        dir.mkdirs()
        File(dir, "locale.json").writeText("{\"locale\": \"$code\"}")
    }
}
