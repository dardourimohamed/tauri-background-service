package app.tauri.backgroundservice

import android.content.Context

class HeadlessBridgeImpl : CoreBridge {
    override fun start(context: Context, reason: String): HeadlessBridgeResult {
        return HeadlessBridge.start(context, reason)
    }

    override fun stop(context: Context, reason: String): HeadlessBridgeResult {
        return HeadlessBridge.stop(context, reason)
    }

    override fun notifyNetworkChanged(): HeadlessBridgeResult {
        return HeadlessBridge.networkChanged()
    }
}
