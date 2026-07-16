package app.tauri.backgroundservice

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Person
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.media.AudioAttributes
import android.os.Build
import androidx.core.app.NotificationCompat

object NotificationHelper {

    /**
     * Dedicated channel for incoming voice/video calls (spec 08 C6, Step 15).
     *
     * `USAGE_NOTIFICATION_RINGTONE` + `IMPORTANCE_HIGH` makes the platform treat
     * the notification as a ringing call: it plays the system ringtone (not the
     * message sound), vibrates, and — when the app holds the
     * `USE_FULL_SCREEN_INTENT` grant — surfaces full-screen on a locked device.
     * This is also the F4 "CallStyle+ringtone fallback" channel used when the
     * full-screen-intent grant has been revoked.
     */
    const val CALL_CHANNEL_ID = "incoming_calls"
    const val CALL_CHANNEL_NAME = "Incoming Calls"

    fun ensureChannel(
        context: Context,
        channelId: String,
        name: String,
        importance: Int,
        description: String? = null,
        showBadge: Boolean = importance != NotificationManager.IMPORTANCE_LOW,
    ) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(channelId, name, importance).apply {
                this.description = description
                setShowBadge(showBadge)
            }
            val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            nm.createNotificationChannel(channel)
        }
    }

    /**
     * Create the dedicated incoming-call channel (spec 08 C6, Step 15).
     *
     * RINGTONE usage + vibration so a headless incoming offer rings the device
     * even with the webview closed. Idempotent — safe to call before every ring.
     */
    fun ensureCallChannel(context: Context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val channel = NotificationChannel(
            CALL_CHANNEL_ID,
            CALL_CHANNEL_NAME,
            NotificationManager.IMPORTANCE_HIGH,
        ).apply {
            description = "Incoming voice and video calls"
            // Ringtone-grade audio: the platform routes this through the call
            // audio stream and respects the user's ringtone volume.
            setSound(
                android.provider.Settings.System.DEFAULT_RINGTONE_URI,
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                    .build(),
            )
            enableVibration(true)
            // A long, call-like vibration pattern.
            vibrationPattern = longArrayOf(0, 1000, 1000)
            setShowBadge(false)
            lockscreenVisibility = Notification.VISIBILITY_PUBLIC
        }
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.createNotificationChannel(channel)
    }

    /**
     * Build the incoming-call notification (spec 08 C6, Step 15).
     *
     * On API 34+ it uses `Notification.CallStyle` so the system renders the
     * native call UI (caller identity, answer/decline affordances, system call
     * sheet). On older platforms it falls back to a max-priority heads-up
     * notification of `CATEGORY_CALL` with explicit Answer/Decline actions.
     *
     * The full-screen intent is attached only when [useFullScreenIntent] is
     * true (the caller has already checked `canUseFullScreenIntent`); when
     * revoked, the same CallStyle notification still rings via the dedicated
     * RINGTONE channel — the F4 fallback.
     */
    fun buildIncomingCallNotification(
        context: Context,
        channelId: String,
        callerName: CharSequence,
        isVideo: Boolean,
        smallIcon: Int,
        fullScreenIntent: PendingIntent?,
        answerIntent: PendingIntent?,
        declineIntent: PendingIntent?,
        useFullScreenIntent: Boolean,
    ): Notification {
        // Platform-owned call-surface labels (NTF-12, doc-06): resolved from
        // res/values[-fr|-ar] so they follow the device locale. Base (English)
        // values match the previous literals. The doc-08 lifecycle strings
        // (foreground/stop/timeout/recovery, below) are intentionally left
        // hardcoded — they belong to doc-08 (BGS-19).
        val answerText = context.getString(
            if (isVideo) R.string.sila_notif_call_answer_video else R.string.sila_notif_call_answer,
        )
        // Notification.CallStyle renders its own answer/decline affordances,
        // so it requires non-null answer/decline intents. When either is
        // missing (or below API 34) fall through to the heads-up fallback.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE &&
            answerIntent != null && declineIntent != null
        ) {
            val caller = Person.Builder()
                .setName(callerName)
                .setImportant(true)
                .build()
            val style = Notification.CallStyle.forIncomingCall(
                caller,
                declineIntent,
                answerIntent,
            )
            val builder = Notification.Builder(context, channelId)
                .setSmallIcon(smallIcon)
                .setStyle(style)
            if (useFullScreenIntent && fullScreenIntent != null) {
                builder.setFullScreenIntent(fullScreenIntent, true)
            }
            return builder.build()
        }

        val builder = NotificationCompat.Builder(context, channelId)
            .setContentTitle(callerName)
            .setContentText(
                context.getString(
                    if (isVideo) R.string.sila_notif_call_incoming_video
                    else R.string.sila_notif_call_incoming_voice,
                ),
            )
            .setSmallIcon(smallIcon)
            .setPriority(NotificationCompat.PRIORITY_MAX)
            .setCategory(NotificationCompat.CATEGORY_CALL)
            .setOngoing(true)
        if (answerIntent != null) {
            builder.addAction(0, answerText, answerIntent)
        }
        if (declineIntent != null) {
            builder.addAction(0, context.getString(R.string.sila_notif_call_decline), declineIntent)
        }
        if (useFullScreenIntent && fullScreenIntent != null) {
            builder.setFullScreenIntent(fullScreenIntent, true)
        }
        return builder.build()
    }

    fun buildForegroundNotification(
        context: Context,
        channelId: String,
        title: CharSequence,
        text: String,
        smallIcon: Int,
        pendingIntent: PendingIntent?,
        showStopAction: Boolean,
        stopActionIntent: Intent?,
    ): Notification {
        val builder = NotificationCompat.Builder(context, channelId)
            .setContentTitle(title)
            .setContentText(text)
            .setSmallIcon(smallIcon)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .apply { pendingIntent?.let { setContentIntent(it) } }

        if (showStopAction && stopActionIntent != null) {
            val stopPendingIntent = PendingIntent.getService(
                context, 0, stopActionIntent,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            )
            builder.addAction(0, "Stop", stopPendingIntent)
        }

        return builder.build()
    }

    fun buildTimeoutNotification(
        context: Context,
        channelId: String,
        title: CharSequence,
        text: String,
        smallIcon: Int,
        pendingIntent: PendingIntent?,
    ): Notification {
        return NotificationCompat.Builder(context, channelId)
            .setContentTitle(title)
            .setContentText(text)
            .setSmallIcon(smallIcon)
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .apply { pendingIntent?.let { setContentIntent(it) } }
            .build()
    }

    fun buildRecoveryNotification(
        context: Context,
        channelId: String,
        title: CharSequence,
        text: String,
        smallIcon: Int,
        pendingIntent: PendingIntent?,
    ): Notification {
        return NotificationCompat.Builder(context, channelId)
            .setContentTitle(title)
            .setContentText(text)
            .setSmallIcon(smallIcon)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .apply { pendingIntent?.let { setContentIntent(it) } }
            .build()
    }
}
