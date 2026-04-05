# tauri-plugin-background-service

[![crates.io](https://img.shields.io/crates/v/tauri-plugin-background-service.svg)](https://crates.io/crates/tauri-plugin-background-service) [![docs.rs](https://docs.rs/tauri-plugin-background-service/badge.svg)](https://docs.rs/tauri-plugin-background-service) [![npm](https://img.shields.io/npm/v/tauri-plugin-background-service.svg)](https://www.npmjs.com/package/tauri-plugin-background-service) [![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](https://github.com/dardourimohamed/tauri-background-service/blob/main/LICENSE)

A Tauri v2 plugin that manages long-lived background service lifecycle across all platforms (Android, iOS, Windows, macOS, Linux).

You implement a single `BackgroundService` trait on your own struct. The plugin spawns it in a Tokio task, keeps the OS from killing it on mobile, and provides helpers for notifications and event emission. No business logic lives in the plugin — only lifecycle management.

## Platform Support

| Capability | Android | iOS | Desktop (Win/macOS/Linux) |
|---|---|---|---|
| Service runs in background | Foreground Service | Best-effort BGTask | Standard Tokio task |
| Service survives app close | `START_STICKY` | No | No |
| Local notifications | Yes | Yes | Yes |

## Installation

### Rust

Add the plugin to your app's `Cargo.toml`:

```toml
[dependencies]
tauri = { version = "2" }
tauri-plugin-notification = "2"
tauri-plugin-background-service = { path = "../tauri-plugin-background-service" }
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

### ServiceContext

The `ServiceContext<R>` passed to `init()` and `run()` provides:

- **`notifier`** — Fire local notifications via `ctx.notifier.show("Title", "Body")`
- **`app`** — Emit events to JS via `ctx.app.emit("my-event", &payload)`
- **`shutdown`** — A `CancellationToken` that resolves when `stopService()` is called. Always include it in `tokio::select!`

## TypeScript Usage

```typescript
import {
  startService,
  stopService,
  isServiceRunning,
  onPluginEvent,
} from 'tauri-plugin-background-service';

// Start the service (optionally configure the Android notification label)
await startService({ serviceLabel: 'Syncing data' });

// Check status
const running = await isServiceRunning();

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

// Stop the service
await stopService();

// Clean up listener
unlisten();
```

### Permissions

Add these to your app's capability configuration:

```json
{
  "permissions": [
    "background-service:allow-start",
    "background-service:allow-stop",
    "background-service:allow-is-running"
  ]
}
```

## Platform Notes

### Android

The plugin uses a Foreground Service with a persistent notification to keep the process alive. Required additions to your app's `AndroidManifest.xml` (the plugin's manifest already declares these):

- `FOREGROUND_SERVICE` and `FOREGROUND_SERVICE_DATA_SYNC` permissions
- `POST_NOTIFICATIONS` runtime permission (requested automatically on Android 13+)
- `foregroundServiceType="dataSync"` on the service declaration
- `stopWithTask="false"` ensures the service survives when the user swipes the app away
- `START_STICKY` causes the OS to restart the service if killed under memory pressure

When the service is restarted by the OS, the Rust process is new. Persist any state you need to restore in `run()` and reload it in `init()`.

### iOS

iOS background execution is **best-effort**. The plugin uses `BGTaskScheduler` to request periodic execution windows (~30 seconds every 15+ minutes). Required `Info.plist` additions:

```xml
<key>BGTaskSchedulerPermittedIdentifiers</key>
<array>
    <string>$(PRODUCT_BUNDLE_IDENTIFIER).bg-refresh</string>
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
