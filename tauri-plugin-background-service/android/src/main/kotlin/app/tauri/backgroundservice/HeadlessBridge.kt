package app.tauri.backgroundservice

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import org.json.JSONObject
import java.io.File
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import java.util.concurrent.TimeoutException

data class HeadlessBridgeResult(
    val ok: Boolean,
    val state: String,
    val message: String?,
    // NTF-04 (Step 7a): cross-JNI recoverable/permanent discriminator surfaced
    // from the Rust HeadlessCoreReport. 7b consumes this to decide re-present
    // (recoverable) vs cancel (permanent) on a failed notification action.
    val recoverable: Boolean,
    val rawJson: String,
) {
    val accepted: Boolean
        get() = ok && (state == "running" || state == "setup_idle" || state == "locked_idle")

    companion object {
        fun fromJson(json: String): HeadlessBridgeResult {
            return try {
                val obj = JSONObject(json)
                HeadlessBridgeResult(
                    ok = obj.optBoolean("ok", false),
                    state = obj.optString("state", "failed"),
                    message = obj.optString("message").ifEmpty { null },
                    recoverable = obj.optBoolean("recoverable", false),
                    rawJson = json,
                )
            } catch (e: Exception) {
                failure("invalid_headless_core_response", e.message ?: e.javaClass.simpleName)
            }
        }

        fun failure(code: String, message: String): HeadlessBridgeResult {
            val json = JSONObject().apply {
                put("ok", false)
                put("state", "failed")
                put("code", code)
                put("message", message)
                put("recoverable", true)
            }.toString()
            return HeadlessBridgeResult(false, "failed", message, true, json)
        }
    }
}

object HeadlessBridge {
    @Volatile private var loaded = false
    @Volatile private var loadError: String? = null

    /**
     * Name of the host app's native (Rust cdylib) core to dlopen for headless
     * lifecycle / call / message bridging. The plugin ships NO native library;
     * a host app that bridges to a native core sets this
     * (`HeadlessBridge.nativeLibName = "app_core"`) AND exports the JNI symbols
     * `Java_app_tauri_backgroundservice_HeadlessBridge_*`
     * (`startCore`/`stopCore`/`notifyNetworkChanged`/`callAction`/`notificationAction`).
     *
     * When the library is absent — the default for consumers that don't bridge
     * to a native core — [ensureLoaded] fails gracefully and every entry point
     * returns a typed `native_library_load_failed` result; the lifecycle-only
     * path (foreground service + Rust `BackgroundService<R>` task) is unaffected.
     */
    @Volatile var nativeLibName: String = "app_core"

    // PRODUCT DECISION: headless message notifications carry a generic body (doc
    // 08, BGS-07). The webview/JS i18n catalog is absent on the force-stopped /
    // closed-webview boot, and the message body must not cross the JNI boundary
    // (non-payload, settled design §10); the sender is shown as the title.
    private const val HEADLESS_MESSAGE_BODY = "New message"

    @JvmStatic private external fun startCore(dataDir: String, reason: String): String
    @JvmStatic private external fun stopCore(dataDir: String, reason: String): String
    @JvmStatic private external fun notifyNetworkChanged(): String

    // M-NATIVE-1 (Step 9): native ring Answer/Decline → in-process Rust control
    // plane. `action` is "answer"/"reject"/"end"; routed to
    // core.answer_call/reject_call/end_call for `callId`. The notification
    // BroadcastReceiver and Telecom onAnswer/onReject reach this without a webview.
    @JvmStatic private external fun callAction(callId: String, action: String): String
    @JvmStatic
    private external fun notificationAction(
        // NTF-04 (Step 7a): dataDir first, mirroring startCore — a locked
        // notification action attempts a headless bring-up before failing.
        dataDir: String,
        action: String,
        chatId: String,
        messageId: String,
        replyText: String,
    ): String

    fun start(context: Context, reason: String): HeadlessBridgeResult {
        ensureLoaded()?.let { return HeadlessBridgeResult.failure("native_library_load_failed", it) }
        val dataDir = dataDir(context)
        if (!ensureDataDir(dataDir)) {
            return HeadlessBridgeResult.failure(
                "data_dir_unavailable",
                "Failed to create data directory: ${dataDir.absolutePath}",
            )
        }
        return HeadlessBridgeResult.fromJson(startCore(dataDir.absolutePath, reason))
    }

    fun stop(context: Context, reason: String): HeadlessBridgeResult {
        ensureLoaded()?.let { return HeadlessBridgeResult.failure("native_library_load_failed", it) }
        return HeadlessBridgeResult.fromJson(stopCore(dataDir(context).absolutePath, reason))
    }

    // Named networkChanged because the external above already takes the
    // notifyNetworkChanged JNI name (the symbol is the Rust-side contract).
    // May throw UnsatisfiedLinkError when the loaded lib predates the export.
    fun networkChanged(): HeadlessBridgeResult {
        ensureLoaded()?.let { return HeadlessBridgeResult.failure("native_library_load_failed", it) }
        return HeadlessBridgeResult.fromJson(notifyNetworkChanged())
    }

    /**
     * Drive a native ring action into the in-process headless Core (M-NATIVE-1,
     * Step 9). `action` is "answer"/"reject"/"end". Runs while the device is
     * locked / the webview is closed — the FGS-resident Core received the offer,
     * so the in-process JNI call reaches the same Core. Returns a report-shaped
     * result; a load failure or locked Core surfaces a typed recoverable failure
     * (never a crash) so the caller can log a diagnostic + terminal reason.
     *
     * Named `performCallAction` (not `callAction`) because the external above
     * already takes the `callAction` JNI name — the symbol is the Rust-side
     * contract (`Java_..._HeadlessBridge_callAction`), mirroring the
     * `notifyNetworkChanged`/`networkChanged` split.
     */
    fun performCallAction(callId: String, action: String): HeadlessBridgeResult {
        ensureLoaded()?.let { return HeadlessBridgeResult.failure("native_library_load_failed", it) }
        return HeadlessBridgeResult.fromJson(callAction(callId, action))
    }

    /**
     * Drive a native message-notification action (inline reply / mark-read) into
     * the in-process headless Core. `context` is resolved to the app data dir
     * internally (NTF-04, Step 7a — mirrors [start]) so a locked Core can
     * attempt a headless bring-up before failing; the caller (the action
     * dispatcher) already holds the context and needs no signature change beyond
     * passing it through. Returns a report-shaped result carrying the
     * recoverable discriminator 7b consumes to re-present vs cancel.
     */
    fun performNotificationAction(
        context: Context,
        action: String,
        chatId: String,
        messageId: String,
        replyText: String,
    ): HeadlessBridgeResult {
        ensureLoaded()?.let { return HeadlessBridgeResult.failure("native_library_load_failed", it) }
        val dataDir = dataDir(context)
        if (!ensureDataDir(dataDir)) {
            return HeadlessBridgeResult.failure(
                "data_dir_unavailable",
                "Failed to create data directory: ${dataDir.absolutePath}",
            )
        }
        return HeadlessBridgeResult.fromJson(
            notificationAction(dataDir.absolutePath, action, chatId, messageId, replyText)
        )
    }

    /**
     * Rust→Kotlin **upcall** target (Step 10, M-NATIVE-2 = NR-4): ring the
     * native incoming-call notification from the headless (webview-absent)
     * forwarder. The mirror of [performCallAction]'s downcall — invoked
     * in-process by the JNI `show_incoming_call_upcall` when a force-stopped /
     * closed-webview boot receives a `CoreEvent::IncomingCall` and no Tauri
     * activity/`AppHandle` exists. Resolves the process Application context
     * itself and delegates to [IncomingCallNotifier] (the same pure Android half
     * the plugin's `showIncomingCall` command uses). Best-effort: a missing
     * context logs and returns (never throws back across the JNI boundary).
     */
    @JvmStatic
    fun showIncomingCall(callId: String, callerName: String, hasVideo: Boolean) {
        val context = applicationContext()
        if (context == null) {
            android.util.Log.w("HeadlessBridge", "showIncomingCall: no application context")
            return
        }
        IncomingCallNotifier.showIncomingCall(
            context = context,
            callId = callId,
            callerName = callerName,
            isVideo = hasVideo,
            smallIcon = NotificationIconResolver.resolve(context),
            launchIntent = context.packageManager.getLaunchIntentForPackage(context.packageName),
        )
    }

    /**
     * Rust→Kotlin **upcall** (BGS-07, Step 10): post a native **message**
     * notification from the webview-absent forwarder — the mirror of
     * [showIncomingCall] for the message case. A message that arrives while the
     * webview is closed/force-stopped must not be silently stored with no
     * notification (the ring-only forwarder's gap). Resolves the process
     * Application context itself and delegates to [ActionableMessageNotifier].
     *
     * Diagnostics stay **non-payload** (settled design §10): only the routing
     * identity (`chatId`/`messageId` + `sender`) crosses the JNI boundary, never
     * the message body/media bytes — so the notification carries the sender as
     * title and a generic body, never the message text. Best-effort: a missing
     * context logs and returns (never throws back across JNI).
     */
    @JvmStatic
    fun showMessage(chatId: String, messageId: String, sender: String) {
        val context = applicationContext()
        if (context == null) {
            android.util.Log.w("HeadlessBridge", "showMessage: no application context")
            return
        }
        ActionableMessageNotifier.showMessageNotification(
            context = context,
            notificationId = ActionableMessageNotifier.notificationIdFor(chatId),
            chatId = chatId,
            messageId = messageId,
            // Non-payload: the sender routing identity is the only content that
            // crosses; the body stays generic (no message text is available here
            // and none may cross the JNI boundary).
            title = sender,
            body = HEADLESS_MESSAGE_BODY,
            routeUri = "bg-service://chat?chat_id=${Uri.encode(chatId)}&message_id=${Uri.encode(messageId)}",
            smallIcon = NotificationIconResolver.resolve(context),
            launchIntent = context.packageManager.getLaunchIntentForPackage(context.packageName),
        )
    }

    /**
     * Rust→Kotlin **upcall** (BGS-07, Step 10): cancel a stale CallStyle ring
     * for [callId] and revert the foreground-service type `phoneCall` →
     * `remoteMessaging` from the webview-absent forwarder — an abandoned/ended
     * incoming call must not leave the full-screen notification up forever.
     * Best-effort: a missing context logs and returns; the FGS revert is skipped
     * when no service is running (nothing to revert).
     */
    // CROSS-DOC: doc 04 owns call-control (ring lifecycle + FGS type).
    @JvmStatic
    fun cancelIncomingCall(callId: String) {
        val context = applicationContext()
        if (context == null) {
            android.util.Log.w("HeadlessBridge", "cancelIncomingCall: no application context")
            return
        }
        IncomingCallNotifier.cancel(context, callId)
        revertForegroundServiceType(context)
    }

    /**
     * Revert the running foreground service to the `remoteMessaging` baseline
     * after a call ends headlessly — the mirror of the plugin's
     * `updateForegroundServiceType` command, driven without a webview via an
     * [LifecycleService.ACTION_UPDATE_TYPE] intent. No-op when the service is not
     * running (nothing to revert). Best-effort: OS start restrictions are logged.
     */
    private fun revertForegroundServiceType(context: Context) {
        if (!LifecycleService.isRunning) return
        val intent = Intent(context, LifecycleService::class.java).apply {
            action = LifecycleService.ACTION_UPDATE_TYPE
            putExtra(LifecycleService.EXTRA_SERVICE_TYPE, "remoteMessaging")
        }
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        } catch (e: Throwable) {
            android.util.Log.w(
                "HeadlessBridge",
                "cancelIncomingCall FGS revert failed: ${e.message}",
            )
        }
    }

    /**
     * The process Application context via `ActivityThread.currentApplication()`
     * — works in the FGS / cold-boot process where there is no Tauri activity,
     * and under Robolectric. Best-effort: null when unavailable.
     */
    private fun applicationContext(): Context? = try {
        val activityThread = Class.forName("android.app.ActivityThread")
        activityThread.getMethod("currentApplication").invoke(null) as? Context
    } catch (e: Throwable) {
        null
    }

    private fun ensureLoaded(): String? {
        if (loaded) return null
        synchronized(this) {
            if (loaded) return null
            loadError?.let { return it }
            return try {
                System.loadLibrary(nativeLibName)
                loaded = true
                null
            } catch (e: Throwable) {
                val message = e.message ?: e.javaClass.simpleName
                loadError = message
                message
            }
        }
    }

    private fun dataDir(context: Context): File =
        File(context.applicationInfo.dataDir, "data")

    private fun ensureDataDir(dir: File): Boolean =
        dir.exists() || dir.mkdirs()
}

object ServiceStartAckRegistry {
    data class Ack(val success: Boolean, val payload: String)

    private val pending = ConcurrentHashMap<String, CompletableFuture<Ack>>()

    fun register(id: String) {
        pending[id] = CompletableFuture()
    }

    fun complete(id: String?, success: Boolean, payload: String) {
        if (id == null) return
        pending.remove(id)?.complete(Ack(success, payload))
    }

    fun forget(id: String) {
        pending.remove(id)
    }

    fun await(id: String, timeoutMs: Long): Ack {
        val future = pending[id]
            ?: return Ack(false, errorJson("ack_missing", "No pending start acknowledgement"))
        return try {
            future.get(timeoutMs, TimeUnit.MILLISECONDS)
        } catch (_: TimeoutException) {
            Ack(false, errorJson("ack_timeout", "Timed out waiting for foreground service startup"))
        } catch (e: Exception) {
            Ack(false, errorJson("ack_failed", e.message ?: e.javaClass.simpleName))
        } finally {
            pending.remove(id)
        }
    }

    fun errorJson(code: String, message: String): String {
        return JSONObject().apply {
            put("ok", false)
            put("state", "failed")
            put("code", code)
            put("message", message)
            put("recoverable", true)
        }.toString()
    }
}
