const COMMANDS: &[&str] = &[
    "start",
    "stop",
    "is_running",
    "get_service_state",
    "get_platform_capabilities",
    "get_scheduling_status",
    "get_pending_bg_task",
    "can_use_full_screen_intent",
    "open_full_screen_intent_settings",
    "enable_auto_restart",
    "disable_auto_restart",
    "get_desired_service_state",
    "native_lifecycle_event",
    "validate_setup",
    "get_lifecycle_status",
    "configure_recovery",
    // BGS-21 (doc-08 Step 12): notification-permission status + request. These
    // route to the Android Kotlin @Command methods of the same camelCase names
    // via run_mobile_plugin; the build.rs entry drives permission-token
    // autogeneration (allow-get-/allow-request-notification-permission).
    "get_notification_permission_status",
    "request_notification_permission",
    // BGS-22 (doc-08 Step 14): Android battery-optimization exemption request.
    // Routes to the Kotlin requestBatteryExemption @Command via run_mobile_plugin
    // (ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS). The
    // REQUEST_IGNORE_BATTERY_OPTIMIZATIONS permission is declared in the plugin
    // AndroidManifest.xml but was never requested (dead) until this flow.
    "request_battery_exemption",
];

#[cfg(feature = "desktop-service")]
const DESKTOP_COMMANDS: &[&str] = &[
    "install_service",
    "uninstall_service",
    "start_os_service",
    "stop_os_service",
    "restart_os_service",
    "get_os_service_status",
];

fn main() {
    #[allow(unused_mut)]
    let mut all_commands = COMMANDS.to_vec();
    #[cfg(feature = "desktop-service")]
    all_commands.extend_from_slice(DESKTOP_COMMANDS);

    let mut builder = tauri_plugin::Builder::new(&all_commands);

    // Only register mobile paths when building through the Tauri CLI, which sets
    // TAURI_ANDROID_PROJECT_PATH / TAURI_IOS_PROJECT_PATH. Bare cargo invocations
    // (e.g. `cargo check --target aarch64-apple-ios` in CI) would otherwise trigger
    // xcodebuild via tauri_utils::build::link_apple_library and panic.
    if std::env::var("TAURI_ANDROID_PROJECT_PATH").is_ok() {
        builder = builder.android_path("android");
    }
    if std::env::var("TAURI_IOS_PROJECT_PATH").is_ok() {
        builder = builder.ios_path("ios");
    }

    if let Err(e) = builder.try_build() {
        panic!("{e:#}");
    }
}
