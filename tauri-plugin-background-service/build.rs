const COMMANDS: &[&str] = &["start", "stop", "is_running"];

#[cfg(feature = "desktop-service")]
const DESKTOP_COMMANDS: &[&str] = &["install_service", "uninstall_service"];

fn main() {
    #[allow(unused_mut)]
    let mut all_commands = COMMANDS.to_vec();
    #[cfg(feature = "desktop-service")]
    all_commands.extend_from_slice(DESKTOP_COMMANDS);

    tauri_plugin::Builder::new(&all_commands)
        .android_path("android")
        .ios_path("ios")
        .build();
}
