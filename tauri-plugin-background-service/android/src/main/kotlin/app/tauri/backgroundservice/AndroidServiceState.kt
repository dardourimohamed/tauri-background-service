package app.tauri.backgroundservice

import android.content.Context
import app.tauri.plugin.JSObject

data class AndroidServiceState(
    val nativeRunning: Boolean = false,
    val nativeForeground: Boolean = false,
    val desiredRunning: Boolean = false,
    val durableState: String = "unknown",
    val serviceLabel: String? = null,
    val foregroundServiceType: String? = null,
    val notificationId: Int? = null,
    val notificationChannelId: String? = null,
    val recoveryPending: Boolean = false,
    val recoveryReason: String? = null,
    val lastPlatformError: String? = null,
    val dataDir: String = "",
) {
    fun toJSON(): JSObject {
        val json = JSObject()
        json.put("nativeRunning", nativeRunning)
        json.put("nativeForeground", nativeForeground)
        json.put("desiredRunning", desiredRunning)
        json.put("durableState", durableState)
        json.put("serviceLabel", serviceLabel)
        json.put("foregroundServiceType", foregroundServiceType)
        json.put("notificationId", notificationId)
        json.put("notificationChannelId", notificationChannelId)
        json.put("recoveryPending", recoveryPending)
        json.put("recoveryReason", recoveryReason)
        json.put("lastPlatformError", lastPlatformError)
        json.put("dataDir", dataDir)
        return json
    }

    companion object {
        fun fromJSON(json: JSObject): AndroidServiceState {
            return AndroidServiceState(
                nativeRunning = json.optBoolean("nativeRunning", false),
                nativeForeground = json.optBoolean("nativeForeground", false),
                desiredRunning = json.optBoolean("desiredRunning", false),
                durableState = json.optString("durableState") ?: "unknown",
                serviceLabel = json.optString("serviceLabel").ifEmpty { null },
                foregroundServiceType = json.optString("foregroundServiceType").ifEmpty { null },
                notificationId = if (json.has("notificationId") && !json.isNull("notificationId"))
                    json.getInt("notificationId") else null,
                notificationChannelId = json.optString("notificationChannelId").ifEmpty { null },
                recoveryPending = json.optBoolean("recoveryPending", false),
                recoveryReason = json.optString("recoveryReason").ifEmpty { null },
                lastPlatformError = json.optString("lastPlatformError").ifEmpty { null },
                dataDir = json.optString("dataDir") ?: "",
            )
        }

        fun query(context: Context): AndroidServiceState {
            val durableState = DurableState.load(context)
            val prefs = context.getSharedPreferences("bg_service", Context.MODE_PRIVATE)

            return AndroidServiceState(
                nativeRunning = LifecycleService.isRunning,
                nativeForeground = LifecycleService.isForeground,
                desiredRunning = durableState.desiredRunning,
                durableState = durableState.lastNativeState,
                serviceLabel = durableState.lastServiceLabel.ifEmpty { null },
                foregroundServiceType = durableState.lastServiceType.ifEmpty { null },
                notificationId = prefs.getInt("bg_notif_id", -1).let { if (it == -1) null else it },
                notificationChannelId = prefs.getString("bg_notif_channel_id", null),
                recoveryPending = durableState.recoveryPending,
                recoveryReason = durableState.recoveryReason,
                lastPlatformError = durableState.lastPlatformError,
                dataDir = context.dataDir.absolutePath,
            )
        }
    }
}
