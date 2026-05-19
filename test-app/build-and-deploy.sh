#!/bin/bash
set -euo pipefail

# ── Preflight Validation ─────────────────────────────────────────────
errors=0

# JAVA_HOME: must be JDK 21+ (JDK 25 is incompatible with Gradle 8.x)
if [ -z "${JAVA_HOME:-}" ]; then
    echo "ERROR: JAVA_HOME is not set. Install JDK 21+ and export JAVA_HOME." >&2
    errors=$((errors + 1))
elif [ ! -x "$JAVA_HOME/bin/java" ]; then
    echo "ERROR: JAVA_HOME ($JAVA_HOME) does not contain a valid JDK (missing bin/java)." >&2
    errors=$((errors + 1))
else
    java_version=$("$JAVA_HOME/bin/java" -version 2>&1 | head -1)
    echo "JAVA_HOME: $JAVA_HOME ($java_version)"
fi

# waydroid
if ! command -v waydroid &>/dev/null; then
    echo "ERROR: 'waydroid' not found in PATH." >&2
    errors=$((errors + 1))
else
    echo "waydroid: $(command -v waydroid)"
fi

# adb
if ! command -v adb &>/dev/null; then
    echo "ERROR: 'adb' not found in PATH." >&2
    errors=$((errors + 1))
else
    echo "adb: $(command -v adb)"
fi

if [ "$errors" -gt 0 ]; then
    echo >&2
    echo "Preflight failed with $errors error(s). Fix the above issues and re-run." >&2
    exit 1
fi

echo "Preflight passed."
echo

# ── Build & Deploy ───────────────────────────────────────────────────

# Start Waydroid if not running
if ! waydroid status | grep -q "RUNNING"; then
    echo "Starting Waydroid..."
    waydroid session start
    sleep 5
fi

# Connect ADB
echo "Connecting ADB..."
waydroid adb connect
sleep 2

# Verify connection
adb devices

# Build APK (x86_64 for Waydroid)
echo "Building APK..."
cd "$(dirname "$0")"
cargo tauri android build --apk --debug --target x86_64

# Install APK
echo "Installing APK..."
adb install -r src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk

echo "Deploy complete!"
