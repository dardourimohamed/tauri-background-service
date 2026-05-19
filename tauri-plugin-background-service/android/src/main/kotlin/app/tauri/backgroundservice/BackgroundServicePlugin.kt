package app.tauri.backgroundservice

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.os.Build
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.json.JSONArray

@InvokeArg class StartKeepaliveArgs {
    var label: String = "Service running"
    var foregroundServiceType: String = "dataSync"
}

@InvokeArg
class GetAutoStartConfigResult {
    var pending: Boolean = false
    var label: String? = null
    var serviceType: String? = null
}

@TauriPlugin
class BackgroundServicePlugin(private val activity: Activity) : Plugin(activity) {

    private var allowedFgsTypes: List<String> = listOf("dataSync")
    private var validateFgsType: Boolean = true
    private var onTimeoutPolicy: String = "notifyUser"
    private var notificationChannelId: String = "bg_service"
    private var notificationChannelName: String = "Background Service"
    private var notificationId: Int = 9001
    private var notificationSmallIcon: String? = null
    private var showStopAction: Boolean = true
    private var requestNotificationPermissionOnLoad: Boolean = true

    private fun prefs() =
        activity.getSharedPreferences("bg_service", Context.MODE_PRIVATE)

    override fun load(webView: android.webkit.WebView) {
        super.load(webView)
        loadConfig()
        // Request POST_NOTIFICATIONS once so Rust's Notifier can fire freely
        if (requestNotificationPermissionOnLoad &&
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            activity.checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS)
            != android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            activity.requestPermissions(
                arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 1001)
        }

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
    }

    override fun onDestroy() {
        onTimeoutEvent = null
        onNativeLifecycleEvent = null
        super.onDestroy()
    }

    private fun loadConfig() {
        val configJson = handle?.config ?: return
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
        requestNotificationPermissionOnLoad = json.optBoolean("androidRequestNotificationPermissionOnLoad", true)
    }

    @Command
    fun startKeepalive(invoke: Invoke) {
        val args = invoke.parseArgs(StartKeepaliveArgs::class.java)

        val validationError = validateForegroundServiceType(
            args.foregroundServiceType, allowedFgsTypes, validateFgsType
        )
        if (validationError != null) {
            invoke.reject(validationError)
            return
        }

        val intent = Intent(activity, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_START
            putExtra(LifecycleService.EXTRA_LABEL, args.label)
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, args.foregroundServiceType)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O)
            activity.startForegroundService(intent)
        else
            activity.startService(intent)
        prefs().edit()
            .putString("bg_service_label", args.label)
            .putString("bg_service_type", args.foregroundServiceType)
            .putString("bg_notif_channel_id", notificationChannelId)
            .putString("bg_notif_channel_name", notificationChannelName)
            .putInt("bg_notif_id", notificationId)
            .putString("bg_notif_small_icon", notificationSmallIcon)
            .putBoolean("bg_show_stop_action", showStopAction)
            .putString("bg_on_timeout_policy", onTimeoutPolicy)
            .apply()
        invoke.resolve()
    }

    @Command
    fun stopKeepalive(invoke: Invoke) {
        prefs().edit()
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
        DurableState.clear(activity)
        activity.startService(Intent(activity, LifecycleService::class.java)
            .apply { action = LifecycleService.ACTION_STOP })
        invoke.resolve()
    }

    @Command
    fun getAutoStartConfig(invoke: Invoke) {
        val p = prefs()
        val result = GetAutoStartConfigResult()
        result.pending = p.getBoolean("bg_auto_start_pending", false)
        result.label = p.getString("bg_auto_start_label", null)
        result.serviceType = p.getString("bg_auto_start_type", null)
        invoke.resolveObject(result)
    }

    @Command
    fun clearAutoStartConfig(invoke: Invoke) {
        prefs().edit()
            .remove("bg_auto_start_pending")
            .remove("bg_auto_start_label")
            .remove("bg_auto_start_type")
            .apply()
        invoke.resolve()
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
            computePermissionStatus(isGranted, shouldShowRationale)
        }
        val result = JSObject()
        result.put("status", status)
        invoke.resolve(result)
    }

    @Command
    fun requestNotificationPermission(invoke: Invoke) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
            val result = JSObject()
            result.put("status", "granted")
            invoke.resolve(result)
            return
        }
        activity.requestPermissions(
            arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 1001)
        invoke.resolve()
    }

    companion object {
        @Volatile
        internal var onTimeoutEvent: ((String) -> Unit)? = null

        @Volatile
        internal var onNativeLifecycleEvent: ((String, String?) -> Unit)? = null

        fun computePermissionStatus(isGranted: Boolean, shouldShowRationale: Boolean): String {
            if (isGranted) return "granted"
            return if (shouldShowRationale) "notDetermined" else "denied"
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
    }
}
