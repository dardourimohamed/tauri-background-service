package app.tauri.backgroundservice

import android.content.Context
import android.content.pm.PackageManager
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.File
import java.util.Properties

/**
 * Step 13 (M-NATIVE-6 = NR-1): a host-runnable, **load-bearing** assertion that
 * the **MERGED** Android manifest the build consumes carries the `phoneCall`
 * foreground-service type on `LifecycleService` **and** the
 * `FOREGROUND_SERVICE_PHONE_CALL` permission.
 *
 * NR-1 is a **false alarm** on the *source* manifests (they are complete). The
 * real residue is the **absence of an automated gate**: a dropped `phoneCall`
 * FGS type would ship **silently** and break locked / closed-webview ringing on
 * Android 14+ (the OS rejects `startForeground(..., FOREGROUND_SERVICE_TYPE_PHONE_CALL)`
 * without the manifest declaration). A test that merely greps the **source**
 * manifest is necessary-not-sufficient (the source was never the problem) — so
 * this reads the **merged** manifest AGP points the unit-test runtime at via
 * `com.android.tools.test_config.properties` (`android_merged_manifest`, the
 * exact post-merge XML Robolectric ingests), with a build-intermediates fallback.
 *
 * **NV-MUT (AC4):** dropping `phoneCall` (and/or `FOREGROUND_SERVICE_PHONE_CALL`)
 * from the plugin source manifest regenerates this merged manifest without it
 * (the build re-runs `processDebugUnitTestManifest` under `--rerun-tasks`) → the
 * assertions go RED; restoring it goes GREEN.
 *
 * **Scope honesty:** this gates the **plugin module's** merged manifest — the
 * merge the host *can* run. The full **per-variant app-APK** `aapt dump
 * permissions` over every packaged variant (heavy `:app:assemble*` on a
 * RAM-constrained host) is the CI runbook in `scripts/assert-merged-manifest.sh`.
 */
@RunWith(RobolectricTestRunner::class)
class MergedManifestForegroundServiceTest {

    /**
     * The text of the **merged** manifest the unit-test runtime consumes — NOT
     * the source `src/main/AndroidManifest.xml`. Preferred path: the
     * `android_merged_manifest` AGP records in `test_config.properties` (the same
     * file Robolectric reads). Fallback: the well-known AGP intermediate for this
     * module's debug unit test.
     */
    private fun mergedManifestText(): String {
        readViaTestConfig()?.let { return it }
        val fallback = File(
            "build/intermediates/packaged_manifests/debugUnitTest/" +
                "processDebugUnitTestManifest/AndroidManifest.xml",
        )
        assertTrue(
            "merged manifest not found via test_config.properties nor at " +
                "'${fallback.path}' (cwd=${File(".").absolutePath}) — did " +
                "processDebugUnitTestManifest run?",
            fallback.isFile,
        )
        return fallback.readText()
    }

    private fun readViaTestConfig(): String? {
        val loader = javaClass.classLoader ?: return null
        // AGP writes the file under out/com/android/tools/; both forms have been
        // used as the classpath resource name across AGP/Robolectric versions.
        val candidates = listOf(
            "com/android/tools/test_config.properties",
            "com.android.tools.test_config.properties",
        )
        for (name in candidates) {
            val stream = loader.getResourceAsStream(name) ?: continue
            val props = Properties()
            stream.use { props.load(it) }
            val path = props.getProperty("android_merged_manifest") ?: continue
            val file = File(path)
            if (file.isFile) return file.readText()
        }
        return null
    }

    @Test
    fun mergedManifest_lifecycleService_declaresPhoneCallForegroundServiceType() {
        val xml = mergedManifestText()
        val fgsType = Regex("android:foregroundServiceType=\"([^\"]*)\"")
            .find(xml)?.groupValues?.get(1)
        assertNotNull(
            "the merged manifest declares no android:foregroundServiceType at all",
            fgsType,
        )
        assertTrue(
            "phoneCall FGS type dropped from the MERGED manifest (got: '$fgsType') — " +
                "locked / closed-webview call ringing breaks on Android 14+",
            fgsType!!.split("|").map { it.trim() }.contains("phoneCall"),
        )
    }

    @Test
    fun mergedManifest_declaresForegroundServicePhoneCallPermission() {
        val xml = mergedManifestText()
        assertTrue(
            "FOREGROUND_SERVICE_PHONE_CALL permission dropped from the MERGED manifest — " +
                "the phoneCall foreground service cannot start on Android 14+",
            xml.contains("android.permission.FOREGROUND_SERVICE_PHONE_CALL"),
        )
    }

    /**
     * Framework-level corroboration: the platform PackageManager's parse of the
     * merged manifest must also surface the call permission. This proves the
     * declaration survives an actual manifest parse (not just a textual grep).
     */
    @Test
    @Config(sdk = [34])
    fun packageManager_parsesForegroundServicePhoneCallPermission() {
        val ctx = ApplicationProvider.getApplicationContext<Context>()
        val info = ctx.packageManager.getPackageInfo(
            ctx.packageName,
            PackageManager.GET_PERMISSIONS,
        )
        val perms = info.requestedPermissions?.toList().orEmpty()
        assertTrue(
            "framework parse of the merged manifest is missing " +
                "FOREGROUND_SERVICE_PHONE_CALL (requested: $perms)",
            perms.contains("android.permission.FOREGROUND_SERVICE_PHONE_CALL"),
        )
    }
}
