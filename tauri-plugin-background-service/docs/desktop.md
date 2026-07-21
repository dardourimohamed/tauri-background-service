# Desktop Platform Guide

This guide covers desktop-specific behavior for the background service plugin (Linux, macOS, Windows). The OS-service daemon mode is **Unix-only** (Linux systemd user service, macOS launchd agent); Windows is supported in-process only.

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
tauri-plugin-background-service = { version = "1.0", features = ["desktop-service"] }
```

This pulls in the `service-manager` crate and adds six Unix OS-service Tauri
commands (`install_service`, `uninstall_service`, `start_os_service`,
`stop_os_service`, `restart_os_service`, `get_os_service_status`). Windows
remains in-process; the OS-service commands return an unsupported-platform
error there.

### Configuration

Configure the desktop service mode in your Tauri plugin configuration:

```json
{
    "plugins": {
        "background-service": {
            "desktopServiceMode": "osService",
            "desktopServiceLabel": "com.example.myapp.background",
            "desktopServiceAutostart": true,
            "desktopStartServiceIfMissing": true,
            "desktopServiceStartTimeoutMs": 5000
        }
    }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `desktopServiceMode` | `"inProcess"` | `"inProcess"` runs in the app process; `"osService"` runs as an OS daemon |
| `desktopServiceLabel` | Auto-derived from app identifier | Service label for the OS service manager |
| `desktopServiceAutostart` | `false` | Whether the OS service starts automatically on boot (Linux) or login (macOS). Only applies when `desktopServiceMode` is `"osService"`. |
| `desktopStartServiceIfMissing` | `false` | When `true`, calling `startService()` automatically starts the OS service sidecar if the IPC connection is not available. Only applies when `desktopServiceMode` is `"osService"`. |
| `desktopServiceStartTimeoutMs` | `5000` | Timeout in milliseconds to wait for the IPC connection after starting the OS service sidecar. Only applies when `desktopStartServiceIfMissing` is `true`. |

### Automatic Sidecar Recovery

When `desktopStartServiceIfMissing` is `true` and the service is in `osService` mode, the plugin can automatically recover a disconnected sidecar:

1. `startService()` is called from the GUI process
2. The IPC client detects the connection is disconnected
3. The plugin starts the OS service via the service manager (`systemctl --user start` / `launchctl load`)
4. The plugin polls the IPC connection until it becomes available (500 ms intervals)
5. If the connection is established within the timeout, the start request proceeds normally
6. If the timeout elapses, `startService()` returns an IPC error with the socket path

The timeout is controlled by `desktopServiceStartTimeoutMs` (default: 5000 ms). For sidecars that take longer to initialize (e.g., large WASM modules or network setup), increase this value:

```json
{
    "plugins": {
        "background-service": {
            "desktopServiceStartTimeoutMs": 10000
        }
    }
}
```

When `desktopStartServiceIfMissing` is `false` (the default), a disconnected IPC connection causes `startService()` to return an IPC error immediately without attempting recovery.

> **Note:** The OS service must be installed (via `installService()`) for automatic recovery to work. If the service is not installed, the start attempt fails with a platform error from the OS service manager.

### Autostart

When `desktopServiceAutostart` is `true`, the plugin configures the OS service to start automatically:

- **Linux (systemd):** The service unit includes an `[Install]` section with `WantedBy=default.target`. The service starts on user login.
- **macOS (launchd):** The plist sets `RunAtLoad=true`. The service starts on user login.

> **Note:** On macOS, the plist includes `Disabled=true` by default (matching the `service-manager` crate convention). The service must be explicitly started via `installService()` followed by `startOsService()`, which removes the `Disabled` key and reloads the plist. Once loaded, autostart takes effect on subsequent logins.

### Systemd Lingering

On Linux, systemd user services only run while the user has an active login session. For services to survive logout (and run at boot before login), **lingering must be enabled**:

```bash
loginctl enable-linger
```

Verify lingering is enabled:

```bash
loginctl show-user "$USER" -p Linger
# Expected output: Linger=yes
```

Without lingering, the OS service stops when the user logs out and does not restart until the next login. The plugin does not enable lingering automatically — it is a system administration task.

### macOS Sandbox

OS-service mode is **incompatible** with macOS App Sandbox. Sandboxed apps cannot:

- Write to `~/Library/LaunchAgents/` (where launchd plists are stored)
- Use `launchctl` to load/unload services
- Run background processes outside the sandbox

If your app is sandboxed (e.g. distributed via the Mac App Store), use the default `inProcess` mode instead. OS-service mode is only suitable for non-sandboxed apps distributed outside the App Store.

### Architecture

In `osService` mode, the plugin uses a **sidecar + IPC** architecture:

```
GUI process (Tauri app):
  → IpcClient connects to the Unix domain socket
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
use tauri_plugin_background_service::headless_main;

fn main() {
    let app = tauri::Builder::default()
        .build(tauri::generate_context!())
        .expect("failed to build headless app");
    headless_main(
        || Box::new(MyBackgroundService::new()),
        app.handle().clone(),
    );
}
```

Add to your app's `Cargo.toml`:

```toml
[[bin]]
name = "background-service"
path = "src/bin/background_service.rs"
```

Configure Tauri to bundle the sidecar via `externalBin` in `tauri.conf.json`.

### OS Service Management API

When the `desktop-service` feature is enabled and `desktopServiceMode` is `"osService"`, six OS service management functions are available in TypeScript:

```typescript
import {
  installService,
  uninstallService,
  startOsService,
  stopOsService,
  restartOsService,
  getOsServiceStatus,
  type OsServiceStatus,
  type OsServiceInstallState,
} from 'tauri-plugin-background-service';
```

#### Commands

| Function | Description |
|----------|-------------|
| `installService()` | Install the service as an OS-level daemon (systemd user unit or launchd agent). Writes the service unit/plist and optionally enables autostart. |
| `uninstallService()` | Remove the OS-level service. Stops the service if running, then removes the unit/plist file. |
| `startOsService()` | Start the OS service. On Unix, delegates to the service manager (`systemctl --user start` / `launchctl load`). |
| `stopOsService()` | Stop the OS service. On Unix, delegates to the service manager (`systemctl --user stop` / `launchctl unload`). |
| `restartOsService()` | Restart the OS service. Stop-then-start: propagates any stop error, waits boundedly for the IPC disconnect / native stopped status, then starts (or returns a timeout). |
| `getOsServiceStatus()` | Query the current OS service status: label, mode, install state, IPC connection, socket path, and last error. |

#### Usage

```typescript
// Install and start the OS service
await installService();
await startOsService();

// Check status
const status = await getOsServiceStatus();
console.log(status.label);           // "com.example.myapp.background"
console.log(status.mode);            // "systemd" | "launchd"
console.log(status.installed);       // "notInstalled" | "installed" | "running"
console.log(status.ipcConnected);    // true | false
console.log(status.socketPath);      // "/run/user/1000/com.example.myapp.background.sock"

// Restart the service
await restartOsService();

// Stop and uninstall
await stopOsService();
await uninstallService();
```

#### `OsServiceStatus` type

```typescript
interface OsServiceStatus {
  /** The service label (e.g. "com.example.background-service"). */
  label: string;
  /** The service manager kind (e.g. "systemd", "launchd"). */
  mode: string;
  /** Whether the service is installed and/or running. */
  installed: OsServiceInstallState;
  /** Whether the IPC connection to the service sidecar is active. */
  ipcConnected: boolean;
  /** Path to the Unix domain socket used for IPC. Omitted when not available. */
  socketPath?: string;
  /** Last error message from the OS service. Omitted when no error. */
  lastError?: string;
}
```

#### `OsServiceInstallState` type

```typescript
type OsServiceInstallState = 'notInstalled' | 'installed' | 'running';
```

| Value | Meaning |
|-------|---------|
| `'notInstalled'` | The OS service is not installed. |
| `'installed'` | The OS service is installed but not currently running. |
| `'running'` | The OS service is installed and currently running. |

#### Platform Support

| Platform | Service Manager | Supported | Notes |
|----------|----------------|-----------|-------|
| Linux | systemd (user unit) | Yes | Requires `loginctl enable-linger` for services to survive logout. |
| macOS | launchd (user agent) | Yes | Incompatible with App Sandbox. |
| Windows | — | **No (in-process only)** | OS-service daemon support was removed; the six OS-service commands return an unsupported-platform error. Use in-process mode on Windows. |

### Permissions

Add the desktop service permissions to your capabilities:

```json
{
  "permissions": [
    "background-service:default",
    "background-service:allow-install-service",
    "background-service:allow-uninstall-service",
    "background-service:allow-start-os-service",
    "background-service:allow-stop-os-service",
    "background-service:allow-restart-os-service",
    "background-service:allow-get-os-service-status",
    "background-service:allow-get-service-state"
  ]
}
```

### Platform Notes

| Platform | Service Manager | Socket Path |
|----------|----------------|-------------|
| Linux | systemd (user unit) | `$XDG_RUNTIME_DIR/{label}.sock` |
| macOS | launchd (user agent) | `/tmp/{label}.sock` |

> Windows has no OS-service socket — it is in-process only.

## IPC Transport Layer

In `osService` mode, the GUI process communicates with the sidecar via an IPC
transport layer using length-prefixed JSON frames over a **Unix domain socket**
(Linux/macOS). There is no Windows transport — Windows is in-process only.

### Protocol

- **Framing:** Length-prefixed JSON frames (**4-byte big-endian** `u32` length prefix + JSON payload)
- **Max frame size:** 16 MB
- **Encoding:** UTF-8 JSON

### Message Types

| Type | Direction | Purpose |
|------|-----------|---------|
| `IpcRequest` | Client → Server | `Start`, `Stop`, `IsRunning`, `GetState` commands |
| `IpcResponse` | Server → Client | Command results |
| `IpcEvent` | Server → Client | Streaming events (started, stopped, error) |

### Persistent Client

The IPC client maintains a persistent connection with reconnect backoff (via
the `backon` crate). Reconnect is **bounded** — there is no infinite backoff,
and pending requests are subject to a timeout rather than queuing forever. The
local `desired_running` mirror is updated only on successful `Start`/`Stop`
replies, so a disconnect after a successful start surfaces as `recoveryPending`
and a disconnect after a successful stop surfaces as `stopped`.

> **No daemon notification sink.** The headless sidecar does not post user
> notifications itself. Notifications route through `tauri-plugin-notification`
> in the GUI process (the `Notifier` is only wired in the app process); the
> daemon side has no notification surface.
