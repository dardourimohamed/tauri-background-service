package app.tauri.backgroundservice

import android.content.Context

interface CoreBridge {
    fun start(context: Context, reason: String): HeadlessCoreResult
    fun stop(context: Context, reason: String): HeadlessCoreResult

    /**
     * Nudge Core connectivity after an OS network change (spec01 D4).
     * May throw [UnsatisfiedLinkError] when the installed native lib predates
     * the export — callers must catch it and keep the service running.
     */
    fun notifyNetworkChanged(): HeadlessCoreResult
}
