package app.tauri.backgroundservice

import android.content.Context

data class DurableState(
    val desiredRunning: Boolean = false,
    val lastServiceLabel: String = "",
    val lastServiceType: String = "dataSync",
    val lastStartEpochMs: Long = 0L,
    val lastHeartbeatEpochMs: Long = 0L,
    val lastNativeState: String = "unknown",
    val lastPlatformError: String? = null,
    val restartAttempt: Int = 0,
    val recoveryPending: Boolean = false,
    val recoveryReason: String? = null,
) {
    companion object {
        private const val PREFS_NAME = "tauri_bg_service_state"

        private const val KEY_DESIRED_RUNNING = "desired_running"
        private const val KEY_LAST_SERVICE_LABEL = "last_service_label"
        private const val KEY_LAST_SERVICE_TYPE = "last_service_type"
        private const val KEY_LAST_START_EPOCH_MS = "last_start_epoch_ms"
        private const val KEY_LAST_HEARTBEAT_EPOCH_MS = "last_heartbeat_epoch_ms"
        private const val KEY_LAST_NATIVE_STATE = "last_native_state"
        private const val KEY_LAST_PLATFORM_ERROR = "last_platform_error"
        private const val KEY_RESTART_ATTEMPT = "restart_attempt"
        private const val KEY_RECOVERY_PENDING = "recovery_pending"
        private const val KEY_RECOVERY_REASON = "recovery_reason"

        fun load(context: Context): DurableState {
            val prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            return DurableState(
                desiredRunning = prefs.getBoolean(KEY_DESIRED_RUNNING, false),
                lastServiceLabel = prefs.getString(KEY_LAST_SERVICE_LABEL, "") ?: "",
                lastServiceType = prefs.getString(KEY_LAST_SERVICE_TYPE, "dataSync") ?: "dataSync",
                lastStartEpochMs = prefs.getLong(KEY_LAST_START_EPOCH_MS, 0L),
                lastHeartbeatEpochMs = prefs.getLong(KEY_LAST_HEARTBEAT_EPOCH_MS, 0L),
                lastNativeState = prefs.getString(KEY_LAST_NATIVE_STATE, "unknown") ?: "unknown",
                lastPlatformError = prefs.getString(KEY_LAST_PLATFORM_ERROR, null),
                restartAttempt = prefs.getInt(KEY_RESTART_ATTEMPT, 0),
                recoveryPending = prefs.getBoolean(KEY_RECOVERY_PENDING, false),
                recoveryReason = prefs.getString(KEY_RECOVERY_REASON, null),
            )
        }

        fun save(context: Context, state: DurableState) {
            val prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            prefs.edit()
                .putBoolean(KEY_DESIRED_RUNNING, state.desiredRunning)
                .putString(KEY_LAST_SERVICE_LABEL, state.lastServiceLabel)
                .putString(KEY_LAST_SERVICE_TYPE, state.lastServiceType)
                .putLong(KEY_LAST_START_EPOCH_MS, state.lastStartEpochMs)
                .putLong(KEY_LAST_HEARTBEAT_EPOCH_MS, state.lastHeartbeatEpochMs)
                .putString(KEY_LAST_NATIVE_STATE, state.lastNativeState)
                .putString(KEY_LAST_PLATFORM_ERROR, state.lastPlatformError)
                .putInt(KEY_RESTART_ATTEMPT, state.restartAttempt)
                .putBoolean(KEY_RECOVERY_PENDING, state.recoveryPending)
                .putString(KEY_RECOVERY_REASON, state.recoveryReason)
                .apply()
        }

        fun clear(context: Context) {
            val prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            prefs.edit().clear().apply()
        }
    }
}
