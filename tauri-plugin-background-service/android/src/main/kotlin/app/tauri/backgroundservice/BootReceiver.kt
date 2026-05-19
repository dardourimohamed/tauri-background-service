package app.tauri.backgroundservice

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.core.app.NotificationCompat

class BootReceiver : BroadcastReceiver() {

    companion object {
        const val RECOVERY_CHANNEL_ID = "bg_service_recovery"
        const val RECOVERY_NOTIFICATION_ID = 9002

        // FGS types blocked from BOOT_COMPLETED receiver on API 35+
        // See: https://developer.android.com/about/versions/15/behavior-changes-15
        private val BOOT_BLOCKED_TYPES_API35 = setOf(
            "dataSync",
            "camera",
            "mediaPlayback",
            "phoneCall",
            "mediaProjection",
            "microphone",
        )

        fun isBootBlockedType(serviceType: String, apiLevel: Int): Boolean {
            if (apiLevel < 35) return false
            return serviceType in BOOT_BLOCKED_TYPES_API35
        }

        fun postRecoveryNotification(context: Context, label: String) {
            val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

            val channel = NotificationChannel(
                RECOVERY_CHANNEL_ID,
                "Service Recovery",
                NotificationManager.IMPORTANCE_HIGH,
            ).apply {
                description = "Notifications to resume background service after reboot"
                setShowBadge(true)
            }
            nm.createNotificationChannel(channel)

            val pendingIntent = context.packageManager
                .getLaunchIntentForPackage(context.packageName)
                ?.let {
                    PendingIntent.getActivity(
                        context,
                        0,
                        it.apply {
                            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
                        },
                        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
                    )
                }

            val notification = NotificationCompat.Builder(context, RECOVERY_CHANNEL_ID)
                .setContentTitle(context.applicationInfo.loadLabel(context.packageManager))
                .setContentText("Tap to resume: $label")
                .setSmallIcon(android.R.drawable.stat_notify_sync)
                .setOngoing(true)
                .setPriority(NotificationCompat.PRIORITY_HIGH)
                .apply { pendingIntent?.let { setContentIntent(it) } }
                .build()

            nm.notify(RECOVERY_NOTIFICATION_ID, notification)
        }
    }

    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            Intent.ACTION_LOCKED_BOOT_COMPLETED -> {
                // Cannot read credential-encrypted SharedPreferences in direct-boot mode
                return
            }
            Intent.ACTION_BOOT_COMPLETED -> handleBootCompleted(context)
            Intent.ACTION_MY_PACKAGE_REPLACED -> handleMyPackageReplaced(context)
        }
    }

    private fun handleBootCompleted(context: Context) {
        val state = DurableState.load(context)
        if (!state.desiredRunning) return

        if (isBootBlockedType(state.lastServiceType, Build.VERSION.SDK_INT)) {
            DurableState.save(context, state.copy(
                recoveryPending = true,
                recoveryReason = "boot_fgs_type_restricted",
            ))
            postRecoveryNotification(context, state.lastServiceLabel)
            return
        }

        startRecoveryService(context, state.lastServiceLabel, state.lastServiceType)
    }

    private fun handleMyPackageReplaced(context: Context) {
        val state = DurableState.load(context)
        if (!state.desiredRunning) return

        // MY_PACKAGE_REPLACED is not subject to boot-time FGS type restrictions
        startRecoveryService(context, state.lastServiceLabel, state.lastServiceType)
    }

    private fun startRecoveryService(context: Context, label: String, serviceType: String) {
        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, label)
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, serviceType)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            context.startForegroundService(intent)
        } else {
            context.startService(intent)
        }
    }
}
