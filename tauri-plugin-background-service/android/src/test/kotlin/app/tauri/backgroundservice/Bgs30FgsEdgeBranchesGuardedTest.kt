package app.tauri.backgroundservice

import android.content.ComponentName
import android.content.Context
import android.content.ContextWrapper
import android.content.Intent
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.File

/**
 * BGS-30 (doc-08 Step 13 Task 2): host gate for the guarded FGS edge branches.
 *
 * The actual crash class — a background-start `IllegalStateException` on Android
 * O+, `ForegroundServiceStartNotAllowedException` on Android 12+, or the deferred
 * `ForegroundServiceDidNotStartInTime` — is **device-gated**: a host Robolectric
 * run never sees the system reject a start. So the host gate is a **dual-leg
 * STATIC** assertion over the plugin source text (mirrors the
 * `MergedManifestForegroundServiceTest` source-reading precedent), plus an
 * optional Robolectric behavioral leg.
 *
 * - **Leg A — call-site routing:** every formerly-unguarded
 *   `startService`/`startForegroundService` site routes through the shared
 *   `startServiceGuarded` helper, so NONE keeps a bare
 *   `activity/context.startService(`/`startForegroundService(` call. The only
 *   remaining bare `activity.start*` calls are the pre-existing F7 ACK try/catch
 *   in `BackgroundServicePlugin` (~:240-243), which this task leaves untouched.
 * - **Leg B — helper-body guard:** the helper body wraps the start in
 *   `try { ... } catch (e: ... )`, so a helper-neuter mutant (routing kept,
 *   catch removed) REDs here.
 *
 * **NV-MUT (cp-backup restore, sha256-verified):** (a) revert one call site to a
 * bare `activity.startService(` → Leg A REDs; (b) remove `catch` from the helper
 * body → Leg B REDs. Both discriminate; existing plugin/boot/lifecycle tests
 * stay GREEN.
 *
 * Device FGS-contract reachability is a NON-BLOCKING Step 21 runbook carry-forward.
 */
@RunWith(RobolectricTestRunner::class)
class Bgs30FgsEdgeBranchesGuardedTest {

    private fun mainSrc(fileName: String): String {
        val file = File("src/main/kotlin/app/tauri/backgroundservice/$fileName")
        assertTrue(
            "$fileName not found relative to cwd=${File(".").absolutePath} " +
                "(did :testDebugUnitTest change its working dir?)",
            file.isFile,
        )
        return file.readText()
    }

    // ── LEG A: call-site routing ───────────────────────────────────────

    /**
     * Leg A — the three formerly-unguarded plugin sites (onFailure ACTION_STOP,
     * stopKeepalive ACTION_STOP, updateForegroundServiceType ACTION_UPDATE_TYPE)
     * must route through `startServiceGuarded`. The ONLY remaining bare
     * `activity.startService(`/`activity.startForegroundService(` calls are the
     * pre-existing F7 ACK try/catch — exactly two. A revert of any of the three
     * sites to a bare `activity.start*` call bumps the count above 2 and REDs.
     */
    @Test
    fun bgs30_legA_plugin_startSites_routeThroughGuardedHelper() {
        val src = mainSrc("BackgroundServicePlugin.kt")
        assertTrue(
            "shared startServiceGuarded helper is missing from BackgroundServicePlugin.kt",
            src.contains("fun startServiceGuarded("),
        )
        val bareActivityStarts = Regex("""activity\.(startForegroundService|startService)\(""")
            .findAll(src).toList()
        assertEquals(
            "Plugin still has unguarded activity.start* call sites — expected only the 2 " +
                "F7 ACK calls (the pre-existing guarded site), but found: " +
                bareActivityStarts.map { it.value },
            2,
            bareActivityStarts.size,
        )
    }

    /**
     * Leg A — BootReceiver.startRecoveryService (ACTION_START) must route through
     * `startServiceGuarded`. BootReceiver has no other start call, so a bare
     * `context.startService(`/`context.startForegroundService(` count of 0 proves
     * site 4 is fully routed.
     */
    @Test
    fun bgs30_legA_bootReceiver_startRecoveryService_routesThroughGuardedHelper() {
        val src = mainSrc("BootReceiver.kt")
        val bareContextStarts = Regex("""context\.(startForegroundService|startService)\(""")
            .findAll(src).toList()
        assertTrue(
            "BootReceiver.startRecoveryService still calls a bare context.start* — must route " +
                "through startServiceGuarded; found: ${bareContextStarts.map { it.value }}",
            bareContextStarts.isEmpty(),
        )
        assertTrue(
            "BootReceiver.startRecoveryService does not call startServiceGuarded",
            src.contains("startServiceGuarded("),
        )
    }

    // ── LEG B: helper-body guard ───────────────────────────────────────

    /**
     * Leg B — the guarded helper body must wrap the start in `try/catch`. Asserts
     * on the EXTRACTED helper body (not the whole file, which has unrelated
     * `catch (e:` blocks like the F7 ACK handler) so a helper-neuter mutant
     * (remove the try/catch while keeping call-site routing) REDs here.
     */
    @Test
    fun bgs30_legB_guardedHelper_wrapsStartInCatch() {
        val src = mainSrc("BackgroundServicePlugin.kt")
        val body = extractTopLevelFunBody(src, "startServiceGuarded")
        assertTrue(
            "startServiceGuarded body must call startForegroundService/startService inside the guard",
            body.contains("startForegroundService(") || body.contains("startService("),
        )
        assertTrue(
            "startServiceGuarded body must wrap the start in try/catch " +
                "(helper-neuter mutant would drop the catch); body was:\n$body",
            Regex("""catch\s*\(\s*e\s*:""").containsMatchIn(body),
        )
    }

    /**
     * Extract the `{ ... }` body of a top-level `fun <name>(...)` by brace matching.
     * Strips `//` line comments and `"..."` string literals first, so braces inside
     * string templates (`${...}`) or comments cannot unbalance the count.
     */
    private fun extractTopLevelFunBody(src: String, funName: String): String {
        // Sanitize BEFORE indexing: stripping line comments + string literals shortens the
        // text, so a defIdx computed on the raw source no longer aligns with the sanitized
        // text (the helper would appear to have no opening brace past the stale offset).
        val sanitized = src
            .replace(Regex("//[^\n]*"), "")                       // strip line comments
            .replace(Regex("\"(?:\\\\.|[^\"\\\\])*\""), "\"\"")    // strip string literals
        val defIdx = sanitized.indexOf("fun $funName(")
        assertTrue("top-level fun $funName(...) definition not found in source", defIdx >= 0)
        val firstBrace = sanitized.indexOf('{', defIdx)
        assertTrue("fun $funName opening brace not found", firstBrace >= 0)
        var depth = 0
        var i = firstBrace
        while (i < sanitized.length) {
            when (sanitized[i]) {
                '{' -> depth++
                '}' -> {
                    depth--
                    if (depth == 0) return sanitized.substring(firstBrace, i + 1)
                }
            }
            i++
        }
        fail("unbalanced braces while extracting fun $funName body")
        return ""
    }

    // ── Optional behavioral leg: the catch actually swallows ──────────

    /**
     * Behavioral corroboration: a throwing start (the host's stand-in for the
     * device-only OS start-restriction) must NOT propagate out of the guarded
     * helper. Exercises both the `foreground=false` (startService) and
     * `foreground=true` (startForegroundService on API ≥ O) branches under
     * Robolectric's API 33.
     */
    @Test
    @Config(sdk = [33])
    fun bgs30_guardedHelper_swallowsThrownStartException() {
        val base = ApplicationProvider.getApplicationContext<Context>()
        val throwing = ThrowingServiceStartContext(base)
        val stopIntent = Intent(base, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_STOP
        }
        val startIntent = Intent(base, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
        }
        // Neither call may throw — the guard's catch swallows the IllegalStateException.
        startServiceGuarded(throwing, stopIntent, foreground = false)
        startServiceGuarded(throwing, startIntent, foreground = true)
    }

    /** A [ContextWrapper] whose start calls always throw, simulating an OS start-restriction. */
    private class ThrowingServiceStartContext(base: Context) : ContextWrapper(base) {
        override fun startService(service: Intent?): ComponentName? =
            throw IllegalStateException("bgs30_test_bg_start_blocked")
        override fun startForegroundService(service: Intent): ComponentName? =
            throw IllegalStateException("bgs30_test_fgs_start_blocked")
    }
}
