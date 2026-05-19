//! Platform capability reporting.
//!
//! [`CapabilityProvider`] builds [`PlatformCapabilities`]
//! per platform based on the plugin's knowledge of OS-specific background execution guarantees.
//! Each platform has different survival characteristics — the provider reports them honestly
//! without overpromising.

use crate::models::{LifecycleGuarantee, LifecycleMode, Platform, PlatformCapabilities};

/// Builds platform-specific background execution capabilities.
///
/// Exposes per-platform methods for direct testing and a unified
/// [`CapabilityProvider::capabilities`] entry point used by the
/// `get_platform_capabilities` Tauri command.
pub struct CapabilityProvider;

impl CapabilityProvider {
    /// Returns capabilities for Android.
    ///
    /// Android uses a foreground service (FGS) for background execution.
    pub fn android() -> PlatformCapabilities {
        PlatformCapabilities {
            platform: Platform::Android,
            lifecycle_mode: LifecycleMode::AndroidForegroundService,
            survives_app_close: LifecycleGuarantee::BestEffort,
            survives_reboot: LifecycleGuarantee::BestEffort,
            survives_force_quit: LifecycleGuarantee::Unsupported,
            background_execution: LifecycleGuarantee::Guaranteed,
            limitations: vec![
                "OEM battery optimization may kill foreground services".into(),
                "Force stop suppresses receivers and jobs until user launches app".into(),
                "Android 15 dataSync foreground service has 6-hour cumulative timeout per 24h window".into(),
                "Boot receiver cannot start dataSync FGS on Android 15+".into(),
            ],
            required_setup: vec![
                "FOREGROUND_SERVICE permission in manifest".into(),
                "Foreground service type and matching permission declared".into(),
                "Persistent notification channel configured".into(),
            ],
        }
    }

    /// Returns capabilities for iOS.
    ///
    /// iOS uses `BGTaskScheduler` for background execution.
    pub fn ios() -> PlatformCapabilities {
        PlatformCapabilities {
            platform: Platform::Ios,
            lifecycle_mode: LifecycleMode::IosBgTaskScheduler,
            survives_app_close: LifecycleGuarantee::BestEffort,
            survives_reboot: LifecycleGuarantee::BestEffort,
            survives_force_quit: LifecycleGuarantee::Unsupported,
            background_execution: LifecycleGuarantee::BestEffort,
            limitations: vec![
                "Cannot guarantee continuous background execution".into(),
                "Force-quit makes app ineligible for BGTask relaunch".into(),
                "BGAppRefreshTask has ~30s execution window".into(),
                "BGProcessingTask has variable execution window (minutes to hours)".into(),
            ],
            required_setup: vec![
                "UIBackgroundModes in Info.plist (background-fetch, background-processing)".into(),
                "BGTaskSchedulerPermittedIdentifiers in Info.plist".into(),
            ],
        }
    }

    /// Returns capabilities for desktop in-process mode.
    ///
    /// The service runs in the same process as the app.
    pub fn desktop_in_process(platform: Platform) -> PlatformCapabilities {
        PlatformCapabilities {
            platform,
            lifecycle_mode: LifecycleMode::DesktopInProcess,
            survives_app_close: LifecycleGuarantee::Unsupported,
            survives_reboot: LifecycleGuarantee::Unsupported,
            survives_force_quit: LifecycleGuarantee::Unsupported,
            background_execution: LifecycleGuarantee::Guaranteed,
            limitations: vec!["Service runs in-app process; terminates when app closes".into()],
            required_setup: vec![],
        }
    }

    /// Returns capabilities for desktop OS-service mode.
    ///
    /// When `installed_and_running` is `true`, survival guarantees reflect a
    /// properly configured OS service. When `false`, they fall back to
    /// `Unsupported` to indicate the service is not yet set up.
    pub fn desktop_os_service(
        platform: Platform,
        installed_and_running: bool,
    ) -> PlatformCapabilities {
        let (survives_close, survives_reboot, bg_exec) = if installed_and_running {
            (
                LifecycleGuarantee::Guaranteed,
                LifecycleGuarantee::Guaranteed,
                LifecycleGuarantee::Guaranteed,
            )
        } else {
            (
                LifecycleGuarantee::Unsupported,
                LifecycleGuarantee::Unsupported,
                LifecycleGuarantee::Unsupported,
            )
        };

        PlatformCapabilities {
            platform,
            lifecycle_mode: LifecycleMode::DesktopOsService,
            survives_app_close: survives_close,
            survives_reboot,
            survives_force_quit: LifecycleGuarantee::Unsupported,
            background_execution: bg_exec,
            limitations: vec!["Force quit kills the OS service".into()],
            required_setup: vec![
                "OS service must be installed and configured".into(),
                "Autostart must be enabled for reboot survival".into(),
            ],
        }
    }

    /// Detect the current platform and lifecycle mode based on cfg flags.
    ///
    /// For desktop, `desktop_service_mode` controls whether the mode is
    /// `DesktopInProcess` or `DesktopOsService`.
    pub fn detect_platform(desktop_service_mode: Option<&str>) -> (Platform, LifecycleMode) {
        #[cfg(target_os = "android")]
        {
            let _ = desktop_service_mode;
            (Platform::Android, LifecycleMode::AndroidForegroundService)
        }

        #[cfg(target_os = "ios")]
        {
            let _ = desktop_service_mode;
            (Platform::Ios, LifecycleMode::IosBgTaskScheduler)
        }

        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            let platform = Self::desktop_platform();
            let mode = match desktop_service_mode {
                Some("osService") => LifecycleMode::DesktopOsService,
                _ => LifecycleMode::DesktopInProcess,
            };
            (platform, mode)
        }
    }

    /// Determine the desktop platform from the current OS.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn desktop_platform() -> Platform {
        #[cfg(target_os = "linux")]
        {
            Platform::Linux
        }

        #[cfg(target_os = "macos")]
        {
            Platform::Macos
        }

        #[cfg(target_os = "windows")]
        {
            Platform::Windows
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            Platform::Unknown
        }
    }

    /// Build capabilities for the given platform, mode, and state.
    ///
    /// This is the main entry point for the `get_platform_capabilities` command.
    pub fn capabilities(
        platform: Platform,
        lifecycle_mode: LifecycleMode,
        os_service_installed: bool,
    ) -> PlatformCapabilities {
        match lifecycle_mode {
            LifecycleMode::AndroidForegroundService => Self::android(),
            LifecycleMode::IosBgTaskScheduler => Self::ios(),
            LifecycleMode::DesktopInProcess => Self::desktop_in_process(platform),
            LifecycleMode::DesktopOsService => {
                Self::desktop_os_service(platform, os_service_installed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Android capabilities ---

    #[test]
    fn android_correct_platform_and_mode() {
        let caps = CapabilityProvider::android();
        assert_eq!(caps.platform, Platform::Android);
        assert_eq!(caps.lifecycle_mode, LifecycleMode::AndroidForegroundService);
    }

    #[test]
    fn android_survives_app_close_best_effort() {
        assert_eq!(
            CapabilityProvider::android().survives_app_close,
            LifecycleGuarantee::BestEffort
        );
    }

    #[test]
    fn android_survives_reboot_best_effort() {
        assert_eq!(
            CapabilityProvider::android().survives_reboot,
            LifecycleGuarantee::BestEffort
        );
    }

    #[test]
    fn android_survives_force_quit_unsupported() {
        assert_eq!(
            CapabilityProvider::android().survives_force_quit,
            LifecycleGuarantee::Unsupported
        );
    }

    #[test]
    fn android_background_execution_guaranteed() {
        assert_eq!(
            CapabilityProvider::android().background_execution,
            LifecycleGuarantee::Guaranteed
        );
    }

    #[test]
    fn android_limitations_non_empty() {
        let caps = CapabilityProvider::android();
        assert!(!caps.limitations.is_empty());
        for l in &caps.limitations {
            assert!(!l.is_empty(), "limitation strings must not be empty");
        }
    }

    #[test]
    fn android_required_setup_non_empty() {
        let caps = CapabilityProvider::android();
        assert!(!caps.required_setup.is_empty());
    }

    // --- iOS capabilities ---

    #[test]
    fn ios_correct_platform_and_mode() {
        let caps = CapabilityProvider::ios();
        assert_eq!(caps.platform, Platform::Ios);
        assert_eq!(caps.lifecycle_mode, LifecycleMode::IosBgTaskScheduler);
    }

    #[test]
    fn ios_survives_app_close_best_effort() {
        assert_eq!(
            CapabilityProvider::ios().survives_app_close,
            LifecycleGuarantee::BestEffort
        );
    }

    #[test]
    fn ios_survives_reboot_best_effort() {
        assert_eq!(
            CapabilityProvider::ios().survives_reboot,
            LifecycleGuarantee::BestEffort
        );
    }

    #[test]
    fn ios_survives_force_quit_unsupported() {
        assert_eq!(
            CapabilityProvider::ios().survives_force_quit,
            LifecycleGuarantee::Unsupported
        );
    }

    #[test]
    fn ios_background_execution_best_effort() {
        assert_eq!(
            CapabilityProvider::ios().background_execution,
            LifecycleGuarantee::BestEffort
        );
    }

    #[test]
    fn ios_limitations_non_empty() {
        let caps = CapabilityProvider::ios();
        assert!(!caps.limitations.is_empty());
        for l in &caps.limitations {
            assert!(!l.is_empty());
        }
    }

    // --- Desktop in-process ---

    #[test]
    fn desktop_in_process_correct_mode() {
        let caps = CapabilityProvider::desktop_in_process(Platform::Linux);
        assert_eq!(caps.platform, Platform::Linux);
        assert_eq!(caps.lifecycle_mode, LifecycleMode::DesktopInProcess);
    }

    #[test]
    fn desktop_in_process_survives_app_close_unsupported() {
        assert_eq!(
            CapabilityProvider::desktop_in_process(Platform::Linux).survives_app_close,
            LifecycleGuarantee::Unsupported
        );
    }

    #[test]
    fn desktop_in_process_survives_reboot_unsupported() {
        assert_eq!(
            CapabilityProvider::desktop_in_process(Platform::Linux).survives_reboot,
            LifecycleGuarantee::Unsupported
        );
    }

    #[test]
    fn desktop_in_process_background_execution_guaranteed() {
        assert_eq!(
            CapabilityProvider::desktop_in_process(Platform::Linux).background_execution,
            LifecycleGuarantee::Guaranteed
        );
    }

    #[test]
    fn desktop_in_process_preserves_platform() {
        assert_eq!(
            CapabilityProvider::desktop_in_process(Platform::Linux).platform,
            Platform::Linux
        );
        assert_eq!(
            CapabilityProvider::desktop_in_process(Platform::Macos).platform,
            Platform::Macos
        );
        assert_eq!(
            CapabilityProvider::desktop_in_process(Platform::Windows).platform,
            Platform::Windows
        );
    }

    #[test]
    fn desktop_in_process_limitations_non_empty() {
        let caps = CapabilityProvider::desktop_in_process(Platform::Linux);
        assert!(
            !caps.limitations.is_empty(),
            "in-process limitations must not be empty"
        );
        for l in &caps.limitations {
            assert!(!l.is_empty(), "limitation strings must not be empty");
        }
    }

    // --- Desktop OS-service ---

    #[test]
    fn desktop_os_service_installed_reports_guaranteed() {
        let caps = CapabilityProvider::desktop_os_service(Platform::Linux, true);
        assert_eq!(caps.platform, Platform::Linux);
        assert_eq!(caps.lifecycle_mode, LifecycleMode::DesktopOsService);
        assert_eq!(caps.survives_app_close, LifecycleGuarantee::Guaranteed);
        assert_eq!(caps.survives_reboot, LifecycleGuarantee::Guaranteed);
        assert_eq!(caps.background_execution, LifecycleGuarantee::Guaranteed);
    }

    #[test]
    fn desktop_os_service_not_installed_reports_unsupported() {
        let caps = CapabilityProvider::desktop_os_service(Platform::Linux, false);
        assert_eq!(caps.survives_app_close, LifecycleGuarantee::Unsupported);
        assert_eq!(caps.survives_reboot, LifecycleGuarantee::Unsupported);
        assert_eq!(caps.background_execution, LifecycleGuarantee::Unsupported);
    }

    #[test]
    fn desktop_os_service_force_quit_always_unsupported() {
        assert_eq!(
            CapabilityProvider::desktop_os_service(Platform::Linux, true).survives_force_quit,
            LifecycleGuarantee::Unsupported
        );
        assert_eq!(
            CapabilityProvider::desktop_os_service(Platform::Linux, false).survives_force_quit,
            LifecycleGuarantee::Unsupported
        );
    }

    #[test]
    fn desktop_os_service_limitations_non_empty() {
        for installed in [true, false] {
            let caps = CapabilityProvider::desktop_os_service(Platform::Linux, installed);
            assert!(
                !caps.limitations.is_empty(),
                "os-service limitations must not be empty (installed={installed})"
            );
            for l in &caps.limitations {
                assert!(!l.is_empty(), "limitation strings must not be empty");
            }
        }
    }

    // --- capabilities() dispatch ---

    #[test]
    fn capabilities_dispatches_to_android() {
        let caps = CapabilityProvider::capabilities(
            Platform::Android,
            LifecycleMode::AndroidForegroundService,
            false,
        );
        assert_eq!(caps.platform, Platform::Android);
    }

    #[test]
    fn capabilities_dispatches_to_ios() {
        let caps = CapabilityProvider::capabilities(
            Platform::Ios,
            LifecycleMode::IosBgTaskScheduler,
            false,
        );
        assert_eq!(caps.platform, Platform::Ios);
    }

    #[test]
    fn capabilities_dispatches_to_desktop_in_process() {
        let caps = CapabilityProvider::capabilities(
            Platform::Linux,
            LifecycleMode::DesktopInProcess,
            false,
        );
        assert_eq!(caps.lifecycle_mode, LifecycleMode::DesktopInProcess);
        assert_eq!(caps.survives_app_close, LifecycleGuarantee::Unsupported);
    }

    #[test]
    fn capabilities_dispatches_to_desktop_os_service_installed() {
        let caps = CapabilityProvider::capabilities(
            Platform::Linux,
            LifecycleMode::DesktopOsService,
            true,
        );
        assert_eq!(caps.survives_app_close, LifecycleGuarantee::Guaranteed);
    }

    #[test]
    fn capabilities_dispatches_to_desktop_os_service_not_installed() {
        let caps = CapabilityProvider::capabilities(
            Platform::Linux,
            LifecycleMode::DesktopOsService,
            false,
        );
        assert_eq!(caps.survives_app_close, LifecycleGuarantee::Unsupported);
    }

    // --- detect_platform (runs on Linux) ---

    #[test]
    fn detect_platform_desktop_default_is_in_process() {
        let (platform, mode) = CapabilityProvider::detect_platform(None);
        assert_eq!(platform, Platform::Linux);
        assert_eq!(mode, LifecycleMode::DesktopInProcess);
    }

    #[test]
    fn detect_platform_desktop_os_service_mode() {
        let (platform, mode) = CapabilityProvider::detect_platform(Some("osService"));
        assert_eq!(platform, Platform::Linux);
        assert_eq!(mode, LifecycleMode::DesktopOsService);
    }

    #[test]
    fn detect_platform_desktop_in_process_explicit() {
        let (platform, mode) = CapabilityProvider::detect_platform(Some("inProcess"));
        assert_eq!(platform, Platform::Linux);
        assert_eq!(mode, LifecycleMode::DesktopInProcess);
    }
}
