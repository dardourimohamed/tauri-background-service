package app.tauri.backgroundservice

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * NTF-04 (Step 7b): PURE JVM unit tests for [decideNotificationOutcome] — no
 * Robolectric, no JNI, no Android (OOM-safe; runnable under fleet collapse).
 *
 * Pins the recoverable/permanent discriminator + the anti-loop gate (the Kotlin
 * HeadlessCoreResult.failure() synthetics hardcode recoverable=true for PERMANENT
 * pre-JNI env failures — re-presenting on the naive "recoverable==true" rule
 * alone LOOPS FOREVER; `code` present in rawJson <=> synthetic <=> PERMANENT) +
 * replyText preservation at the decision layer (AC3 layer 1; the notification
 * body is asserted in NotificationActionRePresentTest).
 *
 * See 7a-FINALIZER CARRY-FORWARD #1 in the task description.
 */
class NotificationActionOutcomeTest {

    @Test
    fun success_cancels() {
        val result = HeadlessCoreResult(
            ok = true,
            state = "running",
            message = null,
            recoverable = false,
            rawJson = """{"ok":true,"state":"running"}""",
        )
        assertEquals(
            NotificationActionOutcome.Cancel,
            decideNotificationOutcome(result, replyText = "hi"),
        )
    }

    @Test
    fun rustPermanent_notRecoverable_cancels() {
        // Mirrors headless_core.rs:313 empty-reply verdict: recoverable == false.
        val result = HeadlessCoreResult(
            ok = false,
            state = "failed",
            message = "empty notification reply",
            recoverable = false,
            rawJson = """{"ok":false,"state":"failed","recoverable":false,"message":"empty notification reply"}""",
        )
        assertEquals(
            NotificationActionOutcome.Cancel,
            decideNotificationOutcome(result, replyText = ""),
        )
    }

    @Test
    fun rustRecoverable_noCode_rePresentsPreservingReplyText() {
        // Mirrors headless_core.rs:292/:301 "core not running" verdict:
        // recoverable == true, NO `code` field (Rust never emits one).
        val result = HeadlessCoreResult(
            ok = false,
            state = "failed",
            message = "core not running; cannot dispatch notification action",
            recoverable = true,
            rawJson = """{"ok":false,"state":"failed","recoverable":true,"message":"core not running"}""",
        )
        val outcome = decideNotificationOutcome(result, replyText = "draft reply")
        assertTrue("Rust-recoverable (no code) must RE-PRESENT", outcome is NotificationActionOutcome.RePresent)
        assertEquals("draft reply", (outcome as NotificationActionOutcome.RePresent).replyText)
    }

    @Test
    fun antiLoop_nativeLibraryLoadFailed_cancels() {
        // 7a-FINALIZER CARRY-FORWARD #1: the Kotlin synthetic hardcodes
        // recoverable=true for a PERMANENT env failure. Re-presenting LOOPS.
        assertEquals(
            NotificationActionOutcome.Cancel,
            decideNotificationOutcome(syntheticPermanent("native_library_load_failed", "load err"), replyText = "hi"),
        )
    }

    @Test
    fun antiLoop_dataDirUnavailable_cancels() {
        assertEquals(
            NotificationActionOutcome.Cancel,
            decideNotificationOutcome(syntheticPermanent("data_dir_unavailable", "mkdirs failed"), replyText = "hi"),
        )
    }

    @Test
    fun antiLoop_invalidHeadlessCoreResponse_cancels() {
        assertEquals(
            NotificationActionOutcome.Cancel,
            decideNotificationOutcome(syntheticPermanent("invalid_headless_core_response", "bad json"), replyText = "hi"),
        )
    }

    /**
     * Mirrors the exact HeadlessCoreResult HeadlessCoreBridge.failure() emits for
     * a pre-JNI permanent env failure (ok=false, recoverable=true, rawJson carries
     * the `"code":` key). Built via the public constructor (NOT failure()) so this
     * stays a PURE JVM test — failure() builds its rawJson through org.json's
     * JSONObject, which is the Android stub under non-Robolectric unit tests
     * (unitTests.isReturnDefaultValues=true) and returns null there. The
     * failure()-emitted shape itself is exercised under Robolectric in
     * NotificationActionRePresentTest.permanentSyntheticCode_cancels_antiLoop.
     */
    private fun syntheticPermanent(code: String, message: String): HeadlessCoreResult =
        HeadlessCoreResult(
            ok = false,
            state = "failed",
            message = message,
            recoverable = true,
            rawJson = """{"ok":false,"state":"failed","code":"$code","message":"$message","recoverable":true}""",
        )
}
