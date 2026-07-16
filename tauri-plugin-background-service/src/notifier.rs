//! Thin wrapper around [`tauri_plugin_notification`] for fire-and-forget
//! local notifications.
//!
//! Errors are logged but never propagated — callers should not need to
//! handle notification failures.

use crate::models::PluginConfig;
use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::NotificationExt;

/// Thin wrapper over `tauri-plugin-notification`.
///
/// Fire-and-forget: errors are logged via `log::warn!` and never propagated.
#[derive(Clone)]
pub struct Notifier<R: Runtime> {
    pub(crate) app: AppHandle<R>,
}

impl<R: Runtime> Notifier<R> {
    /// Show a local notification with the given title and body.
    ///
    /// Errors are logged but not returned — callers should not need to
    /// handle notification failures.
    pub fn show(&self, title: &str, body: &str) {
        if let Err(e) = self
            .app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
        {
            log::warn!("background-service: notification failed: {e}");
        }
    }

    /// Show a local notification with a stable string id.
    ///
    /// Repeated notifications with the same id replace the previous one
    /// instead of stacking (platform-dependent best effort). Same warn-only
    /// contract as [`Notifier::show`]: errors are logged, never propagated.
    pub fn show_with_id(&self, id: &str, title: &str, body: &str) {
        if let Err(e) = self
            .app
            .notification()
            .builder()
            .id(stable_notification_id(id))
            .title(title)
            .body(body)
            .show()
        {
            log::warn!("background-service: notification {id} failed: {e}");
        }
    }
}

/// Map a string notification id onto the `i32` id the notification builder
/// expects, deterministically (FNV-1a 32-bit), so the same string id keeps
/// replacing the same notification across calls and process restarts.
pub(crate) fn stable_notification_id(id: &str) -> i32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in id.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash as i32
}

/// Which plugin-side lifecycle notifications are enabled (spec 01 D1).
///
/// Derived once from [`PluginConfig`] at actor spawn via
/// [`NotifierPolicy::derive`]; the default is everything off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NotifierPolicy {
    /// Notify when the OS pauses background delivery (timeout/expiration).
    pub on_timeout: bool,
    /// Notify when background delivery is restored after OS restart/boot.
    pub on_recovery: bool,
}

impl NotifierPolicy {
    /// Derive the effective policy from config and platform (DEC-002).
    ///
    /// Pure function so the Android suppression matrix is host-testable;
    /// the call site passes `cfg!(target_os = "android")`.
    ///
    /// Android suppression rules:
    /// - `on_timeout` is forced off when `androidOnTimeout == "notifyUser"`,
    ///   because the Kotlin service already posts a native timeout
    ///   notification on that path.
    /// - `on_recovery` is forced off unconditionally, because the native
    ///   BootReceiver recovery notification path is always active on Android.
    pub fn derive(config: &PluginConfig, is_android: bool) -> Self {
        let native_owns_timeout = is_android && config.android_on_timeout == "notifyUser";
        Self {
            on_timeout: config.notify_on_timeout && !native_owns_timeout,
            on_recovery: config.notify_on_recovery && !is_android,
        }
    }
}

/// Dispatch seam for lifecycle notifications.
///
/// The manager actor talks to this trait instead of [`Notifier`] directly so
/// tests can record notifications without a running Tauri app (the spec's
/// test plan forbids `show()` calls in tests). The production sink is
/// [`Notifier`] itself.
pub trait NotifySink: Send + Sync {
    /// Post a notification with replace-not-stack semantics for `id`.
    fn notify(&self, id: &str, title: &str, body: &str);
}

impl<R: Runtime> NotifySink for Notifier<R> {
    fn notify(&self, id: &str, title: &str, body: &str) {
        self.show_with_id(id, title, body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PluginConfig;

    /// Compile-time test: Notifier can be constructed and cloned from an AppHandle.
    /// (Does not call show() because that requires a running Tauri app.)
    #[allow(dead_code)]
    fn notifier_clone_compiles<R: Runtime + Clone>(app: AppHandle<R>) {
        let n = Notifier { app };
        let _cloned = n.clone();
    }

    /// Compile-time test: show_with_id has the same warn-only, fire-and-forget
    /// shape as show(). (Not called — requires a running Tauri app.)
    #[allow(dead_code)]
    fn notifier_show_with_id_compiles<R: Runtime>(n: &Notifier<R>) {
        n.show_with_id("bg-timeout", "title", "body");
    }

    /// Compile-time test: Notifier is usable as a NotifySink trait object.
    #[allow(dead_code)]
    fn notifier_is_notify_sink<R: Runtime>(n: Notifier<R>) -> std::sync::Arc<dyn NotifySink> {
        std::sync::Arc::new(n)
    }

    #[test]
    fn stable_notification_id_is_deterministic() {
        assert_eq!(
            stable_notification_id("bg-timeout"),
            stable_notification_id("bg-timeout")
        );
        assert_ne!(
            stable_notification_id("bg-timeout"),
            stable_notification_id("bg-recovery")
        );
    }

    // ── NotifierPolicy::derive — DEC-002 suppression matrix ──────────

    fn config(
        notify_on_timeout: bool,
        notify_on_recovery: bool,
        android_on_timeout: &str,
    ) -> PluginConfig {
        PluginConfig {
            notify_on_timeout,
            notify_on_recovery,
            android_on_timeout: android_on_timeout.into(),
            ..Default::default()
        }
    }

    #[test]
    fn derive_desktop_honors_configured_keys() {
        let policy = NotifierPolicy::derive(&config(true, true, "notifyUser"), false);
        assert_eq!(
            policy,
            NotifierPolicy {
                on_timeout: true,
                on_recovery: true
            }
        );
    }

    #[test]
    fn derive_desktop_defaults_off() {
        let policy = NotifierPolicy::derive(&config(false, false, "notifyUser"), false);
        assert_eq!(
            policy,
            NotifierPolicy {
                on_timeout: false,
                on_recovery: false
            }
        );
    }

    #[test]
    fn derive_android_notify_user_suppresses_timeout() {
        // Kotlin LifecycleService already posts the native timeout
        // notification when androidOnTimeout == "notifyUser" (DEC-002).
        let policy = NotifierPolicy::derive(&config(true, true, "notifyUser"), true);
        assert_eq!(
            policy,
            NotifierPolicy {
                on_timeout: false,
                on_recovery: false
            }
        );
    }

    #[test]
    fn derive_android_stop_keeps_timeout() {
        // androidOnTimeout == "stop" posts no native notification, so the
        // plugin-side timeout notice is allowed; recovery stays suppressed.
        let policy = NotifierPolicy::derive(&config(true, true, "stop"), true);
        assert_eq!(
            policy,
            NotifierPolicy {
                on_timeout: true,
                on_recovery: false
            }
        );
    }

    #[test]
    fn derive_android_schedule_recovery_keeps_timeout() {
        let policy = NotifierPolicy::derive(&config(true, true, "scheduleRecovery"), true);
        assert_eq!(
            policy,
            NotifierPolicy {
                on_timeout: true,
                on_recovery: false
            }
        );
    }

    #[test]
    fn derive_android_always_suppresses_recovery() {
        // The Kotlin BootReceiver recovery notification path is always
        // active on Android and has no config switch (DEC-002).
        let policy = NotifierPolicy::derive(&config(false, true, "stop"), true);
        assert_eq!(
            policy,
            NotifierPolicy {
                on_timeout: false,
                on_recovery: false
            }
        );
    }

    #[test]
    fn derive_default_policy_is_all_off() {
        assert_eq!(
            NotifierPolicy::default(),
            NotifierPolicy {
                on_timeout: false,
                on_recovery: false
            }
        );
    }
}
