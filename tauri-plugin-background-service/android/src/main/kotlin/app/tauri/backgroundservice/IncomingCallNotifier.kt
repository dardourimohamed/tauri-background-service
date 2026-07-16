package app.tauri.backgroundservice

import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import androidx.annotation.VisibleForTesting
import java.util.concurrent.ConcurrentHashMap

/**
 * Native incoming-call notifier (spec 08 C6, Step 15).
 *
 * Fires the high-priority `CallStyle` notification when a call offer arrives
 * headlessly (the webview is closed and the headless core under the
 * `remoteMessaging` FGS received the offer). The notification:
 *
 * - Uses the dedicated RINGTONE channel so the device rings.
 * - Attaches a full-screen intent when `USE_FULL_SCREEN_INTENT` is granted,
 *   so a locked-screen device shows the native incoming-call surface.
 * - Falls back (F4) to the same `CallStyle` notification — still ringing via
 *   the channel — when the full-screen-intent grant is missing or revoked,
 *   and surfaces a Settings deep-link during call onboarding.
 *
 * The trigger is the Rust event layer (Tauri), which calls the plugin command
 * wired in [BackgroundServicePlugin]; this object is the pure Android half and
 * is JVM/Robolectric-testable.
 */
object IncomingCallNotifier {

    private const val TAG = "IncomingCallNotifier"
    const val CALL_NOTIFICATION_BASE = 9100

    /**
     * Ring auto-cancel timeout (BGS-07, Step 10). An abandoned incoming call
     * whose caller gives up before the headless forwarder sees a `CallEnded`
     * must not leave the full-screen CallStyle notification ringing forever, so
     * the notification is auto-cancelled after this delay.
     */
    // PRODUCT DECISION: 60s headless incoming-ring timeout (doc 08, BGS-07)
    const val RING_TIMEOUT_MS = 60_000L

    /**
     * Testable timeout seam. Schedules [action] after [delayMs] and returns a
     * canceler that drops the pending run. Production posts to a main-looper
     * [Handler]; tests inject a manual scheduler so the ring timeout fires
     * deterministically with no wall-clock sleep.
     */
    fun interface TimeoutScheduler {
        fun schedule(delayMs: Long, action: () -> Unit): () -> Unit
    }

    @VisibleForTesting
    internal var timeoutScheduler: TimeoutScheduler = TimeoutScheduler { delayMs, action ->
        val handler = Handler(Looper.getMainLooper())
        val runnable = Runnable { action() }
        handler.postDelayed(runnable, delayMs)
        ({ handler.removeCallbacks(runnable) })
    }

    /** Pending ring-timeout cancelers, keyed by call id. */
    private val ringTimeouts = ConcurrentHashMap<String, () -> Unit>()

    /** Deep-link extra carried on the launch intent into the webview. */
    const val EXTRA_CALL_ID = "bg_service.call_id"
    const val EXTRA_CALL_ACTION = "bg_service.call_action"
    const val ACTION_ANSWER = "answer"
    const val ACTION_DECLINE = "decline"

    /**
     * Whether this app may post a full-screen intent (spec 08 C6 F4).
     *
     * `USE_FULL_SCREEN_INTENT` is granted by default for apps with calling/
     * notification responsibilities; on API 34+ the user (or the system) may
     * revoke it, in which case the notification falls back to CallStyle +
     * ringtone. Pre-API-29 the concept does not exist; pre-API-34 the grant is
     * effectively always present once the permission is declared.
     */
    @VisibleForTesting
    fun canUseFullScreenIntent(context: Context): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return false
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            return nm.canUseFullScreenIntent()
        }
        // API 29–33: auto-granted when the permission is declared.
        return true
    }

    /** Stable, non-negative per-call notification id. */
    fun notificationIdFor(callId: String): Int {
        val hash = callId.hashCode()
        return CALL_NOTIFICATION_BASE + (if (hash < 0) hash.inv() else hash)
    }

    /**
     * Show the incoming-call notification. Computes the full-screen-intent
     * eligibility via [canUseFullScreenIntent].
     */
    fun showIncomingCall(
        context: Context,
        callId: String,
        callerName: CharSequence,
        isVideo: Boolean,
        smallIcon: Int,
        launchIntent: Intent?,
    ) {
        showIncomingCall(
            context, callId, callerName, isVideo, smallIcon, launchIntent,
            useFullScreenIntent = canUseFullScreenIntent(context),
        )
    }

    /**
     * Show the incoming-call notification with an explicit full-screen-intent
     * flag — the testable seam for the F4 fallback branch.
     */
    @VisibleForTesting
    fun showIncomingCall(
        context: Context,
        callId: String,
        callerName: CharSequence,
        isVideo: Boolean,
        smallIcon: Int,
        launchIntent: Intent?,
        useFullScreenIntent: Boolean,
    ) {
        NotificationHelper.ensureCallChannel(context)

        val fullScreenIntent = fullScreenPendingIntent(context, callId, launchIntent)
        val answerIntent = callActionPendingIntent(context, callId, ACTION_ANSWER, launchIntent)
        val declineIntent = callActionPendingIntent(context, callId, ACTION_DECLINE, launchIntent)

        val notification = NotificationHelper.buildIncomingCallNotification(
            context = context,
            channelId = NotificationHelper.CALL_CHANNEL_ID,
            callerName = callerName,
            isVideo = isVideo,
            smallIcon = smallIcon,
            fullScreenIntent = fullScreenIntent,
            answerIntent = answerIntent,
            declineIntent = declineIntent,
            useFullScreenIntent = useFullScreenIntent,
        )

        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.notify(notificationIdFor(callId), notification)
        Log.i(TAG, "showIncomingCall: callId=$callId, video=$isVideo, fsi=$useFullScreenIntent")

        // BGS-07 (Step 10): arm the auto-cancel timeout so an abandoned call
        // does not ring forever. Use the application context — the timeout may
        // outlive the caller's (possibly Activity) context.
        scheduleRingTimeout(context.applicationContext ?: context, callId)
    }

    /** Cancel the incoming-call notification for [callId]. */
    fun cancel(context: Context, callId: String) {
        // Drop any pending auto-cancel timeout first (call answered / declined /
        // ended, or a re-ring for the same id) so a stale timeout cannot cancel
        // a fresh notification (BGS-07, Step 10).
        ringTimeouts.remove(callId)?.invoke()
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.cancel(notificationIdFor(callId))
        Log.i(TAG, "cancel: callId=$callId")
    }

    /**
     * Arm the ring auto-cancel timeout for [callId] (BGS-07, Step 10). Replaces
     * any prior timeout for the same call id; on fire it clears its own
     * bookkeeping and cancels the ring via [cancel].
     */
    private fun scheduleRingTimeout(context: Context, callId: String) {
        ringTimeouts.remove(callId)?.invoke()
        val canceler = timeoutScheduler.schedule(RING_TIMEOUT_MS) {
            // Remove our own entry before cancelling so cancel()'s canceler
            // lookup is a no-op (the timeout already fired).
            ringTimeouts.remove(callId)
            Log.i(TAG, "ring timeout: auto-cancel callId=$callId")
            cancel(context, callId)
        }
        ringTimeouts[callId] = canceler
    }

    /** Settings deep-link to the app's notification page (call onboarding, F4). */
    fun openFullScreenIntentSettings(context: Context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return
        val intent = Intent(
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE)
                android.provider.Settings.ACTION_MANAGE_APP_USE_FULL_SCREEN_INTENT
            else
                android.provider.Settings.ACTION_APP_NOTIFICATION_SETTINGS
        ).apply {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                data = Uri.parse("package:${context.packageName}")
            } else {
                putExtra(android.provider.Settings.EXTRA_APP_PACKAGE, context.packageName)
            }
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        try {
            context.startActivity(intent)
        } catch (e: Exception) {
            Log.w(TAG, "Could not open full-screen-intent settings: ${e.message}")
        }
    }

    private fun baseLaunchIntent(context: Context, callId: String, launchIntent: Intent?): Intent {
        val base = launchIntent ?: context.packageManager.getLaunchIntentForPackage(context.packageName)
            ?: Intent()
        return Intent(base).apply {
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
            putExtra(EXTRA_CALL_ID, callId)
        }
    }

    private fun fullScreenPendingIntent(
        context: Context,
        callId: String,
        launchIntent: Intent?,
    ): PendingIntent? {
        val intent = baseLaunchIntent(context, callId, launchIntent)
        return PendingIntent.getActivity(
            context,
            // Unique request code per call so the FSI targets this call.
            notificationIdFor(callId),
            intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
    }

    /**
     * Build the Answer/Decline action [PendingIntent] (M-NATIVE-1, Step 9).
     *
     * This is a **`getBroadcast`** intent targeting [CallActionReceiver] — NOT a
     * `getActivity` launcher. The broadcast runs while the device is locked / the
     * webview is closed, so the receiver can drive the Rust control plane
     * (`answer_call`/`reject_call`) directly. The prior `getActivity` form merely
     * opened the app and left `EXTRA_CALL_ACTION` unconsumed (the inert ring).
     *
     * The full-screen / content intents stay activity launchers (the ringing
     * surface); only the **answer/decline** action must reach the core.
     */
    @VisibleForTesting
    internal fun callActionPendingIntent(
        context: Context,
        callId: String,
        action: String,
        @Suppress("UNUSED_PARAMETER") launchIntent: Intent?,
    ): PendingIntent {
        val intent = Intent(context, CallActionReceiver::class.java).apply {
            this.action = CallActionReceiver.ACTION_CALL_ACTION
            putExtra(EXTRA_CALL_ID, callId)
            putExtra(EXTRA_CALL_ACTION, action)
        }
        // Distinct request codes keep answer/decline intents independent.
        val requestCode = notificationIdFor(callId) + if (action == ACTION_ANSWER) 1 else 2
        return PendingIntent.getBroadcast(
            context,
            requestCode,
            intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
    }
}
