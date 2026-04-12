# Getting Started

This guide walks you through adding a background service to a Tauri v2 app. By the end, you'll have a service that runs in the background, responds to stop/start commands from JavaScript, and shuts down cleanly.

## Prerequisites

| Requirement | Version | Notes |
|-------------|---------|-------|
| Tauri | v2 | [tauri.app](https://v2.tauri.app) |
| Rust | 1.77.2+ | Matches `rust-version` in `Cargo.toml` |
| Node.js | 20+ | For the JavaScript bindings |
| Android SDK | 34 | For Android targets `[Android]` |
| Xcode | 15+ | For iOS targets `[iOS]` |

> **Checkpoint:** Run `rustc --version` and confirm `>= 1.77.2`. For mobile, verify `sdkmanager --list` shows platform 34 (Android) or `xcodebuild -version` shows 15+ (iOS).

## Add Dependencies

### Rust

Add the plugin and its notification dependency to your app's `Cargo.toml`:

```toml
[dependencies]
tauri-plugin-background-service = "0.5"
tauri-plugin-notification = "2"
```

### JavaScript

Install the guest-js bindings:

```bash
npm install tauri-plugin-background-service
```

> **Checkpoint:** Run `cargo check` — it should resolve and compile without errors.

## Implement BackgroundService

Create a struct that implements the `BackgroundService<R>` trait. You must implement two async methods:

- `init()` — called once before the service starts. Use it for setup (database connections, event listeners, etc.).
- `run()` — the main service loop. **Must** use `tokio::select!` with `ctx.shutdown.cancelled()` for cooperative cancellation.

```rust
use async_trait::async_trait;
use std::time::Duration;
use tauri::Runtime;
use tauri_plugin_background_service::{BackgroundService, ServiceContext, ServiceError};

struct MyService {
    counter: u64,
}

impl MyService {
    fn new() -> Self {
        Self { counter: 0 }
    }
}

#[async_trait]
impl<R: Runtime> BackgroundService<R> for MyService {
    async fn init(
        &mut self,
        _ctx: &ServiceContext<R>,
    ) -> Result<(), ServiceError> {
        // One-time setup goes here.
        Ok(())
    }

    async fn run(
        &mut self,
        ctx: &ServiceContext<R>,
    ) -> Result<(), ServiceError> {
        let mut interval = tokio::time::interval(Duration::from_secs(10));

        loop {
            tokio::select! {
                // Cooperative cancellation — always include this branch.
                _ = ctx.shutdown.cancelled() => {
                    break;
                }
                _ = interval.tick() => {
                    self.counter += 1;
                    // Send a local notification on the first tick.
                    if self.counter == 1 {
                        ctx.notifier.show("Background Service", "Service is running");
                    }
                }
            }
        }

        Ok(())
    }
}
```

**What this does:**

- `MyService` holds state (`counter`) that persists across `init()` and `run()`.
- `init()` runs once before `run()`. Return `Err(ServiceError::Init(...))` to abort startup.
- `run()` loops every 10 seconds, incrementing the counter. When `stopService()` is called from JS, the `ctx.shutdown.cancelled()` branch fires and the loop exits cleanly.
- The factory pattern (next step) creates a **fresh** instance each time the service starts.

> **Checkpoint:** Run `cargo check` — the trait implementation must compile.

## Register the Plugin

In your `main.rs`, register both the notification plugin and the background service plugin:

```rust
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_background_service::init_with_service(
            || MyService::new(),
        ))
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

**What this does:**

- `tauri_plugin_notification::init()` must be registered **before** the background service because the service uses `ctx.notifier.show()` internally.
- `init_with_service(|| MyService::new())` takes a **factory closure** — a zero-arg function that returns a fresh service instance. The plugin calls this factory each time the service starts, so state is never carried over from a previous run.

> **Checkpoint:** The app compiles and launches. The service is registered but not yet started.

## Add Permissions

Create or update a capabilities file in `src-tauri/capabilities/` (e.g., `default.json`) to grant the necessary permissions:

```json
{
  "identifier": "default",
  "description": "Capability for the main window",
  "windows": ["main"],
  "permissions": [
    "background-service:default"
  ]
}
```

The `background-service:default` permission grants access to all commands:

| Command | Permission |
|---------|------------|
| Start service | `allow-start` |
| Stop service | `allow-stop` |
| Check if running | `allow-is-running` |
| Query service state | `allow-get-service-state` |

To grant permissions individually instead:

```json
"permissions": [
  "background-service:allow-start",
  "background-service:allow-stop",
  "background-service:allow-is-running",
  "background-service:allow-get-service-state"
]
```

> **Checkpoint:** Run `cargo check` — Tauri validates capabilities at build time.

## Start and Stop from JavaScript

Use the JavaScript bindings to control the service lifecycle:

```typescript
import {
  startService,
  stopService,
  isServiceRunning,
  getServiceState,
  onPluginEvent,
} from "tauri-plugin-background-service";

// Start the service with optional configuration.
await startService({
  serviceLabel: "Syncing data",           // Android notification text
  foregroundServiceType: "dataSync",      // Android foreground service type
});

// Listen to lifecycle events.
const unlisten = await onPluginEvent((event) => {
  switch (event.type) {
    case "started":
      console.log("Service started");
      break;
    case "stopped":
      console.log("Service stopped:", event.reason);
      break;
    case "error":
      console.error("Service error:", event.message);
      break;
  }
});

// Check status.
const running = await isServiceRunning();
console.log("Running:", running);

// Stop the service when done.
await stopService();

// Clean up the event listener.
unlisten();
```

**What this does:**

- `startService()` sends a command to the Rust actor, which creates a fresh `MyService` via the factory, calls `init()`, then spawns `run()` in a Tokio task.
- `onPluginEvent()` listens for `started`, `stopped`, and `error` events emitted by the plugin. Your service can emit custom events separately using `ctx.app.emit()`.
- `stopService()` cancels the `CancellationToken`, which triggers the `ctx.shutdown.cancelled()` branch in your `run()` loop.
- Always call `unlisten()` to clean up the event listener.

> **Checkpoint:** Build and run the app. Call `startService()` from the browser console or a button click. Verify the notification appears and `isServiceRunning()` returns `true`.

## Verify

### Desktop

```bash
cargo tauri dev
```

Open the browser devtools console and run:

```javascript
const { startService, isServiceRunning } = await import("tauri-plugin-background-service");
await startService();
console.log(await isServiceRunning()); // true
```

Expected output in the browser devtools console:

```
true
```

### Android

```bash
cargo tauri android dev
```

A persistent notification labeled "Service running" (or your custom `serviceLabel`) appears in the Android notification drawer.

### iOS

```bash
cargo tauri ios dev
```

The service runs while the app is in the foreground. Background execution is limited to short windows (~30 seconds) managed by BGTaskScheduler.

> **Checkpoint:** The service starts, runs, and stops without panics or errors in the console.

## Next Steps

- **[API Reference](./api-reference.md)** — Full Rust and TypeScript API documentation with types, parameters, and platform behavior.
- **[Android Guide](./android.md)** — Foreground service permissions, auto-restart mechanism, and OEM-specific workarounds.
- **[iOS Guide](./ios.md)** — BGTaskScheduler setup, Info.plist entries, and background execution limitations.
- **[Desktop Guide](./desktop.md)** — Desktop-specific behavior and use cases.
- **[Troubleshooting](./troubleshooting.md)** — Common issues and solutions organized by platform.
