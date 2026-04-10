package app.tauri.backgroundservice

import android.app.*
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import androidx.annotation.RequiresApi
import androidx.core.app.NotificationCompat

class LifecycleService : Service() {

    companion object {
        const val CHANNEL_ID   = "bg_keepalive"
        const val NOTIF_ID     = 9001
        const val EXTRA_LABEL  = "label"
        const val EXTRA_SERVICE_TYPE = "foregroundServiceType"
        const val ACTION_START = "START"
        const val ACTION_STOP  = "STOP"
        internal const val RESTART_TIMEOUT_MS = 30_000L

        @Volatile var isRunning = false
        @Volatile var autoRestarting = false
    }

    private val restartTimeoutHandler = Handler(Looper.getMainLooper())
    private var restartTimeoutRunnable: Runnable? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // ACTION_STOP: clear prefs and stop
        if (intent?.action == ACTION_STOP) {
            getSharedPreferences("bg_service", Context.MODE_PRIVATE).edit()
                .remove("bg_service_label")
                .remove("bg_service_type")
                .remove("bg_auto_start_pending")
                .remove("bg_auto_start_label")
                .remove("bg_auto_start_type")
                .apply()
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }

        // OS restart: null intent or null action means Android restarted the service
        if (intent == null || intent.action == null) {
            return handleOsRestart()
        }

        // Normal start
        // Cancel any pending restart timeout — the plugin has consumed the auto-start.
        restartTimeoutRunnable?.let {
            restartTimeoutHandler.removeCallbacks(it)
            restartTimeoutRunnable = null
        }
        val label = intent.getStringExtra(EXTRA_LABEL) ?: "Service running"
        val serviceType = intent.getStringExtra(EXTRA_SERVICE_TYPE) ?: "dataSync"
        createChannel()
        startForegroundTyped(NOTIF_ID, buildNotification(label), mapServiceType(serviceType))
        isRunning = true

        return START_STICKY
    }

    override fun onDestroy() {
        restartTimeoutRunnable?.let {
            restartTimeoutHandler.removeCallbacks(it)
            restartTimeoutRunnable = null
        }
        isRunning = false
        autoRestarting = false
        super.onDestroy()
    }

    @RequiresApi(Build.VERSION_CODES.VANILLA_ICE_CREAM)
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
        val serviceType = prefs.getString("bg_service_type", "dataSync")!!
        prefs.edit()
            .putBoolean("bg_auto_start_pending", true)
            .putString("bg_auto_start_label", label)
            .putString("bg_auto_start_type", serviceType)
            .apply()

        // Must call startForeground immediately (Android 12+ requirement)
        createChannel()
        startForegroundTyped(NOTIF_ID, buildNotification("Restarting..."), mapServiceType(serviceType))
        isRunning = true
        autoRestarting = true

        // Self-stop timeout: if the plugin doesn't consume the auto-start within
        // 30 seconds (e.g. app has no launcher Activity), stop the service to
        // prevent an orphaned foreground notification.
        restartTimeoutRunnable = Runnable { stopSelf() }
        restartTimeoutHandler.postDelayed(restartTimeoutRunnable!!, RESTART_TIMEOUT_MS)

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

    private fun mapServiceType(type: String): Int {
        return when (type) {
            "dataSync" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
            "mediaPlayback" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
            "phoneCall" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_PHONE_CALL
            "location" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION
            "connectedDevice" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
            "mediaProjection" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
            "camera" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA
            "microphone" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            "health" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_HEALTH
            "remoteMessaging" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING
            "systemExempted" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_SYSTEM_EXEMPTED
            "shortService" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_SHORT_SERVICE
            "specialUse" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
            "mediaProcessing" -> ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROCESSING
            else -> throw IllegalArgumentException("Invalid foreground_service_type: $type")
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
