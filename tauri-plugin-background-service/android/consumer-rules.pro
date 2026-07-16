# Consumer R8/ProGuard rules for tauri-plugin-background-service.
#
# These are packaged with the library and merged into the CONSUMING app's R8 run
# (the Sila app sets `isMinifyEnabled = true` on release). The Tauri framework's
# own consumer rules already keep `app.tauri.**`, but scoping the guarantees to
# this plugin here keeps it correct independently of that and documents intent.

# JNI bridge: the Rust cdylib (`sila_lib`) resolves these native methods by their
# exact `Java_app_tauri_backgroundservice_HeadlessCoreBridge_*` symbol names
# (see tauri/src/lib.rs), so the class and its native methods must not be
# renamed, moved, or stripped.
-keep class app.tauri.backgroundservice.HeadlessCoreBridge {
    native <methods>;
}

# Tauri discovers the plugin and invokes its @Command methods reflectively.
-keep @app.tauri.annotation.TauriPlugin public class app.tauri.backgroundservice.** {
    @app.tauri.annotation.Command public <methods>;
    @app.tauri.annotation.PermissionCallback <methods>;
    @app.tauri.annotation.ActivityCallback <methods>;
    @app.tauri.annotation.Permission <methods>;
    public <init>(...);
}

# @InvokeArg argument classes are populated reflectively from the JS invoke payload.
-keep @app.tauri.annotation.InvokeArg class app.tauri.backgroundservice.** {
    *;
}
