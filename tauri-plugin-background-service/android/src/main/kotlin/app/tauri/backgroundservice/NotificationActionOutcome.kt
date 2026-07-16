package app.tauri.backgroundservice

import android.content.Context

/**
 * NTF-04 (Step 7b): the result of deciding what to do with a notification action
 * after the headless Core reports back. Distilled from a [HeadlessBridgeResult] by
 * [decideNotificationOutcome]; applied to the OS notification by
 * [handleNotificationActionResult].
 */
sealed class NotificationActionOutcome {
    /**
     * The action succeeded OR failed permanently — dismiss the notification.
     * Success: the reply was sent / mark-read applied. Permanent failure: an
     * unrecoverable error (empty reply, unknown action, or a pre-JNI env failure
     * that will not resolve on retry).
     */
    object Cancel : NotificationActionOutcome()

    /**
     * The action failed RECOVERABLY (a locked/dead Core a retry may satisfy).
     * RE-PRESENT the notification preserving the user's [replyText] so the typed
     * reply is not lost while the user believes it was sent (NTF-04 AC3).
     */
    data class RePresent(val replyText: String) : NotificationActionOutcome()
}

/**
 * NTF-04 (Step 7b): PURE discriminator. Decides a [NotificationActionOutcome] from
 * a [HeadlessBridgeResult] and the user's replyText. No Android, no JNI — directly
 * unit-testable with constructed results (see NotificationActionOutcomeTest).
 *
 * `recoverable` is AMBIGUOUS (7a-FINALIZER CARRY-FORWARD #1, verified first-hand
 * against HeadlessBridge.kt + tauri/src/headless_core.rs @ 819c2394): the
 * Rust HeadlessCoreReport sets recoverable=true ONLY for a locked/dead Core a
 * retry may satisfy (=> RE-PRESENT, preserving the reply). But the Kotlin
 * HeadlessBridgeResult.failure() synthetic ALSO hardcodes recoverable=true for
 * PERMANENT pre-JNI env failures (native_library_load_failed /
 * data_dir_unavailable / invalid_headless_core_response), and it is the ONLY
 * emitter of a `code` field. So:
 *   code present in rawJson  <=>  Kotlin-synthetic  <=>  PERMANENT.
 *
 *   RE-PRESENT  <=>  !ok && recoverable && no `code` in rawJson
 *   CANCEL      <=>  ok || !recoverable || `code` present
 *
 * Re-presenting on the naive "recoverable==true" rule alone LOOPS FOREVER on the
 * synthetic env failures above — the `code` discriminator is what prevents that.
 *
 * The `code` presence is detected by a substring scan for the JSON key `"code":`
 * (NOT org.json.JSONObject) so this function is a genuinely PURE JVM predicate —
 * no Android, no JNI — directly unit-testable without Robolectric (the OOM-safe
 * fallback surface). The Rust HeadlessCoreReport has no `code` field (serde never
 * emits one), so only HeadlessBridgeResult.failure()'s synthetic carries the key.
 */
fun decideNotificationOutcome(
    result: HeadlessBridgeResult,
    replyText: String,
): NotificationActionOutcome {
    // Success or a Rust-permanent verdict (recoverable == false, e.g. the empty
    // reply at headless_core.rs:313) → dismiss.
    if (result.ok || !result.recoverable) {
        return NotificationActionOutcome.Cancel
    }
    // recoverable == true: distinguish the Rust-path locked/dead Core (no `code`
    // key → RE-PRESENT) from a Kotlin-synthetic permanent env failure (`"code":`
    // present → CANCEL, never re-present — the anti-loop gate).
    val hasSyntheticCode = result.rawJson.contains("\"code\":")
    return if (!hasSyntheticCode) NotificationActionOutcome.RePresent(replyText)
    else NotificationActionOutcome.Cancel
}

/**
 * NTF-04 (Step 7b): apply a [NotificationActionOutcome] — dismiss the notification
 * on [NotificationActionOutcome.Cancel], or RE-PRESENT it (preserving replyText)
 * on [NotificationActionOutcome.RePresent]. The Core result has already been
 * distilled into the outcome by [decideNotificationOutcome]; the JNI bridge call
 * stays in RealMessageNotificationActionDispatcher.dispatch, so this is testable
 * under Robolectric with a CONSTRUCTED outcome (no JNI — see
 * NotificationActionRePresentTest).
 */
internal fun handleNotificationActionResult(
    context: Context,
    outcome: NotificationActionOutcome,
    chatId: String,
    messageId: String,
    notificationId: Int,
) {
    when (outcome) {
        NotificationActionOutcome.Cancel ->
            // NTF-13 (Step 9c): clear the WHOLE chat by tag — message AND per-chat summary
            // — not just the tapped id. The Cancel outcome is shared by reply-success,
            // mark-read-success (headless_core.rs mark_messages_read marks the WHOLE chat),
            // and permanent-failure; clearing the chat's own <=1 message + summary in each
            // is intentional. cancelChat enumerates active notifications under the chat's
            // tag and cancels each, making the summary dismissal explicit + version-robust
            // (AOSP auto-removes a child-less summary, but that is not OEM-robust).
            ActionableMessageNotifier.cancelChat(context, chatId)
        is NotificationActionOutcome.RePresent ->
            rePresentNotification(context, outcome, chatId, messageId, notificationId)
    }
}

/**
 * RE-PRESENT the notification with a degraded "reply pending" surface. The reply
 * action intent carries only ids + route uri — NOT the original title/body/icon
 * (CHANGES #4, Builder decision: payload fidelity). The user's typed replyText is
 * preserved in the body (AC3) so it is not lost; the route is reconstructed from
 * the ids (the same bg-service://chat route the GUI forwarder uses), so tapping re-opens
 * the conversation. An empty replyText (a re-presented mark-read) falls back to a
 * localized "tap to retry" body.
 */
private fun rePresentNotification(
    context: Context,
    outcome: NotificationActionOutcome.RePresent,
    chatId: String,
    messageId: String,
    notificationId: Int,
) {
    val body = outcome.replyText.takeIf { it.isNotEmpty() }
        ?: context.getString(R.string.bg_service_notif_reply_pending_body)
    ActionableMessageNotifier.showMessageNotification(
        context = context,
        notificationId = notificationId,
        chatId = chatId,
        messageId = messageId,
        title = context.getString(R.string.bg_service_notif_reply_pending_title),
        body = body,
        routeUri = "bg-service://chat?chat_id=$chatId&message_id=$messageId",
        smallIcon = NotificationIconResolver.resolve(context),
        launchIntent = context.packageManager.getLaunchIntentForPackage(context.packageName),
    )
}
