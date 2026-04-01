use serde::Serialize;
use tauri::{
    plugin::{PluginApi, PluginHandle},
    AppHandle, Runtime,
};

use crate::models::{AutoStartConfig, StartConfig};

/// Rust-side bridge to native mobile keepalive code.
///
/// Only compiled on mobile targets (`#[cfg(mobile)]` in lib.rs).
/// Calls through to Kotlin (Android) and Swift (iOS) via `run_mobile_plugin`.
pub struct MobileLifecycle<R: Runtime> {
    pub handle: PluginHandle<R>,
}

/// Arguments sent to the native `startKeepalive` handler.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StartKeepaliveArgs<'a> {
    label: &'a str,
    foreground_service_type: &'a str,
}

impl<R: Runtime> MobileLifecycle<R> {
    /// Start the OS-specific keepalive mechanism.
    ///
    /// - Android: starts a Foreground Service with `label` as notification text.
    /// - iOS: schedules a `BGAppRefreshTask`.
    pub fn start_keepalive(&self, label: &str, foreground_service_type: &str) -> Result<(), tauri::Error> {
        self.handle
            .run_mobile_plugin::<()>("startKeepalive", StartKeepaliveArgs { label, foreground_service_type })?;
        Ok(())
    }

    /// Stop the OS-specific keepalive mechanism.
    ///
    /// - Android: stops the Foreground Service.
    /// - iOS: cancels the scheduled background task.
    pub fn stop_keepalive(&self) -> Result<(), tauri::Error> {
        self.handle.run_mobile_plugin::<()>("stopKeepalive", ())?;
        Ok(())
    }

    /// Notify the native layer that the background service's `run()` completed.
    ///
    /// - iOS: calls `setTaskCompleted` on the stored BGTask and schedules the next one.
    pub fn complete_bg_task(&self, success: bool) -> Result<(), tauri::Error> {
        self.handle
            .run_mobile_plugin::<()>("completeBgTask", CompleteBgTaskArgs { success })?;
        Ok(())
    }

    /// Block until the native layer signals cancellation (e.g. iOS expiration handler).
    ///
    /// Uses the Pending Invoke pattern — the native side stores the Invoke without
    /// resolving it, which blocks this thread via `run_mobile_plugin`'s `rx.recv()`.
    /// When the expiration handler fires, it resolves the Invoke, unblocking this call.
    pub fn wait_for_cancel(&self) -> Result<(), tauri::Error> {
        self.handle.run_mobile_plugin::<()>("waitForCancel", ())?;
        Ok(())
    }

    /// Check if the service was auto-started by OS restart.
    ///
    /// Reads auto-start config from SharedPreferences via the Kotlin bridge.
    /// Returns `Some(StartConfig)` if auto-start is pending and a label is available.
    pub fn get_auto_start_config(&self) -> Result<Option<StartConfig>, tauri::Error> {
        let config: AutoStartConfig = self
            .handle
            .run_mobile_plugin("getAutoStartConfig", ())?;
        Ok(config.into_start_config())
    }

    /// Clear the auto-start flag after processing.
    ///
    /// Called from the plugin setup closure after auto-start has been handled.
    pub fn clear_auto_start_config(&self) -> Result<(), tauri::Error> {
        self.handle
            .run_mobile_plugin::<()>("clearAutoStartConfig", ())?;
        Ok(())
    }

    /// Move the Activity to background after auto-start.
    ///
    /// Hides the briefly-visible Activity that was launched by the OS restart.
    pub fn move_task_to_background(&self) -> Result<(), tauri::Error> {
        self.handle
            .run_mobile_plugin::<()>("moveTaskToBackground", ())?;
        Ok(())
    }
}

/// Arguments sent to the native `completeBgTask` handler.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteBgTaskArgs {
    success: bool,
}

/// Canonical Tauri v2 mobile init function.
///
/// Registers the plugin with the appropriate native layer:
/// - Android: `app.tauri.backgroundservice.BackgroundServicePlugin`
/// - iOS: uses the `init_plugin_background_service` binding macro
pub fn init<R: Runtime, C: serde::de::DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> Result<MobileLifecycle<R>, tauri::Error> {
    #[cfg(target_os = "android")]
    let handle = api.register_android_plugin("app.tauri.backgroundservice", "BackgroundServicePlugin")?;
    #[cfg(target_os = "ios")]
    let handle = api.register_ios_plugin(init_plugin_background_service)?;
    Ok(MobileLifecycle { handle })
}
