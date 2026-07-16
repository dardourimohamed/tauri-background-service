# R8/ProGuard rules for the tauri-plugin-background-service library module.
#
# This module builds with `isMinifyEnabled = false` (see build.gradle.kts), so
# these rules are not applied to the library itself; the file exists to satisfy
# the `proguardFiles(...)` reference in build.gradle.kts. The rules that must
# reach the consuming app's R8 run live in `consumer-rules.pro`.
