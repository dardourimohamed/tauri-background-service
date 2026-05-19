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
        const val TIMEOUT_NOTIFICATION_ID = 9003
        const val TIMEOUT_CHANNEL_ID = "bg_service_timeout"
        const val EXTRA_LABEL  = "label"
        const val EXTRA_SERVICE_TYPE = "foregroundServiceType"
        const val ACTION_START = "START"
        const val ACTION_STOP  = "STOP"
        internal const val RESTART_TIMEOUT_MS = 30_000L

        @Volatile var isRunning = false
        @Volatile var autoRestarting = false

        fun buildStartState(label: String, serviceType: String, previous: DurableState): DurableState {
            return previous.copy(
                desiredRunning = true,
                lastServiceLabel = label,
                lastServiceType = serviceType,
                lastStartEpochMs = System.currentTimeMillis(),
                lastNativeState = "running",
            )
        }

        fun buildStopState(previous: DurableState): DurableState {
            return previous.copy(
                desiredRunning = false,
                recoveryPending = false,
                recoveryReason = null,
            )
        }

        fun buildTimeoutState(previous: DurableState, serviceType: String): DurableState {
            return previous.copy(
                lastNativeState = "timeout",
                lastPlatformError = "FGS timeout (type: $serviceType)",
            )
        }
    }

    private val restartTimeoutHandler = Handler(Looper.getMainLooper())
    private var restartTimeoutRunnable: Runnable? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // ACTION_STOP: clear prefs and stop
        if (intent?.action == ACTION_STOP) {
            // Notify Rust actor that the user pressed stop on the notification.
            // The callback emits a JS event that the TypeScript layer forwards
            // to the Rust native_lifecycle_event command.
            BackgroundServicePlugin.onNativeLifecycleEvent?.invoke(
                "androidNotificationStop", null
            )
            getSharedPreferences("bg_service", Context.MODE_PRIVATE).edit()
                .remove("bg_service_label")
                .remove("bg_service_type")
                .remove("bg_auto_start_pending")
                .remove("bg_auto_start_label")
                .remove("bg_auto_start_type")
                .remove("bg_notif_channel_id")
                .remove("bg_notif_channel_name")
                .remove("bg_notif_id")
                .remove("bg_notif_small_icon")
                .remove("bg_show_stop_action")
                .remove("bg_on_timeout_policy")
                .apply()
            // Persist DurableState: desiredRunning=false
            DurableState.save(this, buildStopState(DurableState.load(this)))
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

        // Cancel any recovery notification from handleOsRestart or BootReceiver
        cancelRecoveryNotification()
        // Cancel any timeout notification from previous handleTimeout
        cancelTimeoutNotification()
        val label = intent.getStringExtra(EXTRA_LABEL) ?: "Service running"
        val serviceType = intent.getStringExtra(EXTRA_SERVICE_TYPE) ?: "dataSync"
        createChannel()
        if (!startForegroundTyped(notifId(), buildNotification(label), mapServiceType(serviceType))) {
            isRunning = false
            return START_NOT_STICKY
        }
        isRunning = true

        // Persist config for OS restart detection
        getSharedPreferences("bg_service", Context.MODE_PRIVATE).edit()
            .putString("bg_service_label", label)
            .putString("bg_service_type", serviceType)
            .apply()

        // Persist DurableState
        DurableState.save(this, buildStartState(label, serviceType, DurableState.load(this)))

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
        handleTimeout(fgsType)
    }

    @Suppress("UNUSED_PARAMETER")
    internal fun handleTimeout(fgsType: Int) {
        val previous = DurableState.load(this)
        val serviceType = previous.lastServiceType.ifEmpty { "dataSync" }
        val label = previous.lastServiceLabel.ifEmpty { "Service" }

        // Notify Rust actor about the timeout before applying policy.
        // The callback emits a JS event that the TypeScript layer forwards
        // to the Rust native_lifecycle_event command.
        BackgroundServicePlugin.onNativeLifecycleEvent?.invoke(
            "androidTimeout", serviceType
        )

        // Persist timeout state
        DurableState.save(this, buildTimeoutState(previous, serviceType))

        // Apply timeout policy
        when (timeoutPolicy()) {
            "stop" -> { /* just stop below */ }
            "notifyUser" -> postTimeoutNotification(label)
            "scheduleRecovery" -> {
                DurableState.save(this, DurableState.load(this).copy(
                    recoveryPending = true,
                    recoveryReason = "timeout",
                ))
                BootReceiver.postRecoveryNotification(this, label)
            }
        }

        // Emit timeout event to JS layer via BackgroundServicePlugin
        BackgroundServicePlugin.onTimeoutEvent?.invoke(
            "FGS timeout (type: $serviceType)"
        )

        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
        isRunning = false
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

        // Persist recovery state
        val previous = DurableState.load(this)
        DurableState.save(this, previous.copy(
            recoveryPending = true,
            recoveryReason = "os_restart",
        ))

        // Must call startForeground immediately (Android 12+ requirement)
        createChannel()
        if (!startForegroundTyped(notifId(), buildNotification("Restarting..."), mapServiceType(serviceType))) {
            return START_NOT_STICKY
        }
        isRunning = true
        autoRestarting = true

        // Self-stop timeout: if the plugin doesn't consume the auto-start within
        // 30 seconds (e.g. app has no launcher Activity), stop the service to
        // prevent an orphaned foreground notification.
        restartTimeoutRunnable = Runnable { stopSelf() }
        restartTimeoutHandler.postDelayed(restartTimeoutRunnable!!, RESTART_TIMEOUT_MS)

        // Post recovery notification instead of launching activity directly.
        // startActivity() from background service context is blocked on Android 10+.
        BootReceiver.postRecoveryNotification(this, label)

        return START_STICKY
    }

    private fun startForegroundTyped(notifId: Int, notification: Notification, serviceType: Int): Boolean {
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                startForeground(notifId, notification, serviceType)
            } else {
                startForeground(notifId, notification)
            }
            return true
        } catch (e: android.app.ForegroundServiceStartNotAllowedException) {
            persistStartForegroundError("fgs_restricted",
                "Foreground service start not allowed by OS: ${e.message}")
        } catch (e: SecurityException) {
            persistStartForegroundError("missing_permission",
                "Missing foreground service permission: ${e.message}")
        } catch (e: Exception) {
            persistStartForegroundError("start_failed",
                "Failed to start foreground service: ${e.message}")
        }
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
        return false
    }

    private fun persistStartForegroundError(code: String, message: String) {
        val previous = DurableState.load(this)
        DurableState.save(this, previous.copy(
            lastPlatformError = "$code: $message"
        ))
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

        val builder = NotificationCompat.Builder(this, notifChannelId())
            .setContentTitle(applicationInfo.loadLabel(packageManager).toString())
            .setContentText(label)
            .setSmallIcon(notifSmallIcon())
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .apply { pi?.let { setContentIntent(it) } }

        if (notifShowStopAction()) {
            val stopIntent = Intent(this, LifecycleService::class.java).apply {
                action = ACTION_STOP
            }
            val stopPendingIntent = PendingIntent.getService(
                this, 0, stopIntent,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            )
            builder.addAction(0, "Stop", stopPendingIntent)
        }

        return builder.build()
    }

    private fun createChannel() {
        getSystemService(NotificationManager::class.java)
            .createNotificationChannel(
                NotificationChannel(notifChannelId(), notifChannelName(),
                    NotificationManager.IMPORTANCE_LOW)
                    .apply { setShowBadge(false) }
            )
    }

    private fun notifPrefs() = getSharedPreferences("bg_service", Context.MODE_PRIVATE)

    private fun notifChannelId(): String =
        notifPrefs().getString("bg_notif_channel_id", CHANNEL_ID) ?: CHANNEL_ID

    private fun notifChannelName(): String =
        notifPrefs().getString("bg_notif_channel_name", "Service Status") ?: "Service Status"

    private fun notifId(): Int =
        notifPrefs().getInt("bg_notif_id", NOTIF_ID)

    private fun notifSmallIcon(): Int {
        val iconName = notifPrefs().getString("bg_notif_small_icon", null)
        if (iconName != null) {
            val resId = resources.getIdentifier(iconName, "drawable", packageName)
            if (resId != 0) return resId
        }
        return android.R.drawable.stat_notify_sync
    }

    private fun notifShowStopAction(): Boolean =
        notifPrefs().getBoolean("bg_show_stop_action", true)

    private fun cancelRecoveryNotification() {
        getSystemService(NotificationManager::class.java)
            .cancel(BootReceiver.RECOVERY_NOTIFICATION_ID)
    }

    private fun cancelTimeoutNotification() {
        getSystemService(NotificationManager::class.java)
            .cancel(TIMEOUT_NOTIFICATION_ID)
    }

    private fun timeoutPolicy(): String =
        notifPrefs().getString("bg_on_timeout_policy", "notifyUser") ?: "notifyUser"

    private fun postTimeoutNotification(label: String) {
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

        val channel = NotificationChannel(
            TIMEOUT_CHANNEL_ID,
            "Service Timeout",
            NotificationManager.IMPORTANCE_HIGH,
        ).apply {
            description = "Notifications when background service times out"
            setShowBadge(true)
        }
        nm.createNotificationChannel(channel)

        val pendingIntent = packageManager.getLaunchIntentForPackage(packageName)
            ?.let {
                PendingIntent.getActivity(
                    this, 0, it,
                    PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
                )
            }

        val notification = NotificationCompat.Builder(this, TIMEOUT_CHANNEL_ID)
            .setContentTitle(applicationInfo.loadLabel(packageManager))
            .setContentText("Background service timed out: $label")
            .setSmallIcon(notifSmallIcon())
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .apply { pendingIntent?.let { setContentIntent(it) } }
            .build()

        nm.notify(TIMEOUT_NOTIFICATION_ID, notification)
    }
}
