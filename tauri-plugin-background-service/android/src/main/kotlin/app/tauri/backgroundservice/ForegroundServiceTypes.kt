package app.tauri.backgroundservice

import android.content.Context
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build

/**
 * AND-01: the single source mapping a foreground-service type string → its
 * [ServiceInfo] bitmask, shared by `LifecycleService.mapServiceType` (the
 * `startForeground(..., type)` dispatch) and the plugin's merged-manifest
 * preflight. A type allowlisted in config but absent from the merged
 * `<service foregroundServiceType>` would otherwise reach `startForeground` and
 * crash late (Android 14+ rejects an undeclared bit). Centralizing the mapping
 * keeps the dispatch and the preflight in lockstep.
 */
internal object ForegroundServiceTypes {
    /** The bitmask for [type]; throws if [type] is not a known FGS type. */
    fun bitFor(type: String): Int = when (type) {
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

    /**
     * The merged manifest's `foregroundServiceType` bitmask declared on this
     * app's [LifecycleService], or 0 when unavailable — API < 29 (the field does
     * not exist), or the service is absent from the merged manifest. Callers
     * treat 0 as "no declaration to check against" (the config allowlist already
     * gated the request).
     */
    fun declaredBits(
        context: Context,
        serviceName: String = LifecycleService::class.java.name,
    ): Int {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return 0
        return try {
            val info = context.packageManager.getPackageInfo(
                context.packageName,
                PackageManager.GET_SERVICES,
            )
            info.services?.firstOrNull { it.name == serviceName }?.foregroundServiceType ?: 0
        } catch (_: PackageManager.NameNotFoundException) {
            0
        }
    }
}
