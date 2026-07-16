package app.tauri.backgroundservice

import android.content.Context

class HeadlessCoreBridgeImpl : CoreBridge {
    override fun start(context: Context, reason: String): HeadlessCoreResult {
        return HeadlessCoreBridge.start(context, reason)
    }

    override fun stop(context: Context, reason: String): HeadlessCoreResult {
        return HeadlessCoreBridge.stop(context, reason)
    }

    override fun notifyNetworkChanged(): HeadlessCoreResult {
        return HeadlessCoreBridge.networkChanged()
    }
}
