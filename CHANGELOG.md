# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- iOS safety timer now resolves (instead of rejecting) the pending cancel invoke, enabling graceful Rust-side shutdown on the most common `BGAppRefreshTask` path
- iOS `completeBgTask` no longer spuriously reschedules `BGTaskScheduler` requests after explicit stop or expiration
- iOS cancel listener now sends `Stop` on timeout (default: 4h) and unblocks the `spawn_blocking` thread via new `cancelCancelListener` native method
- iOS `BGTaskScheduler` submission errors are now logged instead of silently swallowed

### Added

- `From<PluginInvokeError> for ServiceError` conversion for cleaner mobile error handling
- Configurable iOS `BGAppRefreshTask` and `BGProcessingTask` scheduling intervals via `PluginConfig` (`ios_earliest_refresh_begin_minutes`, `ios_earliest_processing_begin_minutes`)
- Configurable iOS `BGProcessingTask` requirements (`ios_requires_external_power`, `ios_requires_network_connectivity`) via `PluginConfig`
- `cancel_cancel_listener` bridge method to unblock the Rust cancel listener thread on timeout
- Rust cancel listener integration tests covering resolved, rejected, timeout, and join-error paths
- Documented required iOS `Info.plist` entries (`BGTaskSchedulerPermittedIdentifiers`, `UIBackgroundModes`) in Swift doc comments and Rust module docs

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

[Unreleased]: https://github.com/dardourimohamed/tauri-background-service/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/dardourimohamed/tauri-background-service/releases/tag/v0.1.0
