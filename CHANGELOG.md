# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0] - 2026-05-20

### Added

#### Structured Stop Reasons

- `StopReason` enum with 9 variants: `userStop`, `appStop`, `platformTimeout`, `platformExpiration`, `nativeNotificationStop`, `osRestart`, `bootRecovery`, `taskCompleted`, `error`
- `StopWithReason` actor command and `stop_with_reason()` handle method
- Cancel listener emits platform-specific `StopReason` values (Android: `platformTimeout`, `nativeNotificationStop`; iOS: `platformExpiration`; Desktop: `appStop`)

#### Native Lifecycle Events

- `NativeLifecycleEvent` enum for OS-signaled lifecycle transitions
- `native_lifecycle_event` Tauri command and permission
- Android Kotlin callbacks: notification stop and FGS timeout events forwarded from `LifecycleService` to Rust via JS bridge

#### Lifecycle Status API

- `LifecycleState` enum (10 states: `idle`, `starting`, `running`, `stopping`, `stopped`, `recovering`, `recoveryPending`, `expired`, `blocked`, `error`)
- `LifecycleStatus` struct with full snapshot: state, desired state, recovery config, last config, platform info, issues
- `get_lifecycle_status` Tauri command and `getLifecycleStatus()` TypeScript API

#### Recovery Configuration

- `configure_recovery` Tauri command and `configureRecovery()` TypeScript API for runtime control of auto-restart behavior

#### Desktop Desired State Persistence

- `FileDesiredStateBackend` for desktop platforms: persists desired state to filesystem for auto-recovery across app restarts
- Integration tests for file-based persistence

#### iOS BGTask Persistence

- Pending BGTask info persisted to `UserDefaults` to survive timing gaps between native handler and Rust setup
- `consumedAt` field on `PendingTaskInfo` to track when auto-start consumed the task
- `getPendingBgTask` reads from `UserDefaults` as source of truth

#### Enhanced Validation

- API 35 (Android 15) boot-blocked FGS type warning in `validate_setup`
- `ValidationIssue` struct with `severity`, `code`, `message`, `fix`, `platform` fields
- `Severity` enum: `error`, `warning`, `info`
- `issues` field on `SetupValidationReport` with unified typed issues

#### Android Notification Permission Control

- `android_request_notification_permission_on_load` config field (default: `true` for backward compat)
- `getNotificationPermissionStatus()` and `requestNotificationPermission()` Kotlin commands
- Structured FGS error handling with `startForegroundTyped()` catching `ForegroundServiceStartNotAllowedException`, `SecurityException`, and generic exceptions

#### TypeScript API

- New types: `LifecycleState`, `StopReason`, `Severity`, `ValidationIssue`, `LifecycleStatus`
- `getLifecycleStatus()`, `configureRecovery()` APIs
- Compatibility wrappers: deprecated legacy API functions delegate to new lifecycle API

#### Configuration

- `StartConfig.service_label` accepts `label` via serde alias for iOS config migration

#### Tests

- Actor-level cancel listener integration tests with `MockMobile`
- Desktop `FileDesiredStateBackend` integration tests
- Android: structured FGS error format tests, permission status tests

#### Build & Tooling

- `build-and-deploy.sh` portability: env check and preflight validation
- CONTRIBUTING.md: native test harness section and release checklist

## [0.6.0] - 2026-05-19

### Added

#### Platform Capabilities

- `PlatformCapabilities` model and `CapabilityProvider` for runtime platform guarantee reporting
- `get_platform_capabilities` command and `getPlatformCapabilities()` TypeScript API
- Honest per-platform reporting of background execution guarantees, survival characteristics, and limitations

#### Auto-Restart & Desired State Persistence

- `desired_state` module with `DesiredStateBackend` trait for cross-platform state persistence
- `enable_auto_restart` / `disable_auto_restart` commands and TypeScript APIs
- `get_desired_service_state` command to query persisted recovery intent
- Services persist their desired state for recovery after app restart or device reboot
- `DesiredState` model with recovery metadata (start config, timestamps, reason)

#### Android Boot Recovery

- `BootReceiver.kt`: handles `ACTION_BOOT_COMPLETED` and `ACTION_MY_PACKAGE_REPLACED` for post-reboot service restart
- `DurableState.kt`: SharedPreferences persistence for service state across restarts
- Enhanced `LifecycleService.kt` with auto-restart capabilities and configurable FGS types
- Android unit tests for boot receiver, durable state, and lifecycle service

#### iOS Background Task Enhancements

- Dual `BGTaskScheduler` support (refresh + processing tasks) with configurable scheduling parameters
- Configurable safety timeouts for refresh and processing tasks
- Desired state persistence via `UserDefaults` with auto-start on BGTask launch
- Pending task detection (`get_pending_bg_task` / `getPendingBgTask()`)
- Scheduling status reporting (`get_scheduling_status` / `getSchedulingStatus()`)
- Proper task completion safety to prevent double-completion
- iOS native unit tests (`BackgroundServicePluginTests.swift`)

#### Setup Validation

- `SetupValidator` and `SetupValidationReport` for checking platform-specific prerequisites
- `validate_setup` command and `validateBackgroundServiceSetup()` TypeScript API
- Platform-specific checks: Android manifest entries, iOS plist entries, desktop systemd/sandbox

#### Desktop OS Service Management

- `start_os_service`, `stop_os_service`, `restart_os_service`, `get_os_service_status` commands
- TypeScript APIs: `startOsService()`, `stopOsService()`, `restartOsService()`, `getOsServiceStatus()`
- `install_service` / `uninstall_service` commands with binary validation
- `OsServiceStatus` and `OsServiceInstallState` models
- Enhanced `DesktopServiceManager` with systemd/launchd lifecycle management
- Persistent IPC client with auto-reconnection and exponential backoff
- `wait_for_connected()` for IPC readiness with timeout
- Environment checks for systemd lingering and macOS sandbox compatibility

#### TypeScript API

- New interfaces: `PlatformCapabilities`, `IOSSchedulingStatus`, `PendingTaskInfo`, `DesiredState`, `SetupValidationReport`, `OsServiceStatus`
- `normalizeBackgroundServiceError()` for consistent error handling

#### CI & Testing

- GitHub Actions CI workflow (`ci.yml`)
- Permission descriptors for all new commands (11 new `.toml` files)
- Comprehensive permission schema updates

#### Documentation

- New docs: `migration-guide.md`, `troubleshooting.md`, `release-checklist.md`
- Expanded: `api-reference.md`, `android.md`, `ios.md`, `desktop.md`, `getting-started.md`

### Fixed

- Gate `which_exists` tests behind `desktop-service` feature flag to fix compilation when feature is disabled
- Remove constant assertion in integration test to silence clippy warning

## [0.5.2] - 2026-04-12

### Changed

- Update backon dependency from ~1.5 to ~1.6

## [0.5.1] - 2026-04-12

### Changed

- Add plugin configuration step (`tauri.conf.json`) to all documentation

## [0.5.0] - 2026-04-12

### Changed

- Documentation overhaul: all docs updated to reflect current API (ServiceState, getServiceState, desktop-service feature)
- Version bump from 0.4.1 to 0.5.0

## [0.4.1] - 2026-04-12

### Fixed

- Android test improvements and reliability fixes
- Desktop IPC transport fixes

## [0.4.0] - 2026-04-11

### Added

- `ServiceState` enum (Idle, Initializing, Running, Stopped) for fine-grained lifecycle state
- `ServiceStatus` struct with state and optional last error
- `get_service_state` command and `getServiceState()` TypeScript API
- `GetState` variant to `ManagerCommand` for actor-loop state queries
- Platform-specific `ServiceContext` fields: `service_label` and `foreground_service_type` are `String` on mobile (behind `#[cfg(mobile)]`), absent on desktop
- IPC transport layer for desktop OS service mode (length-prefixed JSON frames over Unix socket / Windows named pipe)

## [0.3.1] - 2026-04-10

### Changed

- Upgraded `service-manager` dependency from 0.7 to 0.11

## [0.3.0] - 2026-04-10

### Added

- Expanded Android foreground service types from 2 to 14: dataSync, mediaPlayback, phoneCall, location, connectedDevice, mediaProjection, camera, microphone, health, remoteMessaging, systemExempted, shortService, specialUse, mediaProcessing
- `validate_foreground_service_type()` function to reject invalid types at both Rust and Kotlin layers
- Enhanced desktop IPC with persistent client and exponential backoff

## [0.2.4] - 2026-04-10

### Added

- Android unit tests for foreground service lifecycle
- Desktop IPC and headless binary expansion

## [0.2.3] - 2026-04-09

### Fixed

- CI workflow fixes
- Pre-commit hook configuration
- Mobile type inference fixes

## [0.2.2] - 2026-04-08

### Changed

- Include build artifacts in package for docs.rs documentation

## [0.2.1] - 2026-04-08

### Changed

- Version bump

## [0.2.0] - 2026-04-08

### Added

- iOS `BGProcessingTask` support with configurable safety timeout
- Desktop OS service mode via `desktop-service` Cargo feature (systemd / launchd)
- IPC security hardening for desktop sidecar communication
- Persistent IPC client with exponential backoff reconnect
- `installService()` and `uninstallService()` TypeScript APIs (desktop only)

### Changed

- iOS safety timer now resolves (instead of rejecting) the pending cancel invoke
- iOS `completeBgTask` no longer spuriously reschedules after explicit stop
- iOS cancel listener sends `Stop` on timeout and unblocks via `cancelCancelListener`

## [0.1.2] - 2026-04-05

### Added

- README for guest-js npm package

## [0.1.1] - 2026-04-05

### Changed

- Version bump

## [0.1.0] - 2026-04-04

### Added

- `BackgroundService<R>` trait with `init()` and `run()` lifecycle methods
- `ServiceContext<R>` with notifier, app handle, and shutdown token
- Android Foreground Service with `START_STICKY` auto-restart
- iOS BGTaskScheduler integration with configurable safety timeout
- Desktop standard Tokio task execution
- TypeScript API: `startService()`, `stopService()`, `isServiceRunning()`, `onPluginEvent()`
- Permissions system with `allow-start`, `allow-stop`, `allow-is-running`
- `Notifier` helper for fire-and-forget local notifications
- `StartConfig` with configurable `serviceLabel` and `foregroundServiceType`

[Unreleased]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.7.0...HEAD
[0.7.0]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.6.0...plugin-v0.7.0
[0.6.0]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.5.2...plugin-v0.6.0
[0.5.2]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.5.1...plugin-v0.5.2
[0.5.1]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.5.0...plugin-v0.5.1
[0.5.0]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.4.1...plugin-v0.5.0
[0.4.1]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.4.0...plugin-v0.4.1
[0.4.0]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.3.1...plugin-v0.4.0
[0.3.1]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.3.0...plugin-v0.3.1
[0.3.0]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.2.4...plugin-v0.3.0
[0.2.4]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.2.3...plugin-v0.2.4
[0.2.3]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.2.2...plugin-v0.2.3
[0.2.2]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.2.1...plugin-v0.2.2
[0.2.1]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.2.0...plugin-v0.2.1
[0.2.0]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.1.2...plugin-v0.2.0
[0.1.2]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.1.1...plugin-v0.1.2
[0.1.1]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.1.0...plugin-v0.1.1
[0.1.0]: https://github.com/dardourimohamed/tauri-background-service/releases/tag/plugin-v0.1.0
