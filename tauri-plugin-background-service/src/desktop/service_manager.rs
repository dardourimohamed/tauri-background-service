//! Desktop OS service lifecycle management.
//!
//! Wraps the `service-manager` crate to provide install, uninstall, start,
//! and stop operations for OS-level services (systemd, launchd,
//! Windows Service). Also provides helpers for parsing service mode and
//! deriving service labels from the app identifier.

use std::ffi::OsString;
use std::path::PathBuf;

use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager,
    ServiceUninstallCtx,
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
            .set_level(ServiceLevel::User)
            .map_err(|e| ServiceError::Platform(format!("Failed to set service level: {e}")))?;
        Ok(Self {
            label: parsed_label,
            manager,
            exec_path,
        })
    }

    /// Install the OS service.
    pub fn install(&self) -> Result<(), ServiceError> {
        self.manager
            .install(ServiceInstallCtx {
                label: self.label.clone(),
                program: self.exec_path.clone(),
                args: vec![
                    OsString::from("--service-label"),
                    OsString::from(self.label.to_string()),
                ],
                contents: None,
                username: None,
                working_directory: None,
                environment: None,
                autostart: false,
                restart_policy: RestartPolicy::OnFailure {
                    delay_secs: None,
                    max_retries: None,
                    reset_after_secs: None,
                },
            })
            .map_err(|e| ServiceError::ServiceInstall(e.to_string()))
    }

    /// Uninstall the OS service.
    pub fn uninstall(&self) -> Result<(), ServiceError> {
        self.manager
            .uninstall(ServiceUninstallCtx {
                label: self.label.clone(),
            })
            .map_err(|e| ServiceError::ServiceUninstall(e.to_string()))
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
        let label = derive_service_label(&handle, Some("my.custom.label"));
        assert_eq!(label, "my.custom.label");
    }

    #[test]
    fn derive_service_label_auto() {
        let app = tauri::test::mock_app();
        let handle = app.handle();
        let label = derive_service_label(&handle, None);
        assert!(
            label.ends_with(".background-service"),
            "Label should end with .background-service, got: {label}"
        );
    }
}
