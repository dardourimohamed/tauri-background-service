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
tauri-plugin-background-service = "0.1"
```

### npm (TypeScript API)

```bash
npm install tauri-plugin-background-service
```

## Quick Start

### 1. Implement the `BackgroundService` trait

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

Register `tauri-plugin-notification` **before** the background-service plugin:

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

### 3. Use the TypeScript API

```typescript
import {
  startService,
  stopService,
  isServiceRunning,
  onPluginEvent,
} from 'tauri-plugin-background-service';

// Start the service
await startService({ serviceLabel: 'Syncing data' });

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
unlisten();
```

## Project Layout

```
tauri-background-service/
├── tauri-plugin-background-service/   Plugin crate (Rust + TypeScript + native)
│   ├── src/                           Rust source (actor loop, trait, models)
│   ├── guest-js/                      TypeScript API bindings
│   ├── android/                       Kotlin foreground service
│   ├── ios/                           Swift BGTaskScheduler
│   ├── examples/                      Usage examples
│   ├── docs/                          Platform guides and API reference
│   └── tests/                         Integration tests
└── test-app/                          Manual test harness (Android device)
```

## Documentation

- [Getting Started](tauri-plugin-background-service/docs/getting-started.md)
- [API Reference](tauri-plugin-background-service/docs/api-reference.md)
- [Android Guide](tauri-plugin-background-service/docs/android.md)
- [iOS Guide](tauri-plugin-background-service/docs/ios.md)
- [Desktop Guide](tauri-plugin-background-service/docs/desktop.md)
- [Migration Guide](tauri-plugin-background-service/docs/migration-guide.md)
- [Troubleshooting](tauri-plugin-background-service/docs/troubleshooting.md)
- [Architecture](ARCHITECTURE.md)

## Development

```bash
# Build
cargo build

# Test
cargo test

# Lint
cargo clippy

# Build TypeScript bindings
cd tauri-plugin-background-service/guest-js && npm run build
```

See [Contributing](CONTRIBUTING.md) for the full development guide.

## Community

- [Contributing](CONTRIBUTING.md)
- [Changelog](CHANGELOG.md)
- [Security Policy](SECURITY.md)
- [Code of Conduct](CODE_OF_CONDUCT.md)

## License

SPDX-License-Identifier: MIT OR Apache-2.0
