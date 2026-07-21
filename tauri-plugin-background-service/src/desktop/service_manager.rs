//! Desktop OS service lifecycle management.
//!
//! Wraps the `service-manager` crate to provide install, uninstall, start,
//! and stop operations for OS-level services (systemd, launchd,
//! Windows Service). Also provides helpers for parsing service mode and
//! deriving service labels from the app identifier.
//!
//! # Service Level
//!
//! On Unix the service is installed at **user level** (systemd `--user`,
//! launchd per-user agent). On Windows both `service-manager` backends
//! (`sc.exe` and winsw) reject [`ServiceLevel::User`], so the service is
//! installed as a **system-level SCM service**; per-user isolation comes from
//! the daemon's data directory (resolved per-user by the app), not from the
//! service level. SCM stores the service configuration in the registry, so
//! `DesktopServiceManager::install` passes no unit-file contents on Windows
//! (`build_contents()` returns `None` there).

use std::ffi::OsString;
use std::path::PathBuf;

use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager, ServiceStartCtx,
    ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};
use tauri::AppHandle;

use crate::error::ServiceError;

/// Derive the service label from the app identifier.
///
/// If `override_label` is provided, it is used directly. Otherwise the label
/// is derived as `{app_identifier}.background-service`.
pub fn derive_service_label<R: tauri::Runtime>(
    app: &AppHandle<R>,
    override_label: Option<&str>,
) -> String {
    if let Some(label) = override_label {
        return label.to_string();
    }
    let ident = app.config().identifier.clone();
    format!("{ident}.background-service")
}

/// Options for OS service installation.
///
/// Passed to [`DesktopServiceManager::install()`] to control autostart,
/// restart behavior, and logging output for the OS service.
#[derive(Default)]
pub(crate) struct InstallOptions {
    /// Whether the OS service should start automatically on boot (Linux)
    /// or login (macOS). Only applies when `desktop_service_mode` is `"osService"`.
    pub autostart: bool,
    /// Delay in seconds before restarting after a failure.
    /// Maps to `RestartSec` in systemd. Launchd does not support restart delays.
    pub restart_delay_secs: Option<u32>,
    /// Whether to direct stdout/stderr to the systemd journal (Linux only).
    /// When true, `StandardOutput=journal` and `StandardError=journal` are
    /// added to the systemd unit file.
    pub journal_output: bool,
    /// Directory path for stdout/stderr log files (macOS only).
    /// When set, `StandardOutPath` and `StandardErrorPath` are added to the
    /// launchd plist, pointing to `{log_path}/stdout.log` and `{log_path}/stderr.log`.
    pub log_path: Option<PathBuf>,
    /// systemd `StartLimitBurst` in `[Unit]` — max restarts within
    /// `start_limit_interval_secs` before systemd stops restarting the unit
    /// (crash-loop backoff, BGS-05 Leg C). `None` ⇒ omit (no burst cap).
    /// systemd-native; launchd has no equivalent.
    pub start_limit_burst: Option<u32>,
    /// systemd `StartLimitIntervalSec` in `[Unit]` — the rolling window over which
    /// `start_limit_burst` is counted (BGS-05 Leg C). `None` ⇒ omit.
    pub start_limit_interval_secs: Option<u32>,
}

/// Build systemd unit file contents with journal output and restart policy.
///
/// Generates a complete `[Unit]`/`[Service]`/`[Install]` unit that includes
/// `StandardOutput=journal` and `StandardError=journal` for log capture,
/// plus `Restart=on-failure` with an optional delay.
pub(crate) fn make_systemd_unit_contents(
    label: &ServiceLabel,
    exec_path: &std::path::Path,
    autostart: bool,
    restart_delay_secs: Option<u32>,
    start_limit_burst: Option<u32>,
    start_limit_interval_secs: Option<u32>,
) -> String {
    use std::fmt::Write as _;
    let label_str = label.to_string();
    let program = exec_path.to_string_lossy();

    let mut out = String::new();
    let _ = writeln!(out, "[Unit]");
    let _ = writeln!(out, "Description={label_str}");
    // BGS-05 Leg C: crash-loop backoff. systemd counts restarts within this
    // interval and stops restarting once `StartLimitBurst` is reached. Both go
    // in `[Unit]` (systemd requirement). Omitted when None.
    if let Some(burst) = start_limit_burst {
        let _ = writeln!(out, "StartLimitBurst={burst}");
    }
    if let Some(interval) = start_limit_interval_secs {
        let _ = writeln!(out, "StartLimitIntervalSec={interval}");
    }
    let _ = writeln!(out, "[Service]");
    let _ = writeln!(out, "ExecStart={program} --service-label {label_str}");
    let _ = writeln!(out, "Restart=on-failure");
    if let Some(delay) = restart_delay_secs {
        let _ = writeln!(out, "RestartSec={delay}");
    }
    let _ = writeln!(out, "StandardOutput=journal");
    let _ = writeln!(out, "StandardError=journal");

    if autostart {
        let _ = writeln!(out, "[Install]");
        let _ = writeln!(out, "WantedBy=default.target");
    }
    out.trim_end().to_string()
}

/// Build launchd plist contents with log file paths and restart policy.
///
/// Generates a complete plist that includes `StandardOutPath` and
/// `StandardErrorPath` for log capture, plus `KeepAlive` with
/// `SuccessfulExit=false` for on-failure restart behavior.
pub(crate) fn make_launchd_plist_contents(
    label: &ServiceLabel,
    exec_path: &std::path::Path,
    autostart: bool,
    log_path: &std::path::Path,
) -> String {
    let label_str = label.to_qualified_name();
    let program = exec_path.to_string_lossy();
    let stdout_file = log_path.join("stdout.log");
    let stderr_file = log_path.join("stderr.log");
    let stdout_path = stdout_file.to_string_lossy();
    let stderr_path = stderr_file.to_string_lossy();
    let run_at_load = if autostart { "true" } else { "false" };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label_str}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
        <string>--service-label</string>
        <string>{label_str}</string>
    </array>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>RunAtLoad</key>
    <{run_at_load}/>
    <key>Disabled</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{stdout_path}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_path}</string>
</dict>
</plist>"#
    )
    .trim_end()
    .to_string()
}

/// The service level for the current platform.
///
/// `User` on Unix (systemd `--user` / launchd agent); `System` on Windows,
/// where both `service-manager` backends (sc.exe, winsw) reject
/// `ServiceLevel::User` — see the module docs for the per-user data-dir
/// strategy that compensates.
pub(crate) fn service_level_for_platform() -> ServiceLevel {
    // DESK-01: Windows daemon support was removed. The OS-service path is
    // Unix-only (systemd user service / launchd agent), so the level is
    // always User. (Windows SCM used System; that path is gone.)
    ServiceLevel::User
}

/// Manages an OS-level service lifecycle using the `service-manager` crate.
///
/// Used in later steps to install/uninstall/start/stop OS-level services.
pub(crate) struct DesktopServiceManager {
    label: ServiceLabel,
    manager: Box<dyn ServiceManager>,
    exec_path: PathBuf,
}

impl DesktopServiceManager {
    /// Create a new `DesktopServiceManager` for the given label and executable.
    pub fn new(label: &str, exec_path: PathBuf) -> Result<Self, ServiceError> {
        let parsed_label: ServiceLabel = label
            .parse()
            .map_err(|e| ServiceError::Platform(format!("Invalid service label: {e}")))?;
        let mut manager = <dyn ServiceManager>::native()
            .map_err(|e| ServiceError::Platform(format!("No native service manager: {e}")))?;
        manager
            .set_level(service_level_for_platform())
            .map_err(|e| ServiceError::Platform(format!("Failed to set service level: {e}")))?;
        Ok(Self {
            label: parsed_label,
            manager,
            exec_path,
        })
    }

    /// Install the OS service with the given options.
    pub fn install(&self, options: &InstallOptions) -> Result<(), ServiceError> {
        let contents = self.build_contents(options);
        self.manager
            .install(ServiceInstallCtx {
                label: self.label.clone(),
                program: self.exec_path.clone(),
                args: vec![
                    OsString::from("--service-label"),
                    OsString::from(self.label.to_string()),
                ],
                contents,
                username: None,
                working_directory: None,
                environment: None,
                autostart: options.autostart,
                restart_policy: RestartPolicy::OnFailure {
                    delay_secs: options.restart_delay_secs,
                    max_retries: None,
                    reset_after_secs: None,
                },
            })
            .map_err(|e| ServiceError::ServiceInstall(e.to_string()))
    }

    /// Unit-file contents for the install context, when the platform uses one.
    ///
    /// Windows is a deliberate no-op (`None`): SCM stores the service
    /// configuration in the registry — there is no unit file to render.
    fn build_contents(&self, options: &InstallOptions) -> Option<String> {
        if cfg!(target_os = "linux") && options.journal_output {
            return Some(make_systemd_unit_contents(
                &self.label,
                &self.exec_path,
                options.autostart,
                options.restart_delay_secs,
                options.start_limit_burst,
                options.start_limit_interval_secs,
            ));
        }
        if cfg!(target_os = "macos") {
            if let Some(ref log_path) = options.log_path {
                return Some(make_launchd_plist_contents(
                    &self.label,
                    &self.exec_path,
                    options.autostart,
                    log_path,
                ));
            }
        }
        None
    }

    /// Uninstall the OS service.
    pub fn uninstall(&self) -> Result<(), ServiceError> {
        self.manager
            .uninstall(ServiceUninstallCtx {
                label: self.label.clone(),
            })
            .map_err(|e| ServiceError::ServiceUninstall(e.to_string()))
    }

    /// Start the OS service.
    ///
    /// Wraps `service-manager` crate's `start()` call. On failure, returns
    /// [`ServiceError::ServiceStart`].
    #[allow(dead_code)] // Used by start_os_service Tauri command (next task).
    pub fn start(&self) -> Result<(), ServiceError> {
        self.manager
            .start(ServiceStartCtx {
                label: self.label.clone(),
            })
            .map_err(|e| ServiceError::ServiceStart(e.to_string()))
    }

    /// Stop the OS service.
    ///
    /// Wraps `service-manager` crate's `stop()` call. On failure, returns
    /// [`ServiceError::ServiceStop`].
    #[allow(dead_code)] // Used by stop_os_service Tauri command (next task).
    pub fn stop(&self) -> Result<(), ServiceError> {
        self.manager
            .stop(ServiceStopCtx {
                label: self.label.clone(),
            })
            .map_err(|e| ServiceError::ServiceStop(e.to_string()))
    }

    /// Query the OS-native install/run status.
    ///
    /// DESK-02: wraps `service-manager`'s `status()` so the caller can
    /// distinguish `NotInstalled` from `Stopped` from `Running` rather than
    /// guessing `Installed` whenever IPC is disconnected. The IPC-connectivity
    /// axis stays separate (see `build_os_service_status`).
    pub fn status(&self) -> Result<service_manager::ServiceStatus, ServiceError> {
        self.manager
            .status(ServiceStatusCtx {
                label: self.label.clone(),
            })
            .map_err(|e| ServiceError::Platform(format!("status query failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- derive_service_label tests ---

    #[test]
    fn derive_service_label_with_override() {
        let app = tauri::test::mock_app();
        let handle = app.handle();
        let label = derive_service_label(handle, Some("my.custom.label"));
        assert_eq!(label, "my.custom.label");
    }

    #[test]
    fn derive_service_label_auto() {
        let app = tauri::test::mock_app();
        let handle = app.handle();
        let label = derive_service_label(handle, None);
        assert!(
            label.ends_with(".background-service"),
            "Label should end with .background-service, got: {label}"
        );
    }

    // --- service_level_for_platform tests ---

    #[cfg(unix)]
    #[test]
    fn service_level_is_user_on_unix() {
        assert!(
            matches!(service_level_for_platform(), ServiceLevel::User),
            "Unix service managers (systemd, launchd) run the service per-user"
        );
    }

    #[cfg(windows)]
    #[test]
    fn service_level_is_system_on_windows() {
        assert!(
            matches!(service_level_for_platform(), ServiceLevel::System),
            "Windows SCM backends (sc.exe, winsw) reject ServiceLevel::User"
        );
    }

    // --- InstallOptions tests ---

    #[test]
    fn install_options_default_values() {
        let opts = InstallOptions::default();
        assert!(!opts.autostart);
        assert_eq!(opts.restart_delay_secs, None);
        assert!(!opts.journal_output);
        assert_eq!(opts.log_path, None);
        // BGS-05 Leg C: the two crash-loop-backoff fields default to None so the
        // prod caller is the sole site that opts in (mirrors Step-5's unchanged-
        // default discipline). NV-MUT (flip the default to Some) REDs these.
        assert_eq!(opts.start_limit_burst, None);
        assert_eq!(opts.start_limit_interval_secs, None);
    }

    // --- make_systemd_unit_contents tests ---

    #[test]
    fn systemd_contents_autostart_true_has_install_section() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let contents = make_systemd_unit_contents(&label, &exec, true, None, None, None);
        assert!(
            contents.contains("[Install]"),
            "should have [Install] section: {contents}"
        );
        assert!(
            contents.contains("WantedBy=default.target"),
            "should have WantedBy: {contents}"
        );
    }

    #[test]
    fn systemd_contents_autostart_false_no_install_section() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let contents = make_systemd_unit_contents(&label, &exec, false, None, None, None);
        assert!(
            !contents.contains("[Install]"),
            "should NOT have [Install] section: {contents}"
        );
        assert!(
            !contents.contains("WantedBy"),
            "should NOT have WantedBy: {contents}"
        );
    }

    #[test]
    fn systemd_contents_has_journal_output() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let contents = make_systemd_unit_contents(&label, &exec, false, None, None, None);
        assert!(
            contents.contains("StandardOutput=journal"),
            "should have StandardOutput=journal: {contents}"
        );
        assert!(
            contents.contains("StandardError=journal"),
            "should have StandardError=journal: {contents}"
        );
    }

    #[test]
    fn systemd_contents_restart_delay() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let contents = make_systemd_unit_contents(&label, &exec, false, Some(5), None, None);
        assert!(
            contents.contains("RestartSec=5"),
            "should have RestartSec=5: {contents}"
        );
    }

    #[test]
    fn systemd_contents_no_restart_delay_when_none() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let contents = make_systemd_unit_contents(&label, &exec, false, None, None, None);
        assert!(
            !contents.contains("RestartSec"),
            "should NOT have RestartSec: {contents}"
        );
    }

    // ── BGS-05 Leg C: crash-loop backoff (StartLimitBurst / StartLimitIntervalSec) ──

    #[test]
    fn bgs05_systemd_contents_has_start_limit_when_some() {
        // The prod unit caps restarts at 5 per 60s (BGS-05 Leg C). Both lines
        // land in `[Unit]` (systemd requirement). NV-MUT (drop either writeln)
        // REDs this leg.
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let contents = make_systemd_unit_contents(&label, &exec, false, None, Some(5), Some(60));
        assert!(
            contents.contains("StartLimitBurst=5"),
            "should have StartLimitBurst=5: {contents}"
        );
        assert!(
            contents.contains("StartLimitIntervalSec=60"),
            "should have StartLimitIntervalSec=60: {contents}"
        );
        // Both must appear inside [Unit] (before [Service]).
        let unit_end = contents.find("[Service]").unwrap_or(contents.len());
        let unit_section = &contents[..unit_end];
        assert!(
            unit_section.contains("StartLimitBurst=5")
                && unit_section.contains("StartLimitIntervalSec=60"),
            "StartLimit lines must be in [Unit] (before [Service]): {contents}"
        );
    }

    #[test]
    fn bgs05_systemd_contents_no_start_limit_when_none() {
        // Default (None): neither line is emitted (regression guard — preserves
        // the pre-Step-6 unit shape for callers that pass None).
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let contents = make_systemd_unit_contents(&label, &exec, false, None, None, None);
        assert!(
            !contents.contains("StartLimitBurst"),
            "should NOT have StartLimitBurst: {contents}"
        );
        assert!(
            !contents.contains("StartLimitIntervalSec"),
            "should NOT have StartLimitIntervalSec: {contents}"
        );
    }

    #[test]
    fn systemd_contents_restart_on_failure() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let contents = make_systemd_unit_contents(&label, &exec, false, None, None, None);
        assert!(
            contents.contains("Restart=on-failure"),
            "should have Restart=on-failure: {contents}"
        );
    }

    #[test]
    fn systemd_contents_exec_start_with_label() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let contents = make_systemd_unit_contents(&label, &exec, false, None, None, None);
        assert!(
            contents.contains("ExecStart=/usr/bin/myapp --service-label com.example.bg-service"),
            "should have correct ExecStart: {contents}"
        );
    }

    #[test]
    fn systemd_contents_description_uses_label() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let contents = make_systemd_unit_contents(&label, &exec, false, None, None, None);
        assert!(
            contents.contains("Description=com.example.bg-service"),
            "should have description: {contents}"
        );
    }

    #[test]
    fn systemd_contents_unit_and_service_sections() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let contents = make_systemd_unit_contents(&label, &exec, false, None, None, None);
        assert!(
            contents.contains("[Unit]"),
            "should have [Unit]: {contents}"
        );
        assert!(
            contents.contains("[Service]"),
            "should have [Service]: {contents}"
        );
    }

    // --- make_launchd_plist_contents tests ---

    #[test]
    fn launchd_contents_has_log_paths() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let log_path = PathBuf::from("/var/log/myservice");
        let contents = make_launchd_plist_contents(&label, &exec, false, &log_path);
        assert!(
            contents.contains("<key>StandardOutPath</key>"),
            "should have StandardOutPath: {contents}"
        );
        assert!(
            contents.contains("<string>/var/log/myservice/stdout.log</string>"),
            "should have stdout path: {contents}"
        );
        assert!(
            contents.contains("<key>StandardErrorPath</key>"),
            "should have StandardErrorPath: {contents}"
        );
        assert!(
            contents.contains("<string>/var/log/myservice/stderr.log</string>"),
            "should have stderr path: {contents}"
        );
    }

    #[test]
    fn launchd_contents_autostart_true() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let log_path = PathBuf::from("/var/log/myservice");
        let contents = make_launchd_plist_contents(&label, &exec, true, &log_path);
        assert!(
            contents.contains("<key>RunAtLoad</key>\n    <true/>"),
            "should have RunAtLoad=true: {contents}"
        );
    }

    #[test]
    fn launchd_contents_autostart_false() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let log_path = PathBuf::from("/var/log/myservice");
        let contents = make_launchd_plist_contents(&label, &exec, false, &log_path);
        assert!(
            contents.contains("<key>RunAtLoad</key>\n    <false/>"),
            "should have RunAtLoad=false: {contents}"
        );
    }

    #[test]
    fn launchd_contents_has_keep_alive() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let log_path = PathBuf::from("/var/log/myservice");
        let contents = make_launchd_plist_contents(&label, &exec, false, &log_path);
        assert!(
            contents.contains("<key>KeepAlive</key>"),
            "should have KeepAlive: {contents}"
        );
        assert!(
            contents.contains("<key>SuccessfulExit</key>\n        <false/>"),
            "should have SuccessfulExit=false: {contents}"
        );
    }

    #[test]
    fn launchd_contents_has_disabled() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let log_path = PathBuf::from("/var/log/myservice");
        let contents = make_launchd_plist_contents(&label, &exec, false, &log_path);
        assert!(
            contents.contains("<key>Disabled</key>\n    <true/>"),
            "should have Disabled=true: {contents}"
        );
    }

    #[test]
    fn launchd_contents_label() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let log_path = PathBuf::from("/var/log/myservice");
        let contents = make_launchd_plist_contents(&label, &exec, false, &log_path);
        assert!(
            contents.contains("<key>Label</key>\n    <string>com.example.bg-service</string>"),
            "should have Label: {contents}"
        );
    }

    #[test]
    fn launchd_contents_program_arguments() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let log_path = PathBuf::from("/var/log/myservice");
        let contents = make_launchd_plist_contents(&label, &exec, false, &log_path);
        assert!(
            contents.contains("<string>/usr/bin/myapp</string>"),
            "should have program: {contents}"
        );
        assert!(
            contents.contains("<string>--service-label</string>"),
            "should have --service-label arg: {contents}"
        );
    }

    #[test]
    fn launchd_contents_valid_xml_structure() {
        let label: ServiceLabel = "com.example.bg-service".parse().unwrap();
        let exec = PathBuf::from("/usr/bin/myapp");
        let log_path = PathBuf::from("/var/log/myservice");
        let contents = make_launchd_plist_contents(&label, &exec, true, &log_path);
        assert!(
            contents.starts_with("<?xml"),
            "should start with XML declaration: {contents}"
        );
        assert!(
            contents.contains("<!DOCTYPE plist"),
            "should have DOCTYPE: {contents}"
        );
        assert!(
            contents.contains("<plist version=\"1.0\">"),
            "should have plist tag: {contents}"
        );
        assert!(
            contents.trim_end().ends_with("</plist>"),
            "should end with closing plist tag"
        );
    }
}
