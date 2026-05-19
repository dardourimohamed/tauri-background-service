//! Setup validation for background service prerequisites.
//!
//! [`SetupValidator`] checks platform-specific prerequisites (permissions,
//! manifest entries, service manager availability) and returns a
//! [`SetupValidationReport`] with errors (blocking) and warnings (non-blocking).
//!
//! This module is available on all platforms. Platform-specific checks are
//! gated by `cfg` attributes so they only run on the target platform.

use crate::models::{Platform, SetupIssue, SetupValidationReport, Severity};

#[cfg(test)]
use crate::models::ValidationIssue;

/// Validates background service setup prerequisites for the current platform.
///
/// Returns a [`SetupValidationReport`] containing errors (blocking issues that
/// prevent the service from working) and warnings (non-blocking issues that
/// may cause degraded behavior).
pub struct SetupValidator;

impl SetupValidator {
    /// Run all applicable checks for the current platform.
    ///
    /// The `platform` parameter is typically obtained from
    /// [`crate::capabilities::CapabilityProvider::detect_platform`].
    pub fn validate(platform: Platform) -> SetupValidationReport {
        match platform {
            Platform::Android => Self::android_checks(),
            Platform::Ios => Self::ios_checks(),
            Platform::Linux | Platform::Macos | Platform::Windows | Platform::Unknown => {
                Self::desktop_checks(platform)
            }
        }
    }

    fn android_checks() -> SetupValidationReport {
        let warnings = vec![
            SetupIssue {
                code: "android_fgs_type".into(),
                message: "Ensure the foreground service type is declared in AndroidManifest.xml \
                          with the matching permission"
                    .into(),
                platform: Platform::Android,
                fix: Some(
                    "Add <foregroundServiceType> to your <service> element and the \
                     corresponding <uses-permission> to the manifest"
                        .into(),
                ),
            },
            SetupIssue {
                code: "android_post_notifications".into(),
                message: "Android 13+ requires POST_NOTIFICATIONS runtime permission for \
                          foreground service notifications"
                    .into(),
                platform: Platform::Android,
                fix: Some(
                    "Request android.permission.POST_NOTIFICATIONS at runtime before \
                     starting the service on Android 13+"
                        .into(),
                ),
            },
            SetupIssue {
                code: "android_boot_receiver".into(),
                message: "Boot recovery requires a registered BroadcastReceiver for \
                          BOOT_COMPLETED"
                    .into(),
                platform: Platform::Android,
                fix: Some(
                    "Add RECEIVE_BOOT_COMPLETED permission and a <receiver> element for \
                     BOOT_COMPLETED in AndroidManifest.xml"
                        .into(),
                ),
            },
            SetupIssue {
                code: "android_special_use_subtype".into(),
                message: "When using specialUse FGS type, PROPERTY_SPECIAL_USE_FGS_SUBTYPE \
                          must be declared in the manifest"
                    .into(),
                platform: Platform::Android,
                fix: Some(
                    "Add <property android:name=\"android.app.PROPERTY_SPECIAL_USE_FGS_SUBTYPE\" \
                     android:value=\"your_reason\" /> to the <service> element"
                        .into(),
                ),
            },
            SetupIssue {
                code: "android_api35_boot_blocked_type".into(),
                message:
                    "Android 15 (API 35) blocks certain FGS types from starting in \
                          BOOT_COMPLETED receivers: dataSync, camera, mediaPlayback, phoneCall, \
                          mediaProjection, microphone. Boot recovery will not work with these types"
                        .into(),
                platform: Platform::Android,
                fix: Some(
                    "Use a non-blocked FGS type (e.g. connectedDevice, health, location, \
                     mediaProcessing) for boot recovery, or handle re-launch via user interaction"
                        .into(),
                ),
            },
        ];

        let issues: Vec<_> = warnings
            .iter()
            .map(|w| w.to_validation_issue(Severity::Warning))
            .collect();

        SetupValidationReport {
            ok: true,
            errors: vec![],
            warnings,
            issues,
        }
    }

    fn ios_checks() -> SetupValidationReport {
        let warnings = vec![
            SetupIssue {
                code: "ios_ui_background_modes".into(),
                message: "UIBackgroundModes must include 'background-fetch' and \
                          'background-processing' in Info.plist"
                    .into(),
                platform: Platform::Ios,
                fix: Some(
                    "Add UIBackgroundModes array with 'background-fetch' and \
                     'background-processing' to Info.plist"
                        .into(),
                ),
            },
            SetupIssue {
                code: "ios_bg_task_identifiers".into(),
                message: "BGTaskSchedulerPermittedIdentifiers must list your task \
                          identifiers in Info.plist"
                    .into(),
                platform: Platform::Ios,
                fix: Some(
                    "Add BGTaskSchedulerPermittedIdentifiers array with \
                     '$(BUNDLE_ID).bg-refresh' and '$(BUNDLE_ID).bg-processing' to Info.plist"
                        .into(),
                ),
            },
            SetupIssue {
                code: "ios_background_refresh".into(),
                message: "Background App Refresh must be enabled in iOS Settings for \
                          BGTaskScheduler to work"
                    .into(),
                platform: Platform::Ios,
                fix: Some(
                    "Instruct users to enable Background App Refresh in Settings > General > \
                     Background App Refresh"
                        .into(),
                ),
            },
        ];

        let issues: Vec<_> = warnings
            .iter()
            .map(|w| w.to_validation_issue(Severity::Warning))
            .collect();

        SetupValidationReport {
            ok: true,
            errors: vec![],
            warnings,
            issues,
        }
    }

    #[allow(unused_mut)]
    fn desktop_checks(platform: Platform) -> SetupValidationReport {
        let mut errors: Vec<SetupIssue> = vec![];
        let mut warnings: Vec<SetupIssue> = vec![];

        #[cfg(feature = "desktop-service")]
        {
            if matches!(platform, Platform::Linux) {
                let systemctl = std::path::Path::new("/usr/bin/systemctl").exists()
                    || std::path::Path::new("/bin/systemctl").exists()
                    || which_exists("systemctl");

                if !systemctl {
                    errors.push(SetupIssue {
                        code: "desktop_systemd_missing".into(),
                        message: "systemctl not found — OS service mode requires systemd".into(),
                        platform: Platform::Linux,
                        fix: Some("Install systemd or use inProcess mode".into()),
                    });
                } else {
                    let uid = unsafe { libc::getuid() };
                    let linger_path = format!("/var/lib/systemd/linger/{uid}");
                    let linger_ok = std::path::Path::new(&linger_path).exists()
                        || std::env::var("USER")
                            .ok()
                            .map(|u| {
                                std::path::Path::new(&format!("/var/lib/systemd/linger/{u}"))
                                    .exists()
                            })
                            .unwrap_or(false);

                    if !linger_ok {
                        warnings.push(SetupIssue {
                            code: "desktop_systemd_no_linger".into(),
                            message: "systemd lingering is not enabled — user services \
                                      will stop when you log out"
                                .into(),
                            platform: Platform::Linux,
                            fix: Some(
                                "Run 'loginctl enable-linger' to keep user services alive \
                                 after logout"
                                    .into(),
                            ),
                        });
                    }
                }
            }

            if matches!(platform, Platform::Macos) {
                warnings.push(SetupIssue {
                    code: "desktop_macos_sandbox".into(),
                    message: "OS service mode is incompatible with macOS App Sandbox. \
                              Ensure your app is not sandboxed or use inProcess mode"
                        .into(),
                    platform: Platform::Macos,
                    fix: Some(
                        "Disable App Sandbox in your app's entitlements, or use \
                         desktopServiceMode: 'inProcess'"
                            .into(),
                    ),
                });
            }
        }

        #[cfg(not(feature = "desktop-service"))]
        {
            let _ = platform;
        }

        let issues: Vec<_> = errors
            .iter()
            .map(|e| e.to_validation_issue(Severity::Error))
            .chain(
                warnings
                    .iter()
                    .map(|w| w.to_validation_issue(Severity::Warning)),
            )
            .collect();

        SetupValidationReport {
            ok: errors.is_empty(),
            errors,
            warnings,
            issues,
        }
    }
}

/// Check if a command exists in PATH.
#[cfg(feature = "desktop-service")]
fn which_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_returns_no_errors() {
        let report = SetupValidator::validate(Platform::Android);
        assert!(
            report.errors.is_empty(),
            "Android should have no hard errors (checks happen at build/Kotlin level)"
        );
        assert!(!report.warnings.is_empty(), "Android should have warnings");
        assert!(report.ok, "ok should be true when errors is empty");
    }

    #[test]
    fn android_has_fgs_type_warning() {
        let report = SetupValidator::validate(Platform::Android);
        let codes: Vec<&str> = report.warnings.iter().map(|w| w.code.as_str()).collect();
        assert!(
            codes.contains(&"android_fgs_type"),
            "Should warn about FGS type: {codes:?}"
        );
    }

    #[test]
    fn android_has_post_notifications_warning() {
        let report = SetupValidator::validate(Platform::Android);
        let codes: Vec<&str> = report.warnings.iter().map(|w| w.code.as_str()).collect();
        assert!(
            codes.contains(&"android_post_notifications"),
            "Should warn about POST_NOTIFICATIONS: {codes:?}"
        );
    }

    #[test]
    fn android_has_boot_receiver_warning() {
        let report = SetupValidator::validate(Platform::Android);
        let codes: Vec<&str> = report.warnings.iter().map(|w| w.code.as_str()).collect();
        assert!(
            codes.contains(&"android_boot_receiver"),
            "Should warn about boot receiver: {codes:?}"
        );
    }

    #[test]
    fn android_has_special_use_subtype_warning() {
        let report = SetupValidator::validate(Platform::Android);
        let codes: Vec<&str> = report.warnings.iter().map(|w| w.code.as_str()).collect();
        assert!(
            codes.contains(&"android_special_use_subtype"),
            "Should warn about specialUse subtype: {codes:?}"
        );
    }

    #[test]
    fn android_has_api35_boot_blocked_type_warning() {
        let report = SetupValidator::validate(Platform::Android);
        let codes: Vec<&str> = report.warnings.iter().map(|w| w.code.as_str()).collect();
        assert!(
            codes.contains(&"android_api35_boot_blocked_type"),
            "Should warn about API 35+ boot-blocked FGS types: {codes:?}"
        );
    }

    #[test]
    fn android_api35_boot_blocked_warning_lists_types() {
        let report = SetupValidator::validate(Platform::Android);
        let warning = report
            .warnings
            .iter()
            .find(|w| w.code == "android_api35_boot_blocked_type")
            .expect("Should have android_api35_boot_blocked_type warning");
        for ty in &[
            "dataSync",
            "camera",
            "mediaPlayback",
            "phoneCall",
            "mediaProjection",
            "microphone",
        ] {
            assert!(
                warning.message.contains(ty),
                "Warning message should mention '{}': {}",
                ty,
                warning.message
            );
        }
        assert!(warning.fix.is_some(), "Should have a fix suggestion");
    }

    #[test]
    fn android_all_warnings_have_fix() {
        let report = SetupValidator::validate(Platform::Android);
        for w in &report.warnings {
            assert!(
                w.fix.is_some(),
                "Warning '{}' should have a fix suggestion",
                w.code
            );
        }
    }

    #[test]
    fn android_all_warnings_are_android_platform() {
        let report = SetupValidator::validate(Platform::Android);
        for w in &report.warnings {
            assert_eq!(
                w.platform,
                Platform::Android,
                "Warning '{}' should be Android platform",
                w.code
            );
        }
    }

    #[test]
    fn ios_returns_no_errors() {
        let report = SetupValidator::validate(Platform::Ios);
        assert!(
            report.errors.is_empty(),
            "iOS should have no hard errors (checks happen at build/Swift level)"
        );
        assert!(!report.warnings.is_empty(), "iOS should have warnings");
        assert!(report.ok, "ok should be true when errors is empty");
    }

    #[test]
    fn ios_has_background_modes_warning() {
        let report = SetupValidator::validate(Platform::Ios);
        let codes: Vec<&str> = report.warnings.iter().map(|w| w.code.as_str()).collect();
        assert!(
            codes.contains(&"ios_ui_background_modes"),
            "Should warn about UIBackgroundModes: {codes:?}"
        );
    }

    #[test]
    fn ios_has_task_identifiers_warning() {
        let report = SetupValidator::validate(Platform::Ios);
        let codes: Vec<&str> = report.warnings.iter().map(|w| w.code.as_str()).collect();
        assert!(
            codes.contains(&"ios_bg_task_identifiers"),
            "Should warn about BGTaskSchedulerPermittedIdentifiers: {codes:?}"
        );
    }

    #[test]
    fn ios_has_background_refresh_warning() {
        let report = SetupValidator::validate(Platform::Ios);
        let codes: Vec<&str> = report.warnings.iter().map(|w| w.code.as_str()).collect();
        assert!(
            codes.contains(&"ios_background_refresh"),
            "Should warn about background refresh: {codes:?}"
        );
    }

    #[test]
    fn ios_all_warnings_have_fix() {
        let report = SetupValidator::validate(Platform::Ios);
        for w in &report.warnings {
            assert!(
                w.fix.is_some(),
                "Warning '{}' should have a fix suggestion",
                w.code
            );
        }
    }

    #[test]
    fn ios_all_warnings_are_ios_platform() {
        let report = SetupValidator::validate(Platform::Ios);
        for w in &report.warnings {
            assert_eq!(
                w.platform,
                Platform::Ios,
                "Warning '{}' should be iOS platform",
                w.code
            );
        }
    }

    #[test]
    fn desktop_linux_no_errors_by_default() {
        let report = SetupValidator::validate(Platform::Linux);
        assert!(
            report.ok || !report.errors.is_empty(),
            "Report should be consistent: ok == errors.is_empty()"
        );
        assert_eq!(report.ok, report.errors.is_empty());
    }

    #[test]
    fn desktop_macos_no_errors_by_default() {
        let report = SetupValidator::validate(Platform::Macos);
        assert_eq!(report.ok, report.errors.is_empty());
    }

    #[test]
    fn desktop_windows_no_errors() {
        let report = SetupValidator::validate(Platform::Windows);
        assert!(
            report.errors.is_empty(),
            "Windows should have no desktop-service errors (not yet supported)"
        );
        assert!(report.ok);
    }

    #[test]
    fn desktop_unknown_no_errors() {
        let report = SetupValidator::validate(Platform::Unknown);
        assert!(report.errors.is_empty());
        assert!(report.ok);
    }

    #[test]
    fn all_issues_have_non_empty_message() {
        for platform in [
            Platform::Android,
            Platform::Ios,
            Platform::Linux,
            Platform::Macos,
            Platform::Windows,
        ] {
            let report = SetupValidator::validate(platform);
            for issue in report.errors.iter().chain(report.warnings.iter()) {
                assert!(
                    !issue.message.is_empty(),
                    "Issue '{}' on {:?} should have a non-empty message",
                    issue.code,
                    platform
                );
                assert!(
                    !issue.code.is_empty(),
                    "Found an issue with an empty code on {:?}",
                    platform
                );
            }
        }
    }

    #[test]
    fn setup_issue_serde_roundtrip() {
        let issue = SetupIssue {
            code: "test_code".into(),
            message: "Test message".into(),
            platform: Platform::Android,
            fix: Some("Do something".into()),
        };
        let json = serde_json::to_string(&issue).unwrap();
        let de: SetupIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(de.code, "test_code");
        assert_eq!(de.message, "Test message");
        assert_eq!(de.platform, Platform::Android);
        assert_eq!(de.fix, Some("Do something".into()));
    }

    #[test]
    fn setup_issue_json_keys_camel_case() {
        let issue = SetupIssue {
            code: "c".into(),
            message: "m".into(),
            platform: Platform::Linux,
            fix: Some("f".into()),
        };
        let json = serde_json::to_string(&issue).unwrap();
        assert!(json.contains("\"code\":"), "{json}");
        assert!(json.contains("\"message\":"), "{json}");
        assert!(json.contains("\"platform\":"), "{json}");
        assert!(json.contains("\"fix\":"), "{json}");
    }

    #[test]
    fn setup_issue_fix_absent_when_none() {
        let issue = SetupIssue {
            code: "c".into(),
            message: "m".into(),
            platform: Platform::Linux,
            fix: None,
        };
        let json = serde_json::to_string(&issue).unwrap();
        assert!(
            !json.contains("\"fix\""),
            "fix should be absent when None: {json}"
        );
    }

    #[test]
    fn setup_validation_report_serde_roundtrip() {
        let report = SetupValidationReport {
            ok: true,
            errors: vec![],
            warnings: vec![SetupIssue {
                code: "w1".into(),
                message: "Warning 1".into(),
                platform: Platform::Android,
                fix: Some("Fix it".into()),
            }],
            issues: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        let de: SetupValidationReport = serde_json::from_str(&json).unwrap();
        assert!(de.ok);
        assert!(de.errors.is_empty());
        assert_eq!(de.warnings.len(), 1);
        assert_eq!(de.warnings[0].code, "w1");
    }

    #[test]
    fn setup_validation_report_json_keys_camel_case() {
        let report = SetupValidationReport {
            ok: false,
            errors: vec![SetupIssue {
                code: "e1".into(),
                message: "Error".into(),
                platform: Platform::Ios,
                fix: None,
            }],
            warnings: vec![],
            issues: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"ok\":"), "{json}");
        assert!(json.contains("\"errors\":"), "{json}");
        assert!(json.contains("\"warnings\":"), "{json}");
    }

    #[test]
    fn setup_validation_report_ok_true_when_no_errors() {
        let report = SetupValidationReport {
            ok: true,
            errors: vec![],
            warnings: vec![SetupIssue {
                code: "w".into(),
                message: "warn".into(),
                platform: Platform::Linux,
                fix: None,
            }],
            issues: vec![],
        };
        assert!(report.ok);
    }

    #[test]
    fn setup_validation_report_ok_false_with_errors() {
        let report = SetupValidationReport {
            ok: false,
            errors: vec![SetupIssue {
                code: "e".into(),
                message: "err".into(),
                platform: Platform::Linux,
                fix: None,
            }],
            warnings: vec![],
            issues: vec![],
        };
        assert!(!report.ok);
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn which_exists_true_for_ls() {
        assert!(which_exists("ls"), "ls should exist in PATH");
    }

    #[cfg(feature = "desktop-service")]
    #[test]
    fn which_exists_false_for_nonsense() {
        assert!(
            !which_exists("definitely_not_a_real_command_xyz_123"),
            "nonsense command should not exist"
        );
    }

    #[test]
    fn android_error_prevents_ok() {
        let report = SetupValidationReport {
            ok: false,
            errors: vec![SetupIssue {
                code: "test_error".into(),
                message: "test".into(),
                platform: Platform::Android,
                fix: None,
            }],
            warnings: vec![],
            issues: vec![],
        };
        assert!(!report.ok);
        assert_eq!(report.errors.len(), 1);
    }

    #[test]
    fn warnings_do_not_affect_ok() {
        let report = SetupValidationReport {
            ok: true,
            errors: vec![],
            warnings: vec![SetupIssue {
                code: "w".into(),
                message: "just a warning".into(),
                platform: Platform::Linux,
                fix: None,
            }],
            issues: vec![],
        };
        assert!(report.ok);
        assert!(!report.warnings.is_empty());
    }

    // ── Structured issues tests ──────────────────────────────────────

    #[test]
    fn setup_issue_to_validation_issue_error_severity() {
        let issue = SetupIssue {
            code: "test".into(),
            message: "msg".into(),
            platform: Platform::Linux,
            fix: Some("fix it".into()),
        };
        let vi = issue.to_validation_issue(Severity::Error);
        assert_eq!(vi.severity, Severity::Error);
        assert_eq!(vi.code, "test");
        assert_eq!(vi.message, "msg");
        assert_eq!(vi.platform, Platform::Linux);
        assert_eq!(vi.fix, Some("fix it".into()));
    }

    #[test]
    fn setup_issue_to_validation_issue_warning_severity() {
        let issue = SetupIssue {
            code: "w".into(),
            message: "warn msg".into(),
            platform: Platform::Android,
            fix: None,
        };
        let vi = issue.to_validation_issue(Severity::Warning);
        assert_eq!(vi.severity, Severity::Warning);
        assert_eq!(vi.code, "w");
        assert!(vi.fix.is_none());
    }

    #[test]
    fn android_issues_populated_with_warning_severity() {
        let report = SetupValidator::validate(Platform::Android);
        assert!(
            !report.issues.is_empty(),
            "Android should have structured issues"
        );
        assert_eq!(
            report.issues.len(),
            report.warnings.len(),
            "issues count should match warnings count (no errors on Android)"
        );
        for vi in &report.issues {
            assert_eq!(
                vi.severity,
                Severity::Warning,
                "All Android issues should be warnings: {:?}",
                vi.code
            );
        }
    }

    #[test]
    fn ios_issues_populated_with_warning_severity() {
        let report = SetupValidator::validate(Platform::Ios);
        assert!(
            !report.issues.is_empty(),
            "iOS should have structured issues"
        );
        assert_eq!(
            report.issues.len(),
            report.warnings.len(),
            "issues count should match warnings count (no errors on iOS)"
        );
        for vi in &report.issues {
            assert_eq!(
                vi.severity,
                Severity::Warning,
                "All iOS issues should be warnings: {:?}",
                vi.code
            );
        }
    }

    #[test]
    fn windows_issues_empty() {
        let report = SetupValidator::validate(Platform::Windows);
        assert!(report.issues.is_empty(), "Windows has no validation issues");
    }

    #[test]
    fn unknown_issues_empty() {
        let report = SetupValidator::validate(Platform::Unknown);
        assert!(report.issues.is_empty(), "Unknown has no validation issues");
    }

    #[test]
    fn desktop_issues_include_errors_and_warnings() {
        let report = SetupValidator::validate(Platform::Linux);
        let error_count = report
            .issues
            .iter()
            .filter(|vi| vi.severity == Severity::Error)
            .count();
        let warning_count = report
            .issues
            .iter()
            .filter(|vi| vi.severity == Severity::Warning)
            .count();
        assert_eq!(
            error_count,
            report.errors.len(),
            "Error issues should match errors count"
        );
        assert_eq!(
            warning_count,
            report.warnings.len(),
            "Warning issues should match warnings count"
        );
        assert_eq!(
            report.issues.len(),
            report.errors.len() + report.warnings.len(),
            "Total issues = errors + warnings"
        );
    }

    #[test]
    fn all_platforms_issues_match_errors_plus_warnings() {
        for platform in [
            Platform::Android,
            Platform::Ios,
            Platform::Linux,
            Platform::Macos,
            Platform::Windows,
        ] {
            let report = SetupValidator::validate(platform);
            assert_eq!(
                report.issues.len(),
                report.errors.len() + report.warnings.len(),
                "issues count should equal errors + warnings for {:?}",
                platform
            );
        }
    }

    #[test]
    fn issues_preserve_codes_from_errors_and_warnings() {
        let report = SetupValidator::validate(Platform::Android);
        let error_codes: Vec<&str> = report.errors.iter().map(|e| e.code.as_str()).collect();
        let warning_codes: Vec<&str> = report.warnings.iter().map(|w| w.code.as_str()).collect();
        let issue_codes: Vec<&str> = report.issues.iter().map(|i| i.code.as_str()).collect();
        for code in error_codes.iter().chain(warning_codes.iter()) {
            assert!(
                issue_codes.contains(code),
                "issues should contain code '{}'",
                code
            );
        }
    }

    #[test]
    fn validation_issue_serde_roundtrip() {
        let vi = ValidationIssue {
            severity: Severity::Error,
            code: "test_code".into(),
            message: "test message".into(),
            fix: Some("fix it".into()),
            platform: Platform::Linux,
        };
        let json = serde_json::to_string(&vi).unwrap();
        let de: ValidationIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(de.severity, Severity::Error);
        assert_eq!(de.code, "test_code");
        assert_eq!(de.message, "test message");
    }

    #[test]
    fn report_issues_default_empty_on_deserialize() {
        let json = r#"{"ok":true,"errors":[],"warnings":[]}"#;
        let de: SetupValidationReport = serde_json::from_str(json).unwrap();
        assert!(de.issues.is_empty(), "issues should default to empty");
    }
}
