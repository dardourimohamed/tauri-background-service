# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/dardourimohamed/tauri-background-service/compare/plugin-v0.5.2...HEAD
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
