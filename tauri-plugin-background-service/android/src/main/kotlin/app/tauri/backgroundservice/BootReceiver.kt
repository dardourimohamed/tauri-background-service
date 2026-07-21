package app.tauri.backgroundservice

import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build

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
            // BGS-19 (doc-08 Step 16 T2): recovery channel name + description and
            // the resume body are localized from the Rust-persisted locale store
            // (default "en"). BootReceiver runs without the webview, so it reads
            // the store directly (mirrors the Rust headless composer).
            val locale = LocaleStore.load(context)
            NotificationHelper.ensureChannel(
                context, RECOVERY_CHANNEL_ID,
                NotificationStrings.lookup("channel_recovery", locale),
                NotificationManager.IMPORTANCE_HIGH,
                description = NotificationStrings.lookup("channel_recovery_desc", locale),
                showBadge = true,
            )

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

            val notification = NotificationHelper.buildRecoveryNotification(
                context = context,
                channelId = RECOVERY_CHANNEL_ID,
                title = context.applicationInfo.loadLabel(context.packageManager),
                text = NotificationStrings.lookup("tap_to_resume", locale).replace("{label}", label),
                smallIcon = NotificationIconResolver.resolve(context),
                pendingIntent = pendingIntent,
            )

            val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
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

        startRecoveryService(
            context,
            state.lastServiceLabel,
            state.lastServiceType,
            "boot_completed",
        )
    }

    private fun handleMyPackageReplaced(context: Context) {
        val state = DurableState.load(context)
        if (!state.desiredRunning) return

        // MY_PACKAGE_REPLACED is not subject to boot-time FGS type restrictions
        startRecoveryService(
            context,
            state.lastServiceLabel,
            state.lastServiceType,
            "package_replaced",
        )
    }

    private fun startRecoveryService(
        context: Context,
        label: String,
        serviceType: String,
        reason: String,
    ) {
        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, label)
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, serviceType)
            putExtra(LifecycleService.EXTRA_START_REASON, reason)
        }
        // BGS-30 (doc-08 Step 13) + AND-04: route the ACTION_START recovery
        // start through the guarded helper. AND-04 makes the helper return a
        // structured outcome so a boot-time restriction is not just logged but
        // recovered: ANY Rejected (including a runtime
        // ForegroundServiceStartNotAllowedException reason the static
        // BOOT_BLOCKED_TYPES_API35 set doesn't enumerate — that set is now only
        // an optimization for the known fast-path cases) persists recovery +
        // posts one notification so the user can resume the service manually.
        when (val outcome = startServiceGuarded(context, intent, foreground = true)) {
            ServiceStartOutcome.Started -> { /* delivered; LifecycleService owns the rest */ }
            is ServiceStartOutcome.Rejected -> {
                DurableState.save(context, DurableState.load(context).copy(
                    recoveryPending = true,
                    recoveryReason = "start_rejected:" + outcome.cause.javaClass.simpleName,
                ))
                postRecoveryNotification(context, label)
            }
        }
    }
}
