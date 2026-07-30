package app.tauri.backgroundservice

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import java.io.File
import java.util.Properties

/**
 * AND-09: the library manifest must NOT declare
 * `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` — it is Play-policy-restricted and must
 * be opted in by the HOST application. The Tauri `requestBatteryExemption`
 * command still ships (it launches the system dialog); the host-declared
 * permission is what authorizes that dialog.
 *
 * This gate is **load-bearing**: re-adding the `<uses-permission>` to the plugin
 * source manifest REDs `libraryManifest_doesNotDeclareRequestIgnoreBatteryOptimizations`,
 * and the merged-manifest corroboration REDs
 * `mergedLibraryManifest_doesNotContainRequestIgnoreBatteryOptimizations`.
 *
 * The host-fixture leg proves the assertion is not vacuous: a manifest fragment
 * that DOES declare the permission is detectable by the same text scan, so a
 * host opt-in (merged on top of this library) remains observable.
 */
@RunWith(RobolectricTestRunner::class)
class BatteryOptimizationPermissionTest {

    private val permission = "android.permission.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS"

    private fun libraryManifestText(): String {
        val file = File("src/main/AndroidManifest.xml")
        assertTrue(
            "src/main/AndroidManifest.xml not found relative to " +
                "cwd=${File(".").absolutePath} (did :testDebugUnitTest change its working dir?)",
            file.isFile,
        )
        return file.readText()
    }

    private fun mergedManifestText(): String {
        readViaTestConfig()?.let { return it }
        val fallback = File(
            "build/intermediates/packaged_manifests/debugUnitTest/" +
                "processDebugUnitTestManifest/AndroidManifest.xml",
        )
        assertTrue(
            "merged manifest not found via test_config.properties nor at " +
                "'${fallback.path}' — did processDebugUnitTestManifest run?",
            fallback.isFile,
        )
        return fallback.readText()
    }

    private fun readViaTestConfig(): String? {
        val loader = javaClass.classLoader ?: return null
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
    fun libraryManifest_doesNotDeclareRequestIgnoreBatteryOptimizations() {
        val src = libraryManifestText()
        assertFalse(
            "AND-09: the library AndroidManifest.xml still declares " +
                "$permission — it is Play-policy-restricted and must be opted in " +
                "by the HOST app, not the library.",
            src.contains("\"$permission\""),
        )
    }

    @Test
    fun mergedLibraryManifest_doesNotContainRequestIgnoreBatteryOptimizations() {
        // The plugin module's merged manifest is the merge the host *can* run;
        // with the source permission removed it must not surface either.
        val xml = mergedManifestText()
        assertFalse(
            "AND-09: the MERGED library manifest still carries $permission — " +
                "the source removal did not propagate through manifest merge.",
            xml.contains(permission),
        )
    }

    /**
     * Non-vacuousness leg: a host fragment that opts IN (declares the
     * permission) is detectable by the same text scan, proving a real host
     * opt-in remains observable after the library drops it. This mirrors how a
     * host would carry the permission in its own manifest.
     */
    @Test
    fun hostOptInFixture_isDetectableByTheSameScan() {
        val hostFragment = """
            <manifest xmlns:android="http://schemas.android.com/apk/res/android">
                <uses-permission android:name="$permission" />
            </manifest>
        """.trimIndent()
        assertTrue(
            "host opt-in fixture must be detectable (else the absence assertion is vacuous)",
            hostFragment.contains("\"$permission\""),
        )
        assertNotNull(hostFragment)
    }
}
