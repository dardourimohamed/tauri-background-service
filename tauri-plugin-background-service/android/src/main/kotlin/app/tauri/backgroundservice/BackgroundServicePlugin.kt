package app.tauri.backgroundservice

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.os.Build
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

@InvokeArg class StartKeepaliveArgs {
    var label: String = "Service running"
    var foregroundServiceType: String = "dataSync"
}

@InvokeArg
class GetAutoStartConfigResult {
    var pending: Boolean = false
    var label: String? = null
}

@TauriPlugin
class BackgroundServicePlugin(private val activity: Activity) : Plugin(activity) {

    private fun prefs() =
        activity.getSharedPreferences("bg_service", Context.MODE_PRIVATE)

    override fun load(webView: android.webkit.WebView) {
        super.load(webView)
        // Request POST_NOTIFICATIONS once so Rust's Notifier can fire freely
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            activity.checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS)
            != android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            activity.requestPermissions(
                arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 1001)
        }
    }

    @Command
    fun startKeepalive(invoke: Invoke) {
        val args  = invoke.parseArgs(StartKeepaliveArgs::class.java)
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
            .apply()
        invoke.resolve()
    }

    @Command
    fun stopKeepalive(invoke: Invoke) {
        prefs().edit()
            .remove("bg_service_label")
            .remove("bg_auto_start_pending")
            .remove("bg_auto_start_label")
            .apply()
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
        invoke.resolveObject(result)
    }

    @Command
    fun clearAutoStartConfig(invoke: Invoke) {
        prefs().edit()
            .remove("bg_auto_start_pending")
            .remove("bg_auto_start_label")
            .apply()
        invoke.resolve()
    }

    @Command
    fun moveTaskToBackground(invoke: Invoke) {
        activity.moveTaskToBack(true)
        invoke.resolve()
    }
}
