const COMMANDS: &[&str] = &["start", "stop", "is_running"];

#[cfg(feature = "desktop-service")]
const DESKTOP_COMMANDS: &[&str] = &["install_service", "uninstall_service"];

fn main() {
    #[allow(unused_mut)]
    let mut all_commands = COMMANDS.to_vec();
    #[cfg(feature = "desktop-service")]
    all_commands.extend_from_slice(DESKTOP_COMMANDS);

    let result = tauri_plugin::Builder::new(&all_commands)
        .android_path("android")
        .ios_path("ios")
        .try_build();

    // Gracefully handle build failures in CI environments (e.g. missing iOS SDK
    // during cross-compilation checks) without blocking the rest of the build.
    if let Err(e) = result {
        // Only fail hard if this looks like a real build (not a bare cargo check in CI).
        let target = std::env::var("TARGET").unwrap_or_default();
        let is_ios_ci = target.contains("apple-ios") && std::env::var("CI").is_ok();
        if !is_ios_ci {
            panic!("{e:#}");
        }
        println!("cargo:warning=tauri-plugin build skipped for CI cross-check: {e:#}");
    }
}
