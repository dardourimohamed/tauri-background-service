//! Error types returned by background service operations.
//!
//! [`ServiceError`] is `#[non_exhaustive]` — new variants may be added in
//! minor releases. Match with a wildcard `_` arm to avoid breakage.

/// Errors that can occur during background service lifecycle.
#[derive(Debug, thiserror::Error, Clone, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum ServiceError {
    /// A service is already running; call `stopService()` first.
    #[error("Service is already running")]
    AlreadyRunning,

    /// No service is currently running.
    #[error("Service is not running")]
    NotRunning,

    /// The service's `init()` method failed.
    #[error("Initialisation failed: {0}")]
    Init(String),

    /// A runtime error occurred inside the service's `run()` method.
    #[error("Runtime error: {0}")]
    Runtime(String),

    /// A platform-specific error (e.g. Android foreground service denied).
    #[error("Platform error: {0}")]
    Platform(String),

    /// A plugin invoke error from `run_mobile_plugin` (mobile only).
    #[cfg(mobile)]
    #[error("Plugin invoke error: {0}")]
    PluginInvoke(String),

    /// Failed to install the OS service (desktop only).
    #[cfg(feature = "desktop-service")]
    #[error("Service installation failed: {0}")]
    ServiceInstall(String),

    /// Failed to uninstall the OS service (desktop only).
    #[cfg(feature = "desktop-service")]
    #[error("Service uninstallation failed: {0}")]
    ServiceUninstall(String),

    /// An IPC communication error (desktop only).
    #[cfg(feature = "desktop-service")]
    #[error("IPC error: {0}")]
    Ipc(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_already_running() {
        assert_eq!(ServiceError::AlreadyRunning.to_string(), "Service is already running");
    }

    #[test]
    fn display_not_running() {
        assert_eq!(ServiceError::NotRunning.to_string(), "Service is not running");
    }

    #[test]
    fn display_init() {
        let msg = "db connection failed".to_string();
        assert_eq!(
            ServiceError::Init(msg.clone()).to_string(),
            format!("Initialisation failed: {msg}")
        );
    }

    #[test]
    fn display_runtime() {
        let msg = "network timeout".to_string();
        assert_eq!(
            ServiceError::Runtime(msg.clone()).to_string(),
            format!("Runtime error: {msg}")
        );
    }

    #[test]
    fn display_platform() {
        let msg = "foreground service denied".to_string();
        assert_eq!(
            ServiceError::Platform(msg.clone()).to_string(),
            format!("Platform error: {msg}")
        );
    }

    #[test]
    fn convert_to_invoke_error_via_serialize() {
        // ServiceError derives Serialize, so Tauri's blanket From<T: Serialize> for InvokeError applies.
        // Verify the conversion compiles (type-level proof).
        let err = ServiceError::Init("test".into());
        let invoke_err: tauri::ipc::InvokeError = err.into();
        // InvokeError wraps serde_json::Value — verify it contains the serialized form
        let _val = &invoke_err.0;
        assert!(!invoke_err.0.is_null());
    }

    #[test]
    fn clone_roundtrip() {
        let err = ServiceError::Init("test".into());
        let cloned = err.clone();
        assert_eq!(err.to_string(), cloned.to_string());
    }

    #[test]
    fn serde_roundtrip_already_running() {
        let err = ServiceError::AlreadyRunning;
        let json = serde_json::to_string(&err).unwrap();
        let de: ServiceError = serde_json::from_str(&json).unwrap();
        assert!(matches!(de, ServiceError::AlreadyRunning));
    }

    #[test]
    fn serde_roundtrip_init() {
        let err = ServiceError::Init("boom".into());
        let json = serde_json::to_string(&err).unwrap();
        let de: ServiceError = serde_json::from_str(&json).unwrap();
        assert!(matches!(de, ServiceError::Init(ref s) if s == "boom"));
    }

    #[cfg(feature = "desktop-service")]
    mod desktop_service {
        use super::*;

        #[test]
        fn display_service_install() {
            let msg = "permission denied".to_string();
            assert_eq!(
                ServiceError::ServiceInstall(msg.clone()).to_string(),
                format!("Service installation failed: {msg}")
            );
        }

        #[test]
        fn display_service_uninstall() {
            let msg = "not found".to_string();
            assert_eq!(
                ServiceError::ServiceUninstall(msg.clone()).to_string(),
                format!("Service uninstallation failed: {msg}")
            );
        }

        #[test]
        fn display_ipc_error() {
            let msg = "connection lost".to_string();
            assert_eq!(
                ServiceError::Ipc(msg.clone()).to_string(),
                format!("IPC error: {msg}")
            );
        }

        #[test]
        fn serde_roundtrip_service_install() {
            let err = ServiceError::ServiceInstall("fail".into());
            let json = serde_json::to_string(&err).unwrap();
            let de: ServiceError = serde_json::from_str(&json).unwrap();
            assert!(matches!(de, ServiceError::ServiceInstall(ref s) if s == "fail"));
        }

        #[test]
        fn serde_roundtrip_service_uninstall() {
            let err = ServiceError::ServiceUninstall("fail".into());
            let json = serde_json::to_string(&err).unwrap();
            let de: ServiceError = serde_json::from_str(&json).unwrap();
            assert!(matches!(de, ServiceError::ServiceUninstall(ref s) if s == "fail"));
        }

        #[test]
        fn serde_roundtrip_ipc() {
            let err = ServiceError::Ipc("socket closed".into());
            let json = serde_json::to_string(&err).unwrap();
            let de: ServiceError = serde_json::from_str(&json).unwrap();
            assert!(matches!(de, ServiceError::Ipc(ref s) if s == "socket closed"));
        }

        #[test]
        fn clone_roundtrip_service_install() {
            let err = ServiceError::ServiceInstall("fail".into());
            let cloned = err.clone();
            assert_eq!(err.to_string(), cloned.to_string());
        }

        #[test]
        fn clone_roundtrip_service_uninstall() {
            let err = ServiceError::ServiceUninstall("fail".into());
            let cloned = err.clone();
            assert_eq!(err.to_string(), cloned.to_string());
        }

        #[test]
        fn clone_roundtrip_ipc() {
            let err = ServiceError::Ipc("timeout".into());
            let cloned = err.clone();
            assert_eq!(err.to_string(), cloned.to_string());
        }
    }
}
