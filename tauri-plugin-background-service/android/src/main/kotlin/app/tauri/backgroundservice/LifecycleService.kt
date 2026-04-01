package app.tauri.backgroundservice

import android.app.*
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat

class LifecycleService : Service() {

    companion object {
        const val CHANNEL_ID   = "bg_keepalive"
        const val NOTIF_ID     = 9001
        const val EXTRA_LABEL  = "label"
        const val ACTION_START = "START"
        const val ACTION_STOP  = "STOP"

        @Volatile var isRunning = false
        @Volatile var autoRestarting = false
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // ACTION_STOP: clear prefs and stop
        if (intent?.action == ACTION_STOP) {
            getSharedPreferences("bg_service", Context.MODE_PRIVATE).edit()
                .remove("bg_service_label")
                .remove("bg_auto_start_pending")
                .remove("bg_auto_start_label")
                .apply()
            stopSelf()
            return START_NOT_STICKY
        }

        // OS restart: null intent or null action means Android restarted the service
        if (intent == null || intent.action == null) {
            return handleOsRestart()
        }

        // Normal start
        val label = intent.getStringExtra(EXTRA_LABEL) ?: "Service running"
        createChannel()
        startForegroundTyped(NOTIF_ID, buildNotification(label), ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        isRunning = true

        return START_STICKY
    }

    override fun onDestroy() {
        isRunning = false
        autoRestarting = false
        super.onDestroy()
    }

    override fun onTimeout(startId: Int, fgsType: Int) {
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    override fun onBind(i: Intent?) = null

    private fun handleOsRestart(): Int {
        val prefs = getSharedPreferences("bg_service", Context.MODE_PRIVATE)
        val label = prefs.getString("bg_service_label", null)

        if (label == null) {
            // Service was never started or was stopped cleanly
            stopSelf()
            return START_NOT_STICKY
        }

        // Set auto-start flag for plugin to detect when Activity launches
        prefs.edit()
            .putBoolean("bg_auto_start_pending", true)
            .putString("bg_auto_start_label", label)
            .apply()

        // Must call startForeground immediately (Android 12+ requirement)
        createChannel()
        startForegroundTyped(NOTIF_ID, buildNotification("Restarting..."), ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        autoRestarting = true

        // Launch Activity to reinitialize Tauri runtime
        packageManager.getLaunchIntentForPackage(packageName)?.let { launchIntent ->
            launchIntent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
            startActivity(launchIntent)
        }

        return START_STICKY
    }

    private fun startForegroundTyped(notifId: Int, notification: Notification, serviceType: Int) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(notifId, notification, serviceType)
        } else {
            startForeground(notifId, notification)
        }
    }

    private fun buildNotification(label: String): Notification {
        val pi = packageManager.getLaunchIntentForPackage(packageName)
            ?.let { PendingIntent.getActivity(this, 0, it,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT) }

        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(applicationInfo.loadLabel(packageManager).toString())
            .setContentText(label)
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .apply { pi?.let { setContentIntent(it) } }
            .build()
    }

    private fun createChannel() {
        getSystemService(NotificationManager::class.java)
            .createNotificationChannel(
                NotificationChannel(CHANNEL_ID, "Service Status",
                    NotificationManager.IMPORTANCE_LOW)
                    .apply { setShowBadge(false) }
            )
    }
}
