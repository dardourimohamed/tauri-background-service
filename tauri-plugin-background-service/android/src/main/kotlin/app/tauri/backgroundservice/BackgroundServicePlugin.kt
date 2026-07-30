package app.tauri.backgroundservice

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.annotation.VisibleForTesting
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.json.JSONArray
import java.util.UUID

@InvokeArg class StartKeepaliveArgs {
    var label: String = "Service running"
    var foregroundServiceType: String = "dataSync"
}

// spec 08 C6 (Step 15): swap the running FGS type (remoteMessaging ↔ phoneCall)
// without restarting the headless core.
@InvokeArg
class UpdateForegroundServiceTypeArgs {
    var foregroundServiceType: String = "phoneCall"
}

// spec 08 C6 (Step 15): native incoming-call notification (full-screen intent
// + ringtone channel) when the webview is closed.
@InvokeArg
class ShowIncomingCallArgs {
    var callId: String = ""
    var callerName: String = "Unknown caller"
    var isVideo: Boolean = false
}

@InvokeArg
class CancelIncomingCallArgs {
    var callId: String = ""
}

@InvokeArg
class ShowMessageNotificationArgs {
    var notificationId: Int = 0
    var chatId: String = ""
    var messageId: String = ""
    var title: String = ""
    var body: String = ""
    var routeUri: String = ""
}

// M-NATIVE-3 (Step 11 / CCF-11): set the active call's device audio route.
@InvokeArg
class SetCallAudioRouteArgs {
    var callId: String = ""
    var route: String = "system"
}

@TauriPlugin(
    permissions = [
        Permission(
            strings = ["android.permission.POST_NOTIFICATIONS"],
            alias = "notifications",
        ),
    ],
)
class BackgroundServicePlugin(private val activity: Activity) : Plugin(activity) {

    private var allowedFgsTypes: List<String> = listOf("dataSync")
    private var validateFgsType: Boolean = true
    private var onTimeoutPolicy: String = "notifyUser"
    private var notificationChannelId: String = "bg_service"
    private var notificationChannelName: String = "Background Service"
    private var notificationId: Int = 9001
    private var notificationSmallIcon: String? = null
    private var showStopAction: Boolean = true
    // NTF-09 (Step 10a): default false — the load() startup prompt is suppressed
    // by default; notifications are consented via an explainer (Step 10c). This
    // Kotlin optBoolean fallback IS the load-bearing gate: Tauri forwards the RAW
    // plugin config to mobile (plugin.rs initialize -> raw_config -> mobile
    // PluginManager.load), and the app does not set this key, so a Rust-only
    // serde flip would be a no-op for load().
    @VisibleForTesting
    internal var requestNotificationPermissionOnLoad: Boolean = false

    private fun prefs() =
        activity.getSharedPreferences("bg_service", Context.MODE_PRIVATE)

    override fun load(webView: android.webkit.WebView) {
        super.load(webView)
        loadConfig()
        // Request POST_NOTIFICATIONS once so Rust's Notifier can fire freely
        if (requestNotificationPermissionOnLoad) {
            ensureNotificationPermissionResolved()
        }

        // spec 08 C6 (Step 15): register the self-managed Telecom phone account
        // once at init so the OS can route audio focus, Bluetooth, and the
        // system call sheet through BackgroundCallConnectionService while the webview is
        // closed. Idempotent, API26+-guarded, and SecurityException-safe.
        BackgroundCallConnectionService.registerPhoneAccount(activity)

        // Register timeout callback so LifecycleService can emit events to JS.
        onTimeoutEvent = { errorMessage ->
            val data = JSObject()
            data.put("type", "stopped")
            data.put("reason", "timeout")
            data.put("platformError", errorMessage)
            trigger("timeout", data)
        }

        // Register native lifecycle event callback so LifecycleService can
        // signal notification-stop and timeout events to Rust via JS bridge.
        // The TypeScript layer listens for "native-lifecycle-event" and calls
        // the Rust native_lifecycle_event command.
        onNativeLifecycleEvent = { eventType, fgsType ->
            val data = JSObject()
            data.put("type", eventType)
            if (fgsType != null) {
                data.put("fgsType", fgsType)
            }
            trigger("native-lifecycle-event", data)
        }

        // Surface foreground-start / FGS-type failures to JS so the UI renders
        // them instead of the service self-stopping silently (R-W1.3 / NFR-1).
        onPlatformErrorEvent = { platformError ->
            val data = JSObject()
            data.put("error", platformError)
            trigger("platform-error", data)
        }

        // AND-05: the callbacks are now attached — drain any native
        // lifecycle/platform events that LifecycleService enqueued before load()
        // (boot / OS-restart / timeout paths that run with no webview), exactly
        // once, in insertion order. Without this drain those events were dropped.
        drainQueuedNativeEvents()
    }

    // tauri 2.x's Plugin base class is mid-migration to onDestroy(activity); it
    // still drives the no-arg onDestroy(), so we keep overriding it and silence the
    // transitional deprecation (tauri's own generated WryActivity does the same).
    @Suppress("OVERRIDE_DEPRECATION", "DEPRECATION")
    override fun onDestroy() {
        onTimeoutEvent = null
        onNativeLifecycleEvent = null
        onPlatformErrorEvent = null
        super.onDestroy()
    }

    private fun loadConfig() {
        applyConfigJson(handle?.config)
    }

    /**
     * Apply the RAW plugin config JSON (the `handle.config` string Tauri forwards
     * from `tauri.conf.json`). Extracted from [loadConfig] so the NTF-09
     * `requestNotificationPermissionOnLoad` default (and the rest of the config
     * parsing) is unit-testable without a Tauri `PluginHandle`.
     */
    @VisibleForTesting
    internal fun applyConfigJson(configJson: String?) {
        if (configJson.isNullOrEmpty()) return
        val json = try { org.json.JSONObject(configJson) } catch (_: Exception) { return }
        val typesArray = json.optJSONArray("androidForegroundServiceTypes")
        if (typesArray != null) {
            allowedFgsTypes = (0 until typesArray.length()).map { typesArray.getString(it) }
        }
        validateFgsType = json.optBoolean("androidValidateForegroundServiceType", true)
        onTimeoutPolicy = json.optString("androidOnTimeout", "notifyUser")
        notificationChannelId = json.optString("androidNotificationChannelId", "bg_service")
        notificationChannelName = json.optString("androidNotificationChannelName", "Background Service")
        notificationId = json.optInt("androidNotificationId", 9001)
        notificationSmallIcon = json.optString("androidNotificationSmallIcon").ifEmpty { null }
        showStopAction = json.optBoolean("androidShowStopAction", true)
        // NTF-09 (Step 10a): default false — opt out of the unconditional load()
        // startup prompt; the consented explainer (Step 10c) drives the request.
        requestNotificationPermissionOnLoad = json.optBoolean(
            "androidRequestNotificationPermissionOnLoad", false)
    }

    /**
     * Resolve `POST_NOTIFICATIONS` before the first foreground transition
     * (spec-compliance W1 / R-W1.3). A foreground service whose ongoing
     * notification can't post degrades the start on Android 13+, so request the
     * permission up-front. No-op below API 33 or when already granted; returns
     * whether the permission is already granted.
     */
    internal fun ensureNotificationPermissionResolved(): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return true
        val granted = activity.checkSelfPermission(
            android.Manifest.permission.POST_NOTIFICATIONS
        ) == android.content.pm.PackageManager.PERMISSION_GRANTED
        if (!granted) {
            activity.requestPermissions(
                arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 1001)
            // BGS-21 (doc-08 Step 12 Task 2): this is the DEFAULT first-ask flow
            // (callers: `load()` when `requestNotificationPermissionOnLoad`
            // defaults true, and `startKeepalive()`). It issues a real
            // POST_NOTIFICATIONS ask, so it MUST persist `hasAsked=true`
            // exactly like the explicit `@Command requestNotificationPermission`
            // site — otherwise a user who denies this auto-prompt is later
            // mis-classified as never-asked ("notDetermined") instead of
            // "denied". `load().copy()` so the notification-ask flag does NOT
            // clobber the rest of the durable BG-service state (desiredRunning
            // et al.). Mirrors the requestNotificationPermission persist.
            DurableState.save(activity, DurableState.load(activity).copy(
                hasAskedNotificationPermission = true,
            ))
        }
        return granted
    }

    @Command
    fun startKeepalive(invoke: Invoke) {
        val args = invoke.parseArgs(StartKeepaliveArgs::class.java)
        Log.i("BGServicePlugin", "startKeepalive: label=${args.label}, fgsType=${args.foregroundServiceType}")

        // R-W1.3: resolve POST_NOTIFICATIONS BEFORE dispatching the service
        // start intent (i.e. before the service's first startForeground), so
        // the persistent foreground notification is allowed to post.
        ensureNotificationPermissionResolved()

        val validationError = validateForegroundServiceType(
            args.foregroundServiceType, allowedFgsTypes, validateFgsType
        )
        if (validationError != null) {
            invoke.reject(validationError)
            return
        }

        // AND-01: also reject a type that passes the allowlist but is absent
        // from the merged <service foregroundServiceType> — it would crash late
        // at startForeground on Android 14+. Checked before any dispatch.
        val declaredError = validateDeclaredForegroundServiceType(
            args.foregroundServiceType, ForegroundServiceTypes.declaredBits(activity)
        )
        if (declaredError != null) {
            invoke.reject(declaredError)
            return
        }

        // F6: Persist all prefs BEFORE starting the service using commit() (synchronous).
        // onStartCommand can fire before apply() completes, causing missing prefs.
        val committed = prefs().edit()
            .putString("bg_service_label", args.label)
            .putString("bg_service_type", args.foregroundServiceType)
            .putString("bg_notif_channel_id", notificationChannelId)
            .putString("bg_notif_channel_name", notificationChannelName)
            .putInt("bg_notif_id", notificationId)
            .putString("bg_notif_small_icon", notificationSmallIcon)
            .putBoolean("bg_show_stop_action", showStopAction)
            .putString("bg_on_timeout_policy", onTimeoutPolicy)
            .commit()
        if (!committed) {
            invoke.reject(ServiceStartAckRegistry.errorJson(
                "prefs_commit_failed",
                "Failed to persist foreground service configuration before start",
            ))
            return
        }

        // F7: Wrap service start in try/catch for structured error reporting
        val startAckId = UUID.randomUUID().toString()
        ServiceStartAckRegistry.register(startAckId)
        try {
            val intent = Intent(activity, LifecycleService::class.java).apply {
                action = LifecycleService.ACTION_START
                putExtra(LifecycleService.EXTRA_LABEL, args.label)
                putExtra(LifecycleService.EXTRA_SERVICE_TYPE, args.foregroundServiceType)
                putExtra(LifecycleService.EXTRA_START_ACK_ID, startAckId)
                putExtra(LifecycleService.EXTRA_START_REASON, "manual_start")
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O)
                activity.startForegroundService(intent)
            else
                activity.startService(intent)
        } catch (e: Exception) {
            ServiceStartAckRegistry.forget(startAckId)
            // Roll back active prefs on failure
            rollbackActivePrefs()

            // Persist error for diagnostics
            val errorJson = mapServiceStartException(e, args.foregroundServiceType)
            DurableState.save(activity, DurableState(
                desiredRunning = false,
                lastServiceLabel = args.label,
                lastServiceType = args.foregroundServiceType,
                lastStartEpochMs = System.currentTimeMillis(),
                lastPlatformError = errorJson,
            ))
            invoke.reject(errorJson)
            return
        }

        // spec-compliance W1 / R-W1.2: AWAIT the start ACK OFF the main looper.
        // `startKeepalive` runs as a Tauri @Command on the main thread, and the
        // ACK it waits for is produced by `LifecycleService.onStartCommand`,
        // which ALSO runs on the main thread. Blocking the main thread here on
        // `ServiceStartAckRegistry.await` therefore DEADLOCKS the service start:
        // onStartCommand can never reach `startForeground()` until this wait
        // times out (~30 s), tripping the FGS ANR (`startForegroundCount=0` —
        // confirmed on-device, Waydroid). Waiting on a worker thread frees the
        // looper so onStartCommand runs and completes the ACK; `invoke` is then
        // resolved/rejected asynchronously (Tauri permits off-thread resolution).
        awaitStartAck(
            startAckId,
            onSuccess = { invoke.resolve() },
            onFailure = { ack ->
                rollbackActivePrefs()
                // BGS-30: route the ACTION_STOP cleanup through the guarded helper so an
                // OS start-restriction is logged instead of crashing. foreground=false: a
                // stop intent must not re-enter the FGS-start contract.
                startServiceGuarded(
                    activity,
                    Intent(activity, LifecycleService::class.java)
                        .apply { action = LifecycleService.ACTION_STOP },
                    foreground = false,
                )
                DurableState.save(activity, DurableState(
                    desiredRunning = false,
                    lastServiceLabel = args.label,
                    lastServiceType = args.foregroundServiceType,
                    lastStartEpochMs = System.currentTimeMillis(),
                    lastPlatformError = ack.payload,
                ))
                invoke.reject(ack.payload)
            },
        )
    }

    private fun rollbackActivePrefs() {
        prefs().edit()
            .remove("bg_service_label")
            .remove("bg_service_type")
            .remove("bg_notif_channel_id")
            .remove("bg_notif_channel_name")
            .remove("bg_notif_id")
            .remove("bg_notif_small_icon")
            .remove("bg_show_stop_action")
            .remove("bg_on_timeout_policy")
            .commit()
    }

    @Command
    fun stopKeepalive(invoke: Invoke) {
        prefs().edit()
            .remove("bg_service_label")
            .remove("bg_service_type")
            .remove("bg_notif_channel_id")
            .remove("bg_notif_channel_name")
            .remove("bg_notif_id")
            .remove("bg_notif_small_icon")
            .remove("bg_show_stop_action")
            .remove("bg_on_timeout_policy")
            .commit()
        DurableState.clear(activity)
        // BGS-30: route the ACTION_STOP cleanup through the guarded helper (foreground=false).
        startServiceGuarded(
            activity,
            Intent(activity, LifecycleService::class.java)
                .apply { action = LifecycleService.ACTION_STOP },
            foreground = false,
        )
        invoke.resolve()
    }

    // spec 08 C6 (Step 15): swap the foreground service type of the running
    // service (e.g. remoteMessaging → phoneCall on answer). No-op + reject if
    // the service is not running — the type swap rides the running service.
    @Command
    fun updateForegroundServiceType(invoke: Invoke) {
        val args = invoke.parseArgs(UpdateForegroundServiceTypeArgs::class.java)
        val validationError = validateForegroundServiceType(
            args.foregroundServiceType, allowedFgsTypes, validateFgsType
        )
        if (validationError != null) {
            invoke.reject(validationError)
            return
        }

        // AND-01: a type swap to an undeclared bit would crash late at the
        // re-promotion startForeground; reject before dispatch.
        val declaredError = validateDeclaredForegroundServiceType(
            args.foregroundServiceType, ForegroundServiceTypes.declaredBits(activity)
        )
        if (declaredError != null) {
            invoke.reject(declaredError)
            return
        }
        if (!LifecycleService.isRunning) {
            invoke.reject(ServiceStartAckRegistry.errorJson(
                "not_running",
                "Cannot update foreground service type: service is not running",
            ))
            return
        }
        val intent = Intent(activity, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_UPDATE_TYPE
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, args.foregroundServiceType)
        }
        // BGS-30: route the ACTION_UPDATE_TYPE start through the guarded helper
        // (foreground=true preserves the original branched startForegroundService/startService).
        startServiceGuarded(activity, intent, foreground = true)
        invoke.resolve()
    }

    // spec 08 C6 (Step 15): fire the native incoming-call notification (headless
    // offer, webview closed). CallStyle + full-screen intent when granted,
    // CallStyle + ringtone fallback (F4) otherwise.
    @Command
    fun showIncomingCall(invoke: Invoke) {
        val args = invoke.parseArgs(ShowIncomingCallArgs::class.java)
        IncomingCallNotifier.showIncomingCall(
            context = activity,
            callId = args.callId,
            callerName = args.callerName,
            isVideo = args.isVideo,
            smallIcon = resolveSmallIcon(),
            launchIntent = activity.packageManager
                .getLaunchIntentForPackage(activity.packageName),
        )
        // M-NATIVE-3 (Step 11): DRIVE the registered self-managed Telecom account
        // on the inbound offer so the OS creates a BackgroundCallConnection and its
        // audio-focus / MODE_IN_COMMUNICATION path (Step 9) engages — no longer a
        // registered-but-undriven account.
        BackgroundCallConnectionService.addNewIncomingCall(activity, args.callId, args.isVideo)
        invoke.resolve()
    }

    // M-NATIVE-3 (Step 11 / CCF-11): set the active call's device audio route
    // (speaker/earpiece/bluetooth/system). Applied to the live self-managed
    // BackgroundCallConnection via Connection.setAudioRoute (the route command's GUI path
    // reaches this through ServiceManagerHandle::set_call_audio_route).
    @Command
    fun setCallAudioRoute(invoke: Invoke) {
        val args = invoke.parseArgs(SetCallAudioRouteArgs::class.java)
        BackgroundCallConnectionService.setCallAudioRoute(args.callId, args.route)
        invoke.resolve()
    }

    @Command
    fun cancelIncomingCall(invoke: Invoke) {
        val args = invoke.parseArgs(CancelIncomingCallArgs::class.java)
        IncomingCallNotifier.cancel(activity, args.callId)
        invoke.resolve()
    }

    @Command
    fun showMessageNotification(invoke: Invoke) {
        val args = invoke.parseArgs(ShowMessageNotificationArgs::class.java)
        ActionableMessageNotifier.showMessageNotification(
            context = activity,
            notificationId = args.notificationId,
            chatId = args.chatId,
            messageId = args.messageId,
            title = args.title,
            body = args.body,
            routeUri = args.routeUri,
            smallIcon = resolveSmallIcon(),
            launchIntent = activity.packageManager.getLaunchIntentForPackage(activity.packageName),
        )
        invoke.resolve()
    }

    // M-DIAG-2 / CCF-12 (Step 17): open this app's OS settings (app-details /
    // permission screen) so the user can grant a previously-denied camera/mic
    // permission. The denied-permission "Open Settings" affordance reaches this
    // through ServiceManagerHandle::open_app_settings → run_mobile_plugin.
    @Command
    fun openAppSettings(invoke: Invoke) {
        val intent = Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS).apply {
            data = Uri.fromParts("package", activity.packageName, null)
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        activity.startActivity(intent)
        invoke.resolve()
    }

    private fun resolveSmallIcon(): Int {
        return NotificationIconResolver.resolve(activity, notificationSmallIcon)
    }

    @Command
    fun getAndroidServiceState(invoke: Invoke) {
        val state = AndroidServiceState.query(activity)
        invoke.resolve(state.toJSON())
    }

    @Command
    fun moveTaskToBackground(invoke: Invoke) {
        activity.moveTaskToBack(true)
        invoke.resolve()
    }

    @Command
    fun getNotificationPermissionStatus(invoke: Invoke) {
        val status = if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
            "granted"
        } else {
            val isGranted = activity.checkSelfPermission(
                android.Manifest.permission.POST_NOTIFICATIONS
            ) == android.content.pm.PackageManager.PERMISSION_GRANTED
            val shouldShowRationale = activity.shouldShowRequestPermissionRationale(
                android.Manifest.permission.POST_NOTIFICATIONS
            )
            // BGS-21 (doc-08 Step 12): pass the persisted hasAsked discriminator so
            // the mapping can tell never-asked (notDetermined) from denied-once
            // (denied) — shouldShowRationale alone is ambiguous.
            val hasAsked = DurableState.load(activity).hasAskedNotificationPermission
            computePermissionStatus(isGranted, shouldShowRationale, hasAsked)
        }
        val result = JSObject()
        result.put("status", status)
        invoke.resolve(result)
    }

    @Command
    fun requestNotificationPermission(invoke: Invoke) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
            // Below API 33 POST_NOTIFICATIONS is implicitly granted.
            invoke.resolve(notificationPermissionStatusResult())
            return
        }
        // Persist the asked discriminator before handing the Invoke to Tauri's
        // permission callback. This preserves never-asked vs denied semantics
        // while resolving JS only after the user makes a choice.
        // BGS-21 (doc-08 Step 12): we have now asked at least once — persist so a
        // later getNotificationPermissionStatus reads hasAsked=true and maps a
        // denial to "denied" instead of the never-asked "notDetermined". load()
        // .copy() so the notification-ask flag does NOT clobber the rest of the
        // durable BG-service state (desiredRunning et al.).
        DurableState.save(activity, DurableState.load(activity).copy(
            hasAskedNotificationPermission = true,
        ))
        requestPermissionForAlias("notifications", invoke, "onNotificationPermissionResult")
    }

    // BGS-22 (doc-08 Step 14): request the Android battery-optimization (Doze)
    // exemption. The REQUEST_IGNORE_BATTERY_OPTIMIZATIONS permission is declared
    // in the plugin AndroidManifest.xml but was never requested until this flow.
    // The system dialog is the ONLY honest way to obtain the exemption; we
    // prefer wiring the user-granted flow over dropping the permission. There is
    // no status mirror: the exemption is OS-only and not queryable here.
    @Command
    fun requestBatteryExemption(invoke: Invoke) {
        launchBatteryExemptionRequest()
        invoke.resolve()
    }

    /**
     * Fire the system `ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` dialog for
     * this app (BGS-22, doc-08 Step 14).
     *
     * Extracted from the `@Command requestBatteryExemption` wrapper so the
     * Robolectric test can drive the startActivity without a Tauri Invoke
     * (mirrors the `ensureNotificationPermissionResolved` seam used by BGS-21).
     * The action + permission exist since API 23 (M); minSdk is 24 so no runtime
     * API gate is needed.
     */
    @VisibleForTesting
    internal fun launchBatteryExemptionRequest() {
        val intent = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
            data = Uri.parse("package:${activity.packageName}")
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        activity.startActivity(intent)
    }

    // NTF-16 (Step 12c): whether this app may post a full-screen intent
    // (USE_FULL_SCREEN_INTENT). On API 34+ the user/system may revoke it; the
    // notification screen surfaces a re-grant affordance when this is false.
    // Immediate-resolve getter — no @PermissionCallback / spawn_blocking.
    @Command
    fun canUseFullScreenIntent(invoke: Invoke) {
        val canUse = IncomingCallNotifier.canUseFullScreenIntent(activity)
        val result = JSObject()
        result.put("canUse", canUse)
        invoke.resolve(result)
    }

    // NTF-16 (Step 12c): open the OS settings page where the user can re-grant
    // USE_FULL_SCREEN_INTENT (or the per-app notification settings on API 29-33).
    // Immediate-resolve (startActivity, returns void) — no payload.
    @Command
    fun openFullScreenIntentSettings(invoke: Invoke) {
        IncomingCallNotifier.openFullScreenIntentSettings(activity)
        invoke.resolve()
    }

    /**
     * NTF-09 (Step 10a): invoked by the Tauri permission machinery
     * ([Plugin.requestPermissionForAlias] -> PluginHandle reflection) once the
     * user grants or denies POST_NOTIFICATIONS. The ActivityResult callback
     * carries no grantResults, so the outcome is re-derived from
     * [checkSelfPermission] and the deferred [Invoke] is resolved with the
     * resulting `{status: granted|denied}`.
     */
    @PermissionCallback
    fun onNotificationPermissionResult(invoke: Invoke) {
        invoke.resolve(notificationPermissionStatusResult())
    }

    private fun notificationPermissionStatusResult(): JSObject {
        return JSObject().put("status", notificationPermissionStatus())
    }

    /**
     * Re-derive the POST_NOTIFICATIONS grant state. Below API 33 the permission
     * does not exist and is implicitly granted; otherwise check the runtime
     * permission. Shared by the [onNotificationPermissionResult] callback and the
     * below-TIRAMISU short-circuit in [requestNotificationPermission].
     */
    private fun notificationPermissionStatus(): String {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return "granted"
        val isGranted = activity.checkSelfPermission(
            android.Manifest.permission.POST_NOTIFICATIONS
        ) == android.content.pm.PackageManager.PERMISSION_GRANTED
        return if (isGranted) "granted" else "denied"
    }

    companion object {
        @Volatile
        internal var onTimeoutEvent: ((String) -> Unit)? = null

        @Volatile
        internal var onNativeLifecycleEvent: ((String, String?) -> Unit)? = null

        // spec-compliance W1 / R-W1.3 (NFR-1): a foreground-start failure or
        // FGS-type rejection must surface to JS so the UI can render it — the
        // service must never self-stop silently. LifecycleService invokes this
        // with the durable platform-error string.
        @Volatile
        internal var onPlatformErrorEvent: ((String) -> Unit)? = null

        // ── AND-05: native event emit/drain seam ──────────────────────────
        //
        // Emitters used by LifecycleService: if the matching JS callback is
        // attached, deliver immediately; otherwise enqueue into the bounded
        // process queue so load() can replay the event once, in order. This
        // replaces the old `callback?.invoke(...)` no-op that dropped every
        // event emitted before the plugin loaded.

        internal fun emitNativeLifecycleEvent(type: String, fgsType: String?) {
            val cb = onNativeLifecycleEvent
            if (cb != null) {
                cb(type, fgsType)
            } else {
                NativeEventQueue.enqueue(QueuedNativeEvent.Lifecycle(type, fgsType))
            }
        }

        internal fun emitTimeoutEvent(errorMessage: String) {
            val cb = onTimeoutEvent
            if (cb != null) {
                cb(errorMessage)
            } else {
                NativeEventQueue.enqueue(QueuedNativeEvent.Timeout(errorMessage))
            }
        }

        internal fun emitPlatformErrorEvent(error: String) {
            val cb = onPlatformErrorEvent
            if (cb != null) {
                cb(error)
            } else {
                NativeEventQueue.enqueue(QueuedNativeEvent.PlatformError(error))
            }
        }

        /**
         * AND-05: replay every native event queued before load() attached the
         * callbacks, exactly once, in insertion order. Called from [load] after
         * all three callbacks are wired.
         */
        @VisibleForTesting
        internal fun drainQueuedNativeEvents() {
            val queued = NativeEventQueue.drainAndClear()
            if (queued.isEmpty()) return
            for (event in queued) {
                when (event) {
                    is QueuedNativeEvent.Lifecycle ->
                        onNativeLifecycleEvent?.invoke(event.type, event.fgsType)
                    is QueuedNativeEvent.Timeout ->
                        onTimeoutEvent?.invoke(event.errorMessage)
                    is QueuedNativeEvent.PlatformError ->
                        onPlatformErrorEvent?.invoke(event.error)
                }
            }
        }

        internal const val START_ACK_TIMEOUT_MS = 30_000L

        // spec-compliance W1 / R-W1.2: the start-ACK wait MUST run off the main
        // looper (see startKeepalive — blocking it deadlocks onStartCommand and
        // trips the FGS ANR). Production spawns a worker thread; tests inject an
        // inline executor so the dispatch is deterministic.
        internal val DEFAULT_ACK_WAIT_EXECUTOR: (String, () -> Unit) -> Unit = { name, task ->
            Thread({ task() }, name).start()
        }

        @VisibleForTesting
        internal var ackWaitExecutor: (String, () -> Unit) -> Unit = DEFAULT_ACK_WAIT_EXECUTOR

        /**
         * Await the foreground-service start acknowledgement OFF the calling
         * (main) thread, then dispatch the result. Extracted from [startKeepalive]
         * so the non-blocking contract is unit-testable without a Tauri `Invoke`:
         * with [DEFAULT_ACK_WAIT_EXECUTOR] the call returns before a still-pending
         * ack resolves; [ServiceStartAckRegistry.complete] (called from
         * onStartCommand's worker) later drives [onSuccess]/[onFailure].
         */
        @VisibleForTesting
        internal fun awaitStartAck(
            startAckId: String,
            onSuccess: () -> Unit,
            onFailure: (ServiceStartAckRegistry.Ack) -> Unit,
        ) {
            ackWaitExecutor("bg-start-ack") {
                val ack = ServiceStartAckRegistry.await(startAckId, START_ACK_TIMEOUT_MS)
                if (ack.success) onSuccess() else onFailure(ack)
            }
        }

        fun computePermissionStatus(
            isGranted: Boolean,
            shouldShowRationale: Boolean,
            hasAsked: Boolean,
        ): String {
            if (isGranted) return "granted"
            // BGS-21 (doc-08 Step 12): Android `shouldShowRequestPermissionRationale`
            // returns FALSE for BOTH never-asked AND permanently-denied (and TRUE
            // only after a first soft denial), so it CANNOT distinguish them. The
            // persisted `hasAsked` flag is the discriminator:
            //   !hasAsked -> "notDetermined" (never asked, still promptable)
            //   hasAsked  -> "denied" (denied at least once)
            // `shouldShowRationale` no longer changes the returned status (both
            // denied sub-cases map to "denied"), but is retained on the signature
            // so Task 3's UI can later split denied-once (re-askable) from blocked
            // (route to system Settings) without re-querying the platform.
            if (!hasAsked) return "notDetermined"
            return "denied"
        }

        fun validateForegroundServiceType(
            requestedType: String,
            allowedTypes: List<String>,
            validate: Boolean
        ): String? {
            if (!validate) return null
            if (allowedTypes.contains(requestedType)) return null
            return org.json.JSONObject().apply {
                put("code", "fgs_type_not_allowed")
                put("message", "Foreground service type '$requestedType' is not in the configured allowlist $allowedTypes. " +
                    "Add it to androidForegroundServiceTypes in your plugin config.")
                put("invalidType", requestedType)
                put("validOptions", org.json.JSONArray(allowedTypes))
            }.toString()
        }

        /**
         * AND-01: reject a requested FGS type whose bit is NOT present in the
         * merged manifest's `<service foregroundServiceType>` — such a type
         * would pass the config allowlist but crash late at startForeground on
         * Android 14+. Returns a structured `fgs_type_not_declared` error, or
         * null when the type is declared (or when no declaration is observable,
         * e.g. API < 29 — the allowlist already gated the request). Pure over
         * [declaredBits] so it is deterministically unit-testable.
         */
        fun validateDeclaredForegroundServiceType(
            requestedType: String,
            declaredBits: Int,
        ): String? {
            val requestedBit = try {
                ForegroundServiceTypes.bitFor(requestedType)
            } catch (_: IllegalArgumentException) {
                // Unknown type — validateForegroundServiceType / mapServiceType reject it.
                return null
            }
            if (declaredBits == 0) return null // pre-Q / undeclared — allowlist already gated
            if (declaredBits and requestedBit == 0) {
                return org.json.JSONObject().apply {
                    put("code", "fgs_type_not_declared")
                    put("message", "Foreground service type '$requestedType' is allowlisted but " +
                        "not declared in the merged <service foregroundServiceType> " +
                        "(declaredBits=$declaredBits). Declare it (and the matching " +
                        "FOREGROUND_SERVICE_* permission) in the host manifest.")
                    put("invalidType", requestedType)
                    put("declaredBits", declaredBits)
                }.toString()
            }
            return null
        }

        /** Map a service-start exception to structured error JSON for reject. */
        fun mapServiceStartException(e: Exception, foregroundServiceType: String): String {
            val code = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S &&
                e is android.app.ForegroundServiceStartNotAllowedException
            ) {
                "FGS_NOT_ALLOWED"
            } else when (e) {
                is SecurityException -> "SECURITY"
                else -> "UNKNOWN"
            }
            return org.json.JSONObject().apply {
                put("code", code)
                put("message", e.message ?: e.javaClass.simpleName)
                put("foregroundServiceType", foregroundServiceType)
            }.toString()
        }
    }
}

/**
 * BGS-30 (doc-08 Step 13): best-effort guarded service start. Wraps the platform
 * [Context.startService] / [Context.startForegroundService] call in try/catch so
 * an OS start-restriction (background-start `IllegalStateException` on Android O+,
 * `ForegroundServiceStartNotAllowedException` on Android 12+) is logged rather
 * than crashing the host process. Mirrors the guarded pattern in
 * `HeadlessBridge.revertForegroundServiceType` (catch `Throwable` + `Log.w`).
 *
 * Shared by every edge-branch start site in this plugin AND by
 * [BootReceiver.startRecoveryService] (same module/package), so each call site
 * needs only to build its [Intent] and pick its start variant — none keeps a bare
 * `activity/context.startService(`/`startForegroundService(` call.
 *
 * `foreground` preserves each site's original start semantics:
 * - `true` → branched `startForegroundService` (Android O+) / `startService`
 *   (below O), for starts that promote the service to the foreground
 *   (`ACTION_START`, `ACTION_UPDATE_TYPE`);
 * - `false` → plain `startService`, for `ACTION_STOP` cleanup intents which must
 *   NOT re-enter the foreground-start contract (the service receives the stop and
 *   tears down without calling `startForeground`, so a `startForegroundService`
 *   delivery would risk a `ForegroundServiceDidNotStartInTime` ANR when the
 *   service is not already running).
 *
 * CROSS-DOC: doc 06 (BackgroundServicePlugin.kt is a de-facto cross-doc surface;
 * this guard is additive and shared with BootReceiver's recovery start).
 *
 * NOTE: the try/catch is necessary-but-not-sufficient for the deferred-FGS-contract
 * case (e.g. `ForegroundServiceDidNotStartInTime` if the service stops between the
 * `updateForegroundServiceType` isRunning check and `handleUpdateType`); that
 * remainder is bounded by `LifecycleService`'s `if (!isForeground)` guard and is
 * device-runbook only (Step 21) — not catchable at a call site.
 */
internal fun startServiceGuarded(
    context: Context,
    intent: Intent,
    foreground: Boolean,
): ServiceStartOutcome {
    return try {
        if (foreground && Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            context.startForegroundService(intent)
        } else {
            context.startService(intent)
        }
        ServiceStartOutcome.Started
    } catch (e: Throwable) {
        Log.w(
            "BackgroundServicePlugin",
            "Guarded service start failed (action=${intent.action}, foreground=$foreground): ${e.message}",
        )
        ServiceStartOutcome.Rejected(e)
    }
}

/**
 * AND-04: structured outcome for [startServiceGuarded]. The previous `Unit`
 * return swallowed every OS start-restriction silently; callers that need to
 * react (notably [BootReceiver.startRecoveryService]) now branch on the result.
 * `Rejected` carries the cause so a recovery path can persist a typed reason
 * and post a notification for ANY restriction (the boot blocked-type static set
 * on API 35+ is only an optimization for the KNOWN fast-path cases; a newer
 * `ForegroundServiceStartNotAllowedException` reason must not silently miss
 * recovery).
 */
sealed class ServiceStartOutcome {
    /** The start call returned without throwing. */
    object Started : ServiceStartOutcome()

    /** The platform rejected the start (caught `Throwable`). */
    data class Rejected(val cause: Throwable) : ServiceStartOutcome()
}
