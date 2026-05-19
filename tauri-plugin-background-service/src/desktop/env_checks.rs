//! Desktop environment checks for OS-service mode prerequisites.
//!
//! Provides helpers for detecting whether the desktop environment meets
//! the requirements for running as an OS-level service:
//!
//! - **Linux**: systemd lingering must be enabled for user services to
//!   survive logout.
//! - **macOS**: The app must not be sandboxed, as sandboxed apps cannot
//!   write to `~/Library/LaunchAgents/` or use `launchctl`.

use crate::error::ServiceError;

/// Parse the output of `loginctl show-user <user> -p Linger`.
///
/// Returns `Ok(true)` for `Linger=yes`, `Ok(false)` for `Linger=no`.
pub fn parse_linger_output(output: &str) -> Result<bool, ServiceError> {
    let trimmed = output.trim();
    if let Some(value) = trimmed.strip_prefix("Linger=") {
        match value.trim() {
            "yes" => Ok(true),
            "no" => Ok(false),
            other => Err(ServiceError::Platform(format!(
                "Unexpected Linger value: {other}"
            ))),
        }
    } else {
        Err(ServiceError::Platform(format!(
            "Unexpected loginctl output format: {trimmed}"
        )))
    }
}

/// Parse codesign entitlements XML for the `com.apple.security.app-sandbox` key.
///
/// Returns `true` if the entitlements contain `com.apple.security.app-sandbox`
/// set to `<true/>`.
pub fn parse_entitlements_for_sandbox(xml: &str) -> bool {
    let key = "com.apple.security.app-sandbox";
    let Some(pos) = xml.find(key) else {
        return false;
    };
    let after = &xml[pos + key.len()..];
    let Some(key_end) = after.find("</key>") else {
        return false;
    };
    let value_section = &after[key_end + "</key>".len()..];
    value_section.trim_start().starts_with("<true/>")
}

/// Check whether systemd lingering is enabled for the current user.
///
/// Runs `loginctl show-user <username> -p Linger` and parses the output.
/// Lingering must be enabled for systemd user services to survive logout.
#[cfg(target_os = "linux")]
pub fn check_systemd_lingering() -> Result<bool, ServiceError> {
    let username = std::env::var("USER")
        .map_err(|_| ServiceError::Platform("USER environment variable not set".into()))?;
    let output = std::process::Command::new("loginctl")
        .args(["show-user", &username, "-p", "Linger"])
        .output()
        .map_err(|e| ServiceError::Platform(format!("Failed to run loginctl: {e}")))?;
    if !output.status.success() {
        return Err(ServiceError::Platform(format!(
            "loginctl exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_linger_output(&String::from_utf8_lossy(&output.stdout))
}

/// Check whether the current app is sandboxed on macOS.
///
/// Runs `codesign -d --entitlements :- <exe_path>` and checks for the
/// `com.apple.security.app-sandbox` entitlement set to `true`.
#[cfg(target_os = "macos")]
pub fn check_macos_sandbox() -> Result<bool, ServiceError> {
    let exe_path = std::env::current_exe()
        .map_err(|e| ServiceError::Platform(format!("Cannot determine executable path: {e}")))?;
    let output = std::process::Command::new("codesign")
        .args(["-d", "--entitlements", ":-", &exe_path.to_string_lossy()])
        .output()
        .map_err(|e| ServiceError::Platform(format!("Failed to run codesign: {e}")))?;
    // codesign returns non-zero if no entitlements are embedded — not sandboxed.
    if !output.status.success() {
        return Ok(false);
    }
    Ok(parse_entitlements_for_sandbox(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_linger_output tests ---

    #[test]
    fn linger_yes() {
        assert!(parse_linger_output("Linger=yes").unwrap());
    }

    #[test]
    fn linger_no() {
        assert!(!parse_linger_output("Linger=no").unwrap());
    }

    #[test]
    fn linger_with_trailing_newline() {
        assert!(parse_linger_output("Linger=yes\n").unwrap());
    }

    #[test]
    fn linger_unexpected_value() {
        let result = parse_linger_output("Linger=maybe");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Unexpected Linger value"), "got: {msg}");
    }

    #[test]
    fn linger_unexpected_format() {
        let result = parse_linger_output("something else");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Unexpected loginctl output format"),
            "got: {msg}"
        );
    }

    // --- parse_entitlements_for_sandbox tests ---

    #[test]
    fn sandbox_entitlement_present_and_true() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.app-sandbox</key>
    <true/>
</dict>
</plist>"#;
        assert!(parse_entitlements_for_sandbox(xml));
    }

    #[test]
    fn sandbox_entitlement_present_but_false() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>com.apple.security.app-sandbox</key>
    <false/>
</dict>
</plist>"#;
        assert!(!parse_entitlements_for_sandbox(xml));
    }

    #[test]
    fn no_sandbox_entitlement() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>com.apple.security.network.client</key>
    <true/>
</dict>
</plist>"#;
        assert!(!parse_entitlements_for_sandbox(xml));
    }

    #[test]
    fn empty_entitlements() {
        assert!(!parse_entitlements_for_sandbox(""));
    }

    #[test]
    fn sandbox_with_other_entitlements() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>com.apple.security.network.client</key>
    <true/>
    <key>com.apple.security.app-sandbox</key>
    <true/>
    <key>com.apple.security.files.user-selected.read-only</key>
    <true/>
</dict>
</plist>"#;
        assert!(parse_entitlements_for_sandbox(xml));
    }
}
