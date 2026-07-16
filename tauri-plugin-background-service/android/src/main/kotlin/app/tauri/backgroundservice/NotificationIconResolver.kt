package app.tauri.backgroundservice

import android.content.Context

/**
 * Resolves notification small icons from app-owned resources.
 *
 * Sila's generated app icon lives in `mipmap/ic_launcher`, while older plugin
 * config only looked in `drawable` and then fell back to Android's sync icon.
 * This resolver prefers configured app resources, then the generated launcher
 * icon, then the manifest application icon. The system sync icon is retained
 * only as a true last resort.
 */
object NotificationIconResolver {
    fun resolve(context: Context, configuredName: String? = null): Int {
        resolveNamed(context, configuredName)?.let { return it }
        resolveNamed(context, "ic_stat_sila")?.let { return it }
        resolveNamed(context, "ic_launcher")?.let { return it }
        resolveNamed(context, "ic_launcher_foreground")?.let { return it }

        val appIcon = context.applicationInfo.icon
        if (appIcon != 0) return appIcon

        return android.R.drawable.stat_notify_sync
    }

    private fun resolveNamed(context: Context, name: String?): Int? {
        if (name.isNullOrBlank()) return null
        val normalized = name.substringAfterLast('/').substringBeforeLast('.')
        for (type in listOf("drawable", "mipmap")) {
            val resId = context.resources.getIdentifier(normalized, type, context.packageName)
            if (resId != 0) return resId
        }
        return null
    }
}
