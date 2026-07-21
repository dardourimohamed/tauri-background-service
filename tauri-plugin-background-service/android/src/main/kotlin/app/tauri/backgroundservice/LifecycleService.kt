package app.tauri.backgroundservice

import android.app.*
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import androidx.annotation.RequiresApi
import androidx.annotation.VisibleForTesting

class LifecycleService : Service() {

    companion object {
        const val CHANNEL_ID   = "bg_keepalive"
        const val NOTIF_ID     = 9001
        const val TIMEOUT_NOTIFICATION_ID = 9003
        const val TIMEOUT_CHANNEL_ID = "bg_service_timeout"
        const val EXTRA_LABEL  = "label"
        const val EXTRA_SERVICE_TYPE = "foregroundServiceType"
        const val EXTRA_START_ACK_ID = "startAckId"
        const val EXTRA_START_REASON = "startReason"
        const val ACTION_START = "START"
        const val ACTION_STOP  = "STOP"
        // spec 08 C6 (Step 15): swap the foreground service type of an
        // already-running service (e.g. remoteMessaging → phoneCall on call
        // answer, phoneCall → remoteMessaging on end) WITHOUT re-running the
        // core start. Android permits re-calling startForeground with an
        // updated type on the same running service.
        const val ACTION_UPDATE_TYPE = "UPDATE_TYPE"
        internal const val RESTART_TIMEOUT_MS = 30_000L

        @Volatile var isRunning = false
        @Volatile var isForeground = false

        @VisibleForTesting
        internal var bridgeProvider: () -> CoreBridge = { HeadlessBridgeImpl() }

        // Core start must run off the main looper in production (ANR / start-ACK
        // deadlock when the caller blocks the main thread); tests inject an
        // inline executor so assertions after onStartCommand are deterministic.
        internal val DEFAULT_CORE_START_EXECUTOR: (String, () -> Unit) -> Unit = { name, task ->
            Thread({ task() }, name).start()
        }

        @VisibleForTesting
        internal var coreStartExecutor: (String, () -> Unit) -> Unit = DEFAULT_CORE_START_EXECUTOR

        // Core stop must run off the main looper too (BGS-20, doc-08 Step 11):
        // ACTION_STOP → bridge.stop → HeadlessBridge.stop → lib.rs block_on(
        // stop_headless_core) does a storage flush + network teardown that ANRs
        // if it runs inline on the main thread while the user taps Stop from the
        // notification. Mirrors the start path's coreStartExecutor discipline;
        // tests inject an inline executor for determinism except the off-main
        // test, which installs a thread-distinguishing executor.
        internal val DEFAULT_CORE_STOP_EXECUTOR: (String, () -> Unit) -> Unit = { name, task ->
            Thread({ task() }, name).start()
        }

        @VisibleForTesting
        internal var coreStopExecutor: (String, () -> Unit) -> Unit = DEFAULT_CORE_STOP_EXECUTOR

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
    private val bridge: CoreBridge = bridgeProvider()

    @VisibleForTesting
    @Volatile internal var connectivityMonitor: ConnectivityMonitor? = null

    // Registered from the core-start worker threads, unregistered from the
    // main thread (onDestroy) — synchronized so a destroy racing a late core
    // start cannot leak a registered callback.
    @Synchronized
    private fun registerConnectivityMonitor() {
        if (connectivityMonitor != null) return
        val monitor = ConnectivityMonitor(this, onNetworkChanged = {
            try {
                // peersFlushed counts recipients nudged/attempted, not
                // messages delivered — keep log wording in step.
                val result = bridge.notifyNetworkChanged()
                Log.i("LifecycleService", "network change nudge result: ${result.rawJson}")
            } catch (e: UnsatisfiedLinkError) {
                // Updated APK over an old native lib: the export is missing.
                // Log and keep the service running (additive-JNI compat).
                Log.w("LifecycleService", "notifyNetworkChanged native missing: ${e.message}")
            }
        })
        monitor.register()
        connectivityMonitor = monitor
    }

    @Synchronized
    private fun unregisterConnectivityMonitor() {
        connectivityMonitor?.unregister()
        connectivityMonitor = null
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        Log.i("LifecycleService", "onStartCommand: action=${intent?.action}, startId=$startId")
        // ACTION_STOP: clear prefs and stop
        if (intent?.action == ACTION_STOP) {
            // Persist DurableState BEFORE any JS/Rust forwarding so the state
            // survives webview absence or callback failures.
            DurableState.save(this, buildStopState(DurableState.load(this)))
            // Run the Rust core stop OFF the main thread. The JNI hop
            // (bridge.stop → HeadlessBridge.stop → lib.rs block_on(
            // stop_headless_core)) does a storage flush + network teardown that
            // ANRs if it runs inline on the main looper while the user taps
            // Stop from the notification (BGS-20, doc-08 Step 11). Fire-and-
            // forget onto the worker, mirroring the start path's
            // coreStartExecutor discipline; the cheap main-thread teardown
            // below (event emit, prefs edit, stopForeground, stopSelf) runs
            // immediately and does not block onStartCommand's return on the stop.
            coreStopExecutor("bg-core-stop") {
                bridge.stop(this, "android_service_stop")
            }
            // Notify Rust actor that the user pressed stop on the notification.
            // The callback emits a JS event that the TypeScript layer forwards
            // to the Rust native_lifecycle_event command.
            BackgroundServicePlugin.emitNativeLifecycleEvent(
                "androidNotificationStop", null
            )
            getSharedPreferences("bg_service", Context.MODE_PRIVATE).edit()
                .remove("bg_service_label")
                .remove("bg_service_type")
                .remove("bg_notif_channel_id")
                .remove("bg_notif_channel_name")
                .remove("bg_notif_id")
                .remove("bg_notif_small_icon")
                .remove("bg_show_stop_action")
                .remove("bg_on_timeout_policy")
                .apply()
            stopForeground(STOP_FOREGROUND_REMOVE)
            isForeground = false
            stopSelf()
            return START_NOT_STICKY
        }

        // ACTION_UPDATE_TYPE (spec 08 C6, Step 15): swap the FGS type of the
        // running service without tearing down / re-starting the headless
        // core. Used by the call lifecycle to upgrade remoteMessaging →
        // phoneCall on answer and revert on end.
        if (intent?.action == ACTION_UPDATE_TYPE) {
            return handleUpdateType(intent)
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
        val startAckId = intent.getStringExtra(EXTRA_START_ACK_ID)
        val startReason = intent.getStringExtra(EXTRA_START_REASON) ?: "android_service_start"
        Log.i("LifecycleService", "Normal start: label=$label, serviceType=$serviceType, startReason=$startReason")
        // Promote to foreground FIRST (synchronous), before any core/native init.
        // The core start is scheduled on a worker thread below so onStartCommand
        // reaches startForeground() inside the OS deadline (spec-compliance W1).
        createChannel()
        if (!startForegroundTyped(notifId(), buildNotification(label), mapServiceType(serviceType))) {
            isRunning = false
            isForeground = false
            ServiceStartAckRegistry.complete(
                startAckId,
                false,
                ServiceStartAckRegistry.errorJson(
                    "foreground_start_failed",
                    DurableState.load(this).lastPlatformError
                        ?: "Failed to promote service to foreground",
                ),
            )
            return START_NOT_STICKY
        }

        // Foreground promotion succeeded — the service is foreground now.
        isForeground = true

        // Run the Rust core start OFF the main thread. The JNI call builds the
        // Core (storage open + P2P startup) and can take seconds; running it on
        // the main thread risks ANR and deadlocks the plugin's start ACK when
        // the caller is itself blocking the main thread (e.g. Tauri setup).
        // The ACK registry supports asynchronous completion.
        Log.i("LifecycleService", "Scheduling bridge.start(reason=$startReason) on worker thread")
        coreStartExecutor("bg-core-start") {
            val coreResult = bridge.start(this, startReason)
            Log.i("LifecycleService", "bridge.start result: accepted=${coreResult.accepted}, json=${coreResult.rawJson}")
            if (!coreResult.accepted) {
                isRunning = false
                isForeground = false
                unregisterConnectivityMonitor()
                DurableState.save(this, DurableState.load(this).copy(
                    lastPlatformError = coreResult.rawJson,
                    lastNativeState = "core_start_failed",
                ))
                ServiceStartAckRegistry.complete(startAckId, false, coreResult.rawJson)
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
            } else {
                isRunning = true
                registerConnectivityMonitor()

                // Persist config for OS restart detection
                getSharedPreferences("bg_service", Context.MODE_PRIVATE).edit()
                    .putString("bg_service_label", label)
                    .putString("bg_service_type", serviceType)
                    .apply()

                // Persist DurableState. AND-07: a successful explicit start
                // (manual or desired boot/package-replace recovery) resets the
                // OS-restart attempt counter — the service is in a clean state.
                DurableState.save(this, buildStartState(label, serviceType, DurableState.load(this)).copy(
                    restartAttempt = 0,
                ))
                ServiceStartAckRegistry.complete(startAckId, true, coreResult.rawJson)

                // Boot-recovery start ACCEPTED (BootReceiver routes through the
                // normal-start path) — tell the Rust actor so it can fire the
                // policy-gated bg-recovery notification (suppressed on Android
                // per DEC-002; the emit still flows for observability).
                if (startReason == "boot_completed" || startReason == "package_replaced") {
                    BackgroundServicePlugin.emitNativeLifecycleEvent(
                        "androidBootRecoveryAccepted", null
                    )
                }
            }
        }

        return START_STICKY
    }

    override fun onDestroy() {
        unregisterConnectivityMonitor()
        restartTimeoutRunnable?.let {
            restartTimeoutHandler.removeCallbacks(it)
            restartTimeoutRunnable = null
        }
        isRunning = false
        isForeground = false
        super.onDestroy()
    }

    @RequiresApi(Build.VERSION_CODES.VANILLA_ICE_CREAM)
    override fun onTimeout(startId: Int, fgsType: Int) {
        handleTimeout(fgsType)
    }

    @Suppress("UNUSED_PARAMETER")
    internal fun handleTimeout(fgsType: Int) {
        val previous = DurableState.load(this)
        val serviceType = previous.lastServiceType.ifEmpty { "remoteMessaging" }
        val label = previous.lastServiceLabel.ifEmpty { "Service" }

        // Persist timeout state BEFORE any JS/Rust forwarding so the state
        // survives webview absence or callback failures.
        DurableState.save(this, buildTimeoutState(previous, serviceType))

        // Notify Rust actor about the timeout.
        // The callback emits a JS event that the TypeScript layer forwards
        // to the Rust native_lifecycle_event command.
        BackgroundServicePlugin.emitNativeLifecycleEvent(
            "androidTimeout", serviceType
        )

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
        BackgroundServicePlugin.emitTimeoutEvent(
            "FGS timeout (type: $serviceType)"
        )

        // AND-06: dispatch the off-main core stop (host flush + network
        // teardown) with reason `android_timeout` BEFORE stopForeground/stopSelf.
        // Previously handleTimeout tore down the FGS/process without bridge.stop,
        // skipping the host's graceful shutdown. Mirrors the ACTION_STOP path's
        // coreStopExecutor discipline (BGS-20): fire-and-forget onto the worker;
        // the cheap main-thread teardown below runs immediately and does not
        // block on the JNI hop.
        coreStopExecutor("bg-core-stop") {
            bridge.stop(this, "android_timeout")
        }

        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
        isRunning = false
        isForeground = false
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

        val serviceType = prefs.getString("bg_service_type", "dataSync") ?: "dataSync"

        // Persist recovery state. AND-07: an OS (START_STICKY) restart is an
        // involuntary restart attempt — increment the counter so callers can
        // observe backoff pressure; it is reset to 0 on a successful explicit
        // start (see the onStartCommand normal-start path).
        val previous = DurableState.load(this)
        DurableState.save(this, previous.copy(
            restartAttempt = previous.restartAttempt + 1,
            recoveryPending = true,
            recoveryReason = "os_restart",
        ))

        // Must call startForeground immediately (Android 12+ requirement)
        createChannel()
        val locale = LocaleStore.load(this)
        if (!startForegroundTyped(notifId(), buildNotification(NotificationStrings.lookup("restarting", locale)), mapServiceType(serviceType))) {
            return START_NOT_STICKY
        }

        isForeground = true

        // Core start off the main thread (see onStartCommand normal-start path).
        coreStartExecutor("bg-core-restart") {
            val coreResult = bridge.start(this, "sticky_restart")
            if (!coreResult.accepted) {
                isForeground = false
                unregisterConnectivityMonitor()
                DurableState.save(this, DurableState.load(this).copy(
                    recoveryPending = true,
                    recoveryReason = "core_start_failed",
                    lastNativeState = "core_start_failed",
                    lastPlatformError = coreResult.rawJson,
                ))
                BootReceiver.postRecoveryNotification(this, label)
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
            } else {
                isRunning = true
                registerConnectivityMonitor()

                DurableState.save(this, buildStartState(label, serviceType, DurableState.load(this)).copy(
                    recoveryPending = false,
                    recoveryReason = null,
                ))

                // Recovery start ACCEPTED — tell the Rust actor so it can fire
                // the policy-gated bg-recovery notification (suppressed on
                // Android per DEC-002; the emit still flows for observability).
                BackgroundServicePlugin.emitNativeLifecycleEvent(
                    "androidOsRestartAccepted", null
                )
            }
        }

        return START_STICKY
    }

    /**
     * Handle ACTION_UPDATE_TYPE (spec 08 C6, Step 15): re-promote the already-
     * running foreground service with a new [EXTRA_SERVICE_TYPE], without
     * re-running `bridge.start` (the headless core keeps running). The type
     * swap is persisted so an OS restart rehydrates the latest type.
     */
    private fun handleUpdateType(intent: Intent): Int {
        val newType = intent.getStringExtra(EXTRA_SERVICE_TYPE)
        if (newType.isNullOrEmpty()) {
            Log.w("LifecycleService", "UPDATE_TYPE missing service type extra")
            return START_NOT_STICKY
        }
        if (!isForeground) {
            Log.i("LifecycleService", "UPDATE_TYPE ignored: service not foreground")
            return START_NOT_STICKY
        }
        val mappedType = try {
            mapServiceType(newType)
        } catch (e: IllegalArgumentException) {
            persistStartForegroundError("invalid_type_update", e.message ?: newType)
            Log.w("LifecycleService", "UPDATE_TYPE rejected: ${e.message}")
            return START_NOT_STICKY
        }
        val label = notifPrefs().getString("bg_service_label", null)
            ?: DurableState.load(this).lastServiceLabel.ifEmpty { "Service" }
        createChannel()
        if (!startForegroundTyped(notifId(), buildNotification(label), mappedType)) {
            return START_NOT_STICKY
        }
        // Persist the swapped type for OS-restart rehydration + observability.
        notifPrefs().edit().putString("bg_service_type", newType).apply()
        DurableState.save(
            this,
            DurableState.load(this).copy(lastServiceType = newType),
        )
        Log.i("LifecycleService", "UPDATE_TYPE: foreground service type → $newType")
        return START_NOT_STICKY
    }

    private fun startForegroundTyped(notifId: Int, notification: Notification, serviceType: Int): Boolean {        try {
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
        // Surface to JS so the UI renders the failure instead of the service
        // self-stopping silently (spec-compliance W1 / R-W1.3, NFR-1). DurableState
        // is committed synchronously above, so the pushed error and a later
        // status poll agree.
        BackgroundServicePlugin.emitPlatformErrorEvent("$code: $message")
    }

    // AND-01: the type→bit mapping now lives in the single source
    // [ForegroundServiceTypes] shared with the plugin's merged-manifest
    // preflight, so the dispatch and the preflight cannot drift.
    private fun mapServiceType(type: String): Int = ForegroundServiceTypes.bitFor(type)

    private fun buildNotification(label: String): Notification {
        val pi = packageManager.getLaunchIntentForPackage(packageName)
            ?.let { PendingIntent.getActivity(this, 0, it,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT) }

        val stopActionIntent = if (notifShowStopAction()) {
            Intent(this, LifecycleService::class.java).apply { action = ACTION_STOP }
        } else null

        return NotificationHelper.buildForegroundNotification(
            context = this,
            channelId = notifChannelId(),
            title = applicationInfo.loadLabel(packageManager).toString(),
            text = label,
            smallIcon = notifSmallIcon(),
            pendingIntent = pi,
            showStopAction = notifShowStopAction(),
            stopActionIntent = stopActionIntent,
        )
    }

    private fun createChannel() {
        NotificationHelper.ensureChannel(
            this, notifChannelId(), notifChannelName(), NotificationManager.IMPORTANCE_LOW
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
        return NotificationIconResolver.resolve(this, iconName)
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
        val locale = LocaleStore.load(this)
        NotificationHelper.ensureChannel(
            this, TIMEOUT_CHANNEL_ID,
            NotificationStrings.lookup("channel_timeout", locale),
            NotificationManager.IMPORTANCE_HIGH,
            description = NotificationStrings.lookup("channel_timeout_desc", locale),
            showBadge = true,
        )

        val pendingIntent = packageManager.getLaunchIntentForPackage(packageName)
            ?.let {
                PendingIntent.getActivity(
                    this, 0, it,
                    PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
                )
            }

        val notification = NotificationHelper.buildTimeoutNotification(
            context = this,
            channelId = TIMEOUT_CHANNEL_ID,
            title = applicationInfo.loadLabel(packageManager),
            text = NotificationStrings.lookup("service_timed_out", locale).replace("{label}", label),
            smallIcon = notifSmallIcon(),
            pendingIntent = pendingIntent,
        )

        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.notify(TIMEOUT_NOTIFICATION_ID, notification)
    }
}
