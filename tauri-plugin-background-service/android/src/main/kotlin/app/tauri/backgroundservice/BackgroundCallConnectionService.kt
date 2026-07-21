package app.tauri.backgroundservice

import android.content.ComponentName
import android.content.Context
import android.content.pm.ServiceInfo
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.os.Build
import android.os.Bundle
import android.telecom.CallAudioState
import android.telecom.Connection
import android.telecom.ConnectionRequest
import android.telecom.ConnectionService
import android.telecom.DisconnectCause
import android.telecom.PhoneAccount
import android.telecom.PhoneAccountHandle
import android.telecom.TelecomManager
import android.util.Log
import androidx.annotation.RequiresApi
import androidx.annotation.VisibleForTesting
import java.util.concurrent.ConcurrentHashMap

/**
 * Self-managed Telecom [ConnectionService] (spec 08 C6, Step 15).
 *
 * Registering a self-managed `PhoneAccount` lets the OS route audio focus,
 * Bluetooth/IO device switching, and the system call sheet through the
 * platform call pipeline while the webview is closed — i.e. the system call UI
 * a user expects on Android. The actual call media (signaling + encoded frames)
 * is owned by the Rust/Iroh call stack; this class is the OS integration seam.
 *
 * `MANAGE_OWN_CALLS` is declared in the manifest. The account is registered as
 * `CAPABILITY_SELF_MANAGED` (API 26+), which makes the system defer its own
 * in-call UI to ours and lets us place/answer calls via `TelecomManager`.
 *
 * Physical on-device execution (placing a real call, BT routing verification)
 * is the runbook half (plan §Step 20); the in-repo class is the compilable,
 * manifest-registered seam.
 */
class BackgroundCallConnectionService : ConnectionService() {

    companion object {
        private const val TAG = "BackgroundCallConnectionService"
        const val PHONE_ACCOUNT_ID = "bg_self_managed_call"

        /**
         * Register the self-managed phone account (idempotent). Call once during
         * app/plugin startup, before any call is placed.
         */
        @JvmStatic
        fun registerPhoneAccount(context: Context) {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
            val tm = context.getSystemService(Context.TELECOM_SERVICE) as TelecomManager
            val handle = phoneAccountHandle(context)
            val account = PhoneAccount.builder(
                handle,
                context.applicationInfo.loadLabel(context.packageManager),
            )
                .setCapabilities(PhoneAccount.CAPABILITY_SELF_MANAGED)
                .setHighlightColor(0)
                .setShortDescription("Background calls")
                .build()
            try {
                tm.registerPhoneAccount(account)
                Log.i(TAG, "Registered self-managed phone account")
            } catch (e: SecurityException) {
                Log.w(TAG, "Cannot register phone account (MANAGE_OWN_CALLS?): ${e.message}")
            }
        }

        fun phoneAccountHandle(context: Context): PhoneAccountHandle {
            val componentName = ComponentName(context, BackgroundCallConnectionService::class.java)
            return PhoneAccountHandle(componentName, PHONE_ACCOUNT_ID)
        }

        /**
         * Resolve the Rust call session key a Telecom [ConnectionRequest] carries
         * (M-NATIVE-1, Step 9). The [addNewIncomingCall] issuer (Step 11) puts the
         * `call_id` in the request extras under [IncomingCallNotifier.EXTRA_CALL_ID];
         * `onAnswer`/`onReject` route to the core for this id. Empty when absent
         * (the notification-broadcast route remains a parallel answer/decline
         * binding).
         *
         * `addNewIncomingCall(handle, extras)` nests the connection-service extras
         * under [TelecomManager.EXTRA_INCOMING_CALL_EXTRAS] on real Android, while
         * Robolectric copies the extras verbatim (top-level) — read both so the
         * binding holds in production AND under the host gate.
         */
        @VisibleForTesting
        @JvmStatic
        fun callIdFromRequest(request: ConnectionRequest): String {
            val extras = request.extras ?: return ""
            extras.getBundle(TelecomManager.EXTRA_INCOMING_CALL_EXTRAS)
                ?.getString(IncomingCallNotifier.EXTRA_CALL_ID)
                ?.takeIf { it.isNotEmpty() }
                ?.let { return it }
            return extras.getString(IncomingCallNotifier.EXTRA_CALL_ID).orEmpty()
        }

        // ── M-NATIVE-3 (Step 11): DRIVE the registered self-managed account ──
        //
        // Step 9 wired every Connection callback + the full audio-focus lifecycle,
        // but nothing issued `addNewIncomingCall`, so the OS never created a
        // `BackgroundCallConnection` and the focus path was dead. Step 11 issues
        // it so the platform call pipeline (audio focus, MODE_IN_COMMUNICATION,
        // BT routing) engages for a real inbound call. (AND-02: the symmetric
        // outbound `placeCall`/`onCreateOutgoingConnection` path was unreachable —
        // outbound calls are core-initiated via `start_call` — and has been
        // removed; inbound-only Telecom is retained.)

        /**
         * Issue [TelecomManager.addNewIncomingCall] for an inbound offer so the OS
         * creates a [BackgroundCallConnection] through [onCreateIncomingConnection]. The
         * `call_id` rides the extras (top-level + nested under
         * [TelecomManager.EXTRA_INCOMING_CALL_EXTRAS]) so `onAnswer`/`onReject`
         * (Step 9) and the audio-focus lifecycle bind to it. Idempotent-safe,
         * API26+-guarded, `SecurityException`-safe (MANAGE_OWN_CALLS).
         */
        @JvmStatic
        fun addNewIncomingCall(context: Context, callId: String, isVideo: Boolean) {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
            if (callId.isEmpty()) return
            val tm = context.getSystemService(Context.TELECOM_SERVICE) as TelecomManager
            val handle = phoneAccountHandle(context)
            val csExtras = Bundle().apply {
                putString(IncomingCallNotifier.EXTRA_CALL_ID, callId)
                putBoolean(EXTRA_HAS_VIDEO, isVideo)
            }
            val extras = Bundle().apply {
                // Top-level (Robolectric copies verbatim into the ConnectionRequest).
                putString(IncomingCallNotifier.EXTRA_CALL_ID, callId)
                // Nested (the real-Android channel onCreateIncomingConnection reads).
                putBundle(TelecomManager.EXTRA_INCOMING_CALL_EXTRAS, csExtras)
            }
            try {
                tm.addNewIncomingCall(handle, extras)
                Log.i(TAG, "addNewIncomingCall: callId=$callId, video=$isVideo")
            } catch (e: SecurityException) {
                Log.w(TAG, "Cannot add incoming call (MANAGE_OWN_CALLS?): ${e.message}")
            }
        }

        /**
         * Process-level registry of the live [BackgroundCallConnection] per `call_id`,
         * so the notification-answer path ([CallActionReceiver], the primary answer
         * surface) and the route command ([setCallAudioRoute]) can reach the OS
         * connection the OS created in [onCreateIncomingConnection]. Populated on
         * connection create, cleared on disconnect.
         */
        private val liveConnections = ConcurrentHashMap<String, BackgroundCallConnection>()

        @VisibleForTesting
        internal fun registerConnection(callId: String, connection: BackgroundCallConnection) {
            if (callId.isNotEmpty()) liveConnections[callId] = connection
        }

        @VisibleForTesting
        internal fun unregisterConnection(callId: String) {
            if (callId.isNotEmpty()) liveConnections.remove(callId)
        }

        @VisibleForTesting
        internal fun liveConnectionCount(): Int = liveConnections.size

        @VisibleForTesting
        internal fun clearLiveConnectionsForTest() = liveConnections.clear()

        /**
         * Drive the live Telecom connection for `call_id` to ACTIVE — bridges the
         * notification-answer path (the broadcast [CallActionReceiver] answer, our
         * primary answer surface) to the OS connection so its
         * `onStateChanged(STATE_ACTIVE)` engages audio focus + `MODE_IN_COMMUNICATION`.
         * No-op when no connection is live (e.g. Telecom drive unavailable / answered
         * before the OS created the connection).
         */
        @JvmStatic
        fun markCallActive(callId: String) {
            liveConnections[callId]?.setActive()
        }

        /**
         * Apply a device audio route to the live self-managed connection
         * (M-NATIVE-3 / CCF-11). `Connection.setAudioRoute` is the correct
         * self-managed routing API (no `MODIFY_AUDIO_SETTINGS`). The physical route
         * switch is device-verified; the pure route-string → [CallAudioState] route
         * mapping ([audioRouteFor]) is the host-testable seam. No-op for `system`
         * (let the platform decide) or when no connection is live.
         */
        @JvmStatic
        fun setCallAudioRoute(callId: String, route: String) {
            val connection = liveConnections[callId] ?: run {
                Log.w(TAG, "setCallAudioRoute: no live connection for callId=$callId")
                return
            }
            val routeInt = audioRouteFor(route) ?: run {
                Log.i(TAG, "setCallAudioRoute: route=$route is platform-managed (no override)")
                return
            }
            try {
                connection.setAudioRoute(routeInt)
                Log.i(TAG, "setCallAudioRoute: callId=$callId, route=$route")
            } catch (e: Exception) {
                Log.w(TAG, "setCallAudioRoute failed: ${e.message}")
            }
        }

        /**
         * Pure route-string → [CallAudioState] route mapping (the host-testable
         * seam of [setCallAudioRoute]). `system` (and any unknown value) → `null`
         * = let the platform manage the route (no override).
         */
        @VisibleForTesting
        @JvmStatic
        fun audioRouteFor(route: String): Int? = when (route) {
            "speaker" -> CallAudioState.ROUTE_SPEAKER
            "earpiece" -> CallAudioState.ROUTE_EARPIECE
            "bluetooth" -> CallAudioState.ROUTE_BLUETOOTH
            else -> null // "system" / unknown → platform-managed
        }

        /** Extras key for the offer's video flag carried into the connection. */
        const val EXTRA_HAS_VIDEO = "bg_service.has_video"

        @VisibleForTesting
        fun foregroundServiceType(): Int {
            // The phoneCall FGS type the service is promoted to while a call
            // is active (LifecycleService maps the string to this constant).
            return ServiceInfo.FOREGROUND_SERVICE_TYPE_PHONE_CALL
        }
    }

    @VisibleForTesting
    internal var connectionFactory: (PhoneAccountHandle, ConnectionRequest) -> Connection =
        { handle, request -> BackgroundCallConnection(this, handle, request) }

    override fun onCreateIncomingConnection(
        connectionManagerPhoneAccount: PhoneAccountHandle?,
        request: ConnectionRequest,
    ): Connection {
        Log.i(TAG, "onCreateIncomingConnection: ${request.address}")
        val handle = connectionManagerPhoneAccount
            ?: phoneAccountHandle(applicationContext)
        val connection = connectionFactory(handle, request)
        configureSelfManaged(connection)
        // Track the live connection so the notification-answer path (markCallActive)
        // and the route command (setCallAudioRoute) can reach it (Step 11).
        (connection as? BackgroundCallConnection)?.let { registerConnection(it.callId, it) }
        connection.setRinging()
        return connection
    }

    private fun configureSelfManaged(connection: Connection) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            connection.setConnectionProperties(Connection.PROPERTY_SELF_MANAGED)
        }
        connection.setConnectionCapabilities(
            Connection.CAPABILITY_MUTE or Connection.CAPABILITY_SUPPORT_HOLD
        )
        // Route audio through the VoIP audio mode (speaker/BT handled by the
        // platform call audio router once the connection is active).
        connection.setAudioModeIsVoip(true)
    }

    /**
     * The self-managed connection for one call. Owns audio focus for the
     * `VOICE_COMMUNICATION` stream and releases it on disconnect.
     */
    @RequiresApi(Build.VERSION_CODES.M)
    class BackgroundCallConnection(
        private val context: Context,
        @Suppress("UNUSED_PARAMETER") handle: PhoneAccountHandle,
        request: ConnectionRequest,
    ) : Connection() {

        private val audioManager =
            context.getSystemService(Context.AUDIO_SERVICE) as AudioManager
        private var focusRequest: AudioFocusRequest? = null

        /** The Rust call session key this Telecom connection drives (Step 9). */
        @VisibleForTesting
        internal val callId: String = callIdFromRequest(request)

        // M-NATIVE-1 (Step 9): the system call sheet's Answer/Reject drive the
        // Rust control plane via the same injectable seam as the notification
        // broadcast route. Bound to THIS connection's call_id. (Telecom STATE
        // driving — addNewIncomingCall, setActive on answer, audio-focus keep —
        // is Step 11; Step 9 only routes the action to the core.)
        override fun onAnswer() {
            super.onAnswer()
            if (callId.isNotEmpty()) CallActionDispatch.dispatcher.answerCall(context, callId)
            // Step 11: drive the connection ACTIVE on answer so onStateChanged
            // (STATE_ACTIVE) engages audio focus + MODE_IN_COMMUNICATION. (The
            // notification-answer path reaches the same setActive via markCallActive.)
            setActive()
        }

        override fun onReject() {
            super.onReject()
            if (callId.isNotEmpty()) CallActionDispatch.dispatcher.rejectCall(context, callId)
        }

        // android.telecom.Connection signals lifecycle via onStateChanged(int)
        // (there is no onActive/onRinging override). Request VOICE_COMMUNICATION
        // audio focus while the call is ACTIVE; release on DISCONNECTED.
        override fun onStateChanged(state: Int) {
            super.onStateChanged(state)
            when (state) {
                Connection.STATE_ACTIVE -> {
                    requestCallAudioFocus()
                    audioManager.mode = AudioManager.MODE_IN_COMMUNICATION
                }
                Connection.STATE_DISCONNECTED -> {
                    releaseCallAudioFocus()
                    audioManager.mode = AudioManager.MODE_NORMAL
                    // Step 11: drop the live-connection registry entry on any
                    // disconnect path (local hang-up, abort, remote end).
                    unregisterConnection(callId)
                }
            }
        }

        override fun onDisconnect() {
            super.onDisconnect()
            // Local hang-up via the system call sheet → end_call (Step 9).
            if (callId.isNotEmpty()) CallActionDispatch.dispatcher.endCall(context, callId)
            setDisconnected(DisconnectCause(DisconnectCause.LOCAL))
            releaseCallAudioFocus()
            audioManager.mode = AudioManager.MODE_NORMAL
            destroy()
        }

        override fun onAbort() {
            super.onAbort()
            // Aborted before connect → reject_call (Step 9).
            if (callId.isNotEmpty()) CallActionDispatch.dispatcher.rejectCall(context, callId)
            setDisconnected(DisconnectCause(DisconnectCause.CANCELED))
            releaseCallAudioFocus()
            audioManager.mode = AudioManager.MODE_NORMAL
            destroy()
        }

        private fun requestCallAudioFocus() {
            val attrs = AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION)
                .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                .build()
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                val request = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN_TRANSIENT)
                    .setAudioAttributes(attrs)
                    .setAcceptsDelayedFocusGain(false)
                    .build()
                focusRequest = request
                audioManager.requestAudioFocus(request)
            } else {
                @Suppress("DEPRECATION")
                audioManager.requestAudioFocus(null, AudioManager.STREAM_VOICE_CALL,
                    AudioManager.AUDIOFOCUS_GAIN_TRANSIENT)
            }
        }

        private fun releaseCallAudioFocus() {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                focusRequest?.let { audioManager.abandonAudioFocusRequest(it) }
                focusRequest = null
            } else {
                @Suppress("DEPRECATION")
                audioManager.abandonAudioFocus(null)
            }
        }
    }
}
