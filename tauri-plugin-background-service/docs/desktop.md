# Desktop Platform Guide

This guide covers desktop-specific behavior for the background service plugin (Linux, macOS, Windows).

## In-Process Mode (Default)

On desktop platforms, the background service runs as a **standard Tokio async task** by default. There is no OS-level keepalive mechanism — the service lives as long as the application process.

### Architecture

```
JS: startService()
  → Tauri Command (start)
    → Actor: handle_start()
      → No mobile keepalive (state.mobile is None)
      → tauri::async_runtime::spawn(service task)
        → service.init(&ctx)
        → service.run(&ctx)  ← runs until cancelled or returns
```

Unlike Android (foreground service) and iOS (BGTaskScheduler), desktop has no OS integration. The actor simply spawns the service task and tracks it with a `CancellationToken`.

## No Special Permissions

Desktop platforms require no special permissions, manifest entries, or configuration. The service runs with the same privileges as the application process.

## Service Lifecycle

1. **Start**: `handle_start()` creates a `CancellationToken`, increments the generation counter, and spawns the service task via `tauri::async_runtime::spawn()`.
2. **Run**: The service's `run()` method executes asynchronously. Use `tokio::select!` with `ctx.shutdown.cancelled()` to handle cooperative cancellation.
3. **Stop**: `handle_stop()` cancels the token. The service detects cancellation in `tokio::select!` and returns.
4. **Completion**: The spawned task emits `PluginEvent::Stopped { reason: "completed" }` and fires the `on_complete` callback.

## Cancellation

The only shutdown path is cooperative cancellation via `CancellationToken`:

```rust
async fn run(&mut self, ctx: &ServiceContext<tauri::Wry>) -> Result<(), ServiceError> {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = ctx.shutdown.cancelled() => {
                // Clean up and exit
                break;
            }
            _ = interval.tick() => {
                // Do periodic work
            }
        }
    }
    Ok(())
}
```

Always include `ctx.shutdown.cancelled()` in `tokio::select!`. Without it, `stopService()` will cancel the token but `run()` will never check it.

## Use Cases

Desktop background services are well-suited for:

- **Long-running synchronization**: Continuously sync data with a remote server
- **WebSocket connections**: Maintain persistent connections for real-time updates
- **Periodic tasks**: Run maintenance, cleanup, or polling at regular intervals
- **File watching**: Monitor filesystem changes and react
- **Local server**: Run a local HTTP/WebSocket server alongside the app

## Notification Support

Use the `Notifier` API to display desktop notifications from your background service:

```rust
async fn run(&mut self, ctx: &ServiceContext<tauri::Wry>) -> Result<(), ServiceError> {
    ctx.notifier.show("Sync Complete", "All files are up to date");
    Ok(())
}
```

`Notifier.show(title, body)` uses `tauri-plugin-notification` under the hood. On desktop, notifications appear in the system notification center (Notification Center on macOS, D-Bus notifications on Linux, Action Center on Windows).

## Limits

Desktop has essentially no OS-imposed limits on background execution:

| Aspect | Desktop | Android | iOS |
|--------|---------|---------|-----|
| Execution time | Unlimited (while app runs) | Unlimited (foreground service) | ~30 seconds per window |
| OS restart | No | Yes (`START_STICKY`) | No |
| Permissions | None | Multiple required | Info.plist entries |
| Notification | System notification center | Foreground notification | System notification |
| Keepalive | None (plain task) | Foreground service | BGTaskScheduler |

The service runs for as long as the application process is alive. When the user closes the app, the process exits and the service stops.

## Debugging

Desktop debugging is straightforward — use standard Rust logging and your IDE's debugger:

```bash
# Run with debug logging
RUST_LOG=debug cargo tauri dev

# Filter for plugin-specific logs
RUST_LOG=tauri_plugin_background_service=debug cargo tauri dev
```

### Common Issues

**Service stops when app window is closed:**
This is expected — closing the last window exits the app process on desktop. Use `tauri::Builder::on_window_event` to prevent window close if the service is running:

```rust
app.on_window_event(|window, event| {
    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        // Check if service is running before allowing close
        let manager = window.state::<ServiceManagerHandle<tauri::Wry>>();
        // Use a channel or flag to communicate with the actor
    }
});
```

**Service doesn't respond to stopService():**
Verify your `run()` implementation uses `tokio::select!` with `ctx.shutdown.cancelled()`. Without it, the cancellation token is cancelled but `run()` never checks it.

## OS Service Mode

The `desktop-service` Cargo feature enables running the background service as an OS-level daemon (systemd on Linux, launchd on macOS). In this mode, the service survives app restarts and runs independently of the GUI process.

### Feature Flag

Enable the `desktop-service` feature in your app's `Cargo.toml`:

```toml
[dependencies]
tauri-plugin-background-service = { version = "0.5", features = ["desktop-service"] }
```

This pulls in the `service-manager` crate and adds three additional Tauri commands: `install_service`, `uninstall_service`, and `get_service_state`.

### Configuration

Configure the desktop service mode in your Tauri plugin configuration:

```json
{
    "plugins": {
        "background-service": {
            "desktopServiceMode": "osService",
            "desktopServiceLabel": "com.example.myapp.background"
        }
    }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `desktopServiceMode` | `"inProcess"` | `"inProcess"` runs in the app process; `"osService"` runs as an OS daemon |
| `desktopServiceLabel` | Auto-derived from app identifier | Service label for the OS service manager |

### Architecture

In `osService` mode, the plugin uses a **sidecar + IPC** architecture:

```
GUI process (Tauri app):
  → IpcClient connects to Unix socket / Windows named pipe
  → Sends IpcRequest (Start/Stop/IsRunning)
  → Receives IpcResponse + IpcEvent

Sidecar process (headless binary):
  → Binds to socket path
  → IpcServer translates requests to ManagerCommand
  → Runs BackgroundService in-process
  → Streams events back to connected clients
```

### Sidecar Binary

Create a headless binary that runs the background service:

```rust
// src/bin/background_service.rs
use tauri_plugin_background_service::desktop::headless_main;

fn main() {
    headless_main(|| MyService::new());
}
```

Add to your app's `Cargo.toml`:

```toml
[[bin]]
name = "background-service"
path = "src/bin/background_service.rs"
```

Configure Tauri to bundle the sidecar via `externalBin` in `tauri.conf.json`.

### TypeScript API

When the `desktop-service` feature is enabled, three additional functions are available:

```typescript
import {
  installService,
  uninstallService,
  getServiceState,
} from 'tauri-plugin-background-service';

// Install the OS service
await installService();

// Check service state
const status = await getServiceState();
console.log(status.state); // 'idle' | 'initializing' | 'running' | 'stopped'

// Uninstall the OS service
await uninstallService();
```

### Permissions

Add the desktop service permissions to your capabilities:

```json
{
  "permissions": [
    "background-service:default",
    "background-service:allow-install-service",
    "background-service:allow-uninstall-service",
    "background-service:allow-get-service-state"
  ]
}
```

### Platform Notes

| Platform | Service Manager | Socket Path |
|----------|----------------|-------------|
| Linux | systemd (user unit) | `$XDG_RUNTIME_DIR/{label}.sock` |
| macOS | launchd (user agent) | `/tmp/{label}.sock` |
| Windows | Windows Service | `\\.\pipe\{label}` |

## IPC Transport Layer

In `osService` mode, the GUI process communicates with the sidecar via an IPC transport layer using length-prefixed JSON frames over Unix sockets (Linux/macOS) or Windows named pipes.

### Protocol

- **Framing:** Length-prefixed JSON frames (4-byte little-endian length prefix + JSON payload)
- **Max frame size:** 16 MB
- **Encoding:** UTF-8 JSON

### Message Types

| Type | Direction | Purpose |
|------|-----------|---------|
| `IpcRequest` | Client → Server | `Start`, `Stop`, `IsRunning`, `GetState` commands |
| `IpcResponse` | Server → Client | Command results |
| `IpcEvent` | Server → Client | Streaming events (started, stopped, error) |

### Persistent Client

The IPC client maintains a persistent connection with exponential backoff reconnect. If the sidecar is temporarily unavailable, the client retries automatically without dropping pending requests.
