# tauri-plugin-background-service

[![crates.io](https://img.shields.io/crates/v/tauri-plugin-background-service.svg)](https://crates.io/crates/tauri-plugin-background-service) [![docs.rs](https://docs.rs/tauri-plugin-background-service/badge.svg)](https://docs.rs/tauri-plugin-background-service) [![npm](https://img.shields.io/npm/v/tauri-plugin-background-service.svg)](https://www.npmjs.com/package/tauri-plugin-background-service) [![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](https://github.com/dardourimohamed/tauri-background-service/blob/main/LICENSE)

A Tauri v2 plugin that manages long-lived background service lifecycle across all platforms (Android, iOS, Windows, macOS, Linux).

You implement a single `BackgroundService` trait on your own struct. The plugin spawns it in a Tokio task, manages OS-specific keepalive mechanisms (Android foreground service, iOS BGTaskScheduler, desktop OS service), and provides helpers for notifications and event emission. No business logic lives in the plugin — only lifecycle management.

Use [`getPlatformCapabilities()`](./docs/api-reference.md#getplatformcapabilities) at runtime to query what the current platform can guarantee for background execution survival.

## Platform Guarantees

Background service survival varies significantly across platforms. The table below uses three guarantee levels:

| | **Guaranteed** | **Best-effort** | **Unsupported** |
|---|---|---|---|
| Meaning | Platform reliably supports this scenario. | Platform may support this, but behavior depends on OEM, battery, OS scheduling, etc. | Platform does not support this scenario. |

| Scenario | Android (FGS) | iOS (BGTaskScheduler) | Desktop in-process | Desktop OS service |
|---|---|---|---|---|
| Background execution | Guaranteed | Best-effort | Guaranteed | Guaranteed |
| Survives app close | Best-effort | Best-effort | Unsupported | Guaranteed |
| Survives reboot | Best-effort | Best-effort (scheduled only) | Unsupported | Guaranteed (if autostart enabled) |
| Survives force quit | Unsupported | Unsupported | Unsupported | Unsupported |

**Android:** Foreground Service with persistent notification. `START_STICKY` enables OS restart under memory pressure (best-effort). Boot recovery via `enableAutoRestart()`. Android 15 `dataSync` type has a 6-hour cumulative timeout. OEM battery optimization may kill services.

**iOS:** BGTaskScheduler requests periodic execution windows (~30 seconds every 15+ minutes). Force-quitting the app kills all background tasks and prevents relaunch — this is an iOS design limitation.

**Desktop:** In-process mode runs as a standard Tokio task. OS-service mode (systemd/launchd, requires `desktop-service` feature) provides guaranteed background execution and survives app close/reboot when autostart is enabled.

## Platform Support

| Capability | Android | iOS | Desktop (Win/macOS/Linux) |
|---|---|---|---|
| Background execution | Foreground Service (guaranteed) | Best-effort scheduled execution (BGTaskScheduler) | Standard Tokio task (guaranteed) |
| OS service mode | — | — | systemd / launchd (`desktop-service` feature) |
| Survives app close | Best-effort (`START_STICKY`) | Best-effort (scheduled only) | In-process: Unsupported; OS service: Guaranteed |
| Survives reboot | Best-effort (boot receiver) | Best-effort (scheduled only) | In-process: Unsupported; OS service: Guaranteed (autostart) |
| Survives force quit | Unsupported | Unsupported | Unsupported |
| Local notifications | Yes | Yes | Yes |

## Installation

### Rust

Add the plugin to your app's `Cargo.toml`:

```toml
[dependencies]
tauri = { version = "2" }
tauri-plugin-notification = "2"
tauri-plugin-background-service = "0.7"
```

### npm (TypeScript API)

```bash
npm install tauri-plugin-background-service
```

## Rust Usage

### 1. Implement the `BackgroundService` trait

Create a struct and implement `BackgroundService<R>` with `init()` and `run()` methods:

```rust
use async_trait::async_trait;
use tauri::Runtime;
use tauri_plugin_background_service::{BackgroundService, ServiceContext, ServiceError};

pub struct MyService {
    tick_count: u64,
}

impl MyService {
    pub fn new() -> Self {
        Self { tick_count: 0 }
    }
}

#[async_trait]
impl<R: Runtime> BackgroundService<R> for MyService {
    async fn init(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        // One-time setup: load config, open handles, seed state
        Ok(())
    }

    async fn run(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));

        loop {
            tokio::select! {
                _ = ctx.shutdown.cancelled() => break,
                _ = interval.tick() => {
                    self.tick_count += 1;
                    // Emit events to JS
                    let _ = ctx.app.emit("my-service://tick", self.tick_count);
                    // Show local notifications
                    ctx.notifier.show("Tick", "Service is alive");
                }
            }
        }

        Ok(())
    }
}
```

### 2. Register the plugin

In your `main.rs`, register `tauri-plugin-notification` **before** the background-service plugin:

```rust
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_background_service::init_with_service(
            || MyService::new(),
        ))
        .run(tauri::generate_context!())
        .expect("error while running application");
}
```

### 3. Add plugin configuration

In your `tauri.conf.json`, add the plugin configuration. It can be empty:

```json
{
  "plugins": {
    "background-service": {}
  }
}
```

### ServiceContext

The `ServiceContext<R>` passed to `init()` and `run()` provides:

- **`notifier`** — Fire local notifications via `ctx.notifier.show("Title", "Body")`
- **`app`** — Emit events to JS via `ctx.app.emit("my-event", &payload)`
- **`shutdown`** — A `CancellationToken` that resolves when `stopService()` is called. Always include it in `tokio::select!`
- **`service_label`** — Android notification text, always `String` (mobile only, behind `#[cfg(mobile)]`)
- **`foreground_service_type`** — Android foreground service type string, always `String` (mobile only, behind `#[cfg(mobile)]`)

## TypeScript Usage

```typescript
import {
  startService,
  stopService,
  isServiceRunning,
  getServiceState,
  onPluginEvent,
  getPlatformCapabilities,
  enableAutoRestart,
  disableAutoRestart,
  getDesiredServiceState,
  validateBackgroundServiceSetup,
  normalizeBackgroundServiceError,
} from 'tauri-plugin-background-service';

// Query platform capabilities (call early to set UI expectations)
const caps = await getPlatformCapabilities();
console.log(caps.backgroundExecution);  // 'guaranteed' | 'bestEffort' | 'unsupported'
console.log(caps.survivesReboot);       // 'guaranteed' | 'bestEffort' | 'unsupported'

// Start the service (optionally configure the Android notification label)
await startService({ serviceLabel: 'Syncing data' });

// Check if running (simple boolean)
const running = await isServiceRunning();

// Query detailed service state
const status = await getServiceState();
console.log(status.state); // 'idle' | 'initializing' | 'running' | 'stopped'
console.log(status.lastError); // null or error message
console.log(status.desiredRunning); // true | false | undefined
console.log(status.nativeState); // 'idle' | 'running' | 'timeout' | ... | undefined

// Enable auto-restart for recovery after process kill / reboot
await enableAutoRestart();

// Listen to lifecycle events
const unlisten = await onPluginEvent((event) => {
  switch (event.type) {
    case 'started':
      console.log('Service started');
      break;
    case 'stopped':
      console.log('Service stopped:', event.reason);
      break;
    case 'error':
      console.error('Service error:', event.message);
      break;
  }
});

// Validate your platform setup (checks permissions, manifest entries)
const report = await validateBackgroundServiceSetup();
if (!report.ok) {
  for (const err of report.errors) {
    console.error(`[${err.code}] ${err.message}`);
    if (err.fix) console.error(`  Fix: ${err.fix}`);
  }
}

// Typed error handling (opt-in helper)
try {
  await startService({ serviceLabel: 'Syncing' });
} catch (e) {
  const err = normalizeBackgroundServiceError(e);
  console.error(`[${err.code}] ${err.message}`);
}

// Stop the service and disable recovery
await stopService();
await disableAutoRestart();

// Clean up listener
unlisten();
```

### Desktop Service API

When the `desktop-service` Cargo feature is enabled:

```typescript
import {
  installService,
  uninstallService,
  startOsService,
  stopOsService,
  restartOsService,
  getOsServiceStatus,
} from 'tauri-plugin-background-service';

// Install as OS-level daemon (systemd / launchd)
await installService();

// Check OS service status
const status = await getOsServiceStatus();
console.log(status.installed);    // 'notInstalled' | 'installed' | 'running'
console.log(status.ipcConnected); // true | false

// Manage the OS service lifecycle
await startOsService();
await stopOsService();
await restartOsService();

// Uninstall the OS service
await uninstallService();
```

### Permissions

Add these to your app's capability configuration:

```json
{
  "permissions": [
    "background-service:allow-start",
    "background-service:allow-stop",
    "background-service:allow-is-running",
    "background-service:allow-get-service-state",
    "background-service:allow-get-platform-capabilities"
  ]
}
```

For auto-restart and desired-state:

```json
"background-service:allow-enable-auto-restart",
"background-service:allow-disable-auto-restart",
"background-service:allow-get-desired-service-state"
```

For setup validation:

```json
"background-service:allow-validate-setup"
```

For desktop service mode, also add:

```json
"background-service:allow-install-service",
"background-service:allow-uninstall-service",
"background-service:allow-start-os-service",
"background-service:allow-stop-os-service",
"background-service:allow-restart-os-service",
"background-service:allow-get-os-service-status"
```

## Platform Notes

### Android

The plugin uses a Foreground Service with a persistent notification to reduce the likelihood of the OS killing the process while backgrounded. Required additions to your app's `AndroidManifest.xml` (the plugin's manifest already declares these):

- `FOREGROUND_SERVICE` and `FOREGROUND_SERVICE_DATA_SYNC` permissions
- `POST_NOTIFICATIONS` runtime permission (requested automatically on Android 13+)
- `foregroundServiceType="dataSync"` on the service declaration (see [Android Guide](./docs/android.md) for all 14 valid types)
- `stopWithTask="false"` ensures the service survives when the user swipes the app away
- `START_STICKY` causes the OS to restart the service if killed under memory pressure

When the service is restarted by the OS, the Rust process is new. Persist any state you need to restore in `run()` and reload it in `init()`.

### iOS

iOS background execution is **best-effort scheduled execution**. The plugin uses `BGTaskScheduler` to request periodic execution windows (~30 seconds every 15+ minutes). `startService()` returns a structured scheduling result indicating which task types were accepted. Force-quitting the app kills all background tasks and prevents relaunch. Required `Info.plist` additions:

```xml
<key>BGTaskSchedulerPermittedIdentifiers</key>
<array>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-refresh</string>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-processing</string>
</array>
<key>UIBackgroundModes</key>
<array>
    <string>fetch</string>
    <string>processing</string>
</array>
```

While the app is foregrounded, your `run()` loop executes continuously. When backgrounded, Tokio freezes after ~30 seconds. Design your service to handle intermittent execution windows gracefully.

### Desktop (Windows, macOS, Linux)

No special OS integration is needed. The service runs as a standard Tokio task and continues as long as the app process is alive.

For OS-level daemon mode (systemd / launchd), enable the `desktop-service` Cargo feature:

```toml
tauri-plugin-background-service = { version = "0.7", features = ["desktop-service"] }
```

## Links

**Documentation** (relative paths — works on GitHub and crates.io):
- [Getting Started](./docs/getting-started.md)
- [API Reference](./docs/api-reference.md)
- [Android Guide](./docs/android.md)
- [iOS Guide](./docs/ios.md)
- [Desktop Guide](./docs/desktop.md)
- [Troubleshooting](./docs/troubleshooting.md)
- [Migration Guide](./docs/migration-guide.md)

**Community** (absolute URLs — required for crates.io compatibility):
- [Contributing](https://github.com/dardourimohamed/tauri-background-service/blob/main/CONTRIBUTING.md)
- [Changelog](https://github.com/dardourimohamed/tauri-background-service/blob/main/CHANGELOG.md)
- [Security](https://github.com/dardourimohamed/tauri-background-service/blob/main/SECURITY.md)
- [Architecture](https://github.com/dardourimohamed/tauri-background-service/blob/main/ARCHITECTURE.md)

## License

SPDX-License-Identifier: MIT OR Apache-2.0
