// guest-js/index.ts
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export interface StartConfig {
  /** Text shown in the Android persistent foreground notification */
  serviceLabel?: string;
  /**
   * Android foreground service type.
   * Valid values: `"dataSync"` (default), `"mediaPlayback"`, `"phoneCall"`,
   * `"location"`, `"connectedDevice"`, `"mediaProjection"`, `"camera"`,
   * `"microphone"`, `"health"`, `"remoteMessaging"`, `"systemExempted"`,
   * `"shortService"`, `"specialUse"`, `"mediaProcessing"`.
   * Ignored on non-Android platforms.
   */
  foregroundServiceType?: string;
}

/** Service lifecycle state. */
export type ServiceState = 'idle' | 'initializing' | 'running' | 'stopped';

/** Snapshot of the service lifecycle status. */
export interface ServiceStatus {
  /** Current lifecycle state. */
  state: ServiceState;
  /** Last error message, if the service stopped due to an error. */
  lastError: string | null;
}

/** Built-in plugin lifecycle events */
export type PluginEvent =
  | { type: 'started' }
  | { type: 'stopped';  reason: string }
  | { type: 'error';    message: string };

/**
 * Start the background service.
 * The service struct is already registered in main.rs — this just starts it.
 */
export async function startService(config: StartConfig = {}): Promise<void> {
  await invoke('plugin:background-service|start', { config });
}

/** Stop the service and cancel the shutdown token. */
export async function stopService(): Promise<void> {
  await invoke('plugin:background-service|stop');
}

/** Returns true if the Rust service is currently running. */
export async function isServiceRunning(): Promise<boolean> {
  return invoke<boolean>('plugin:background-service|is_running');
}

/** Returns the current service lifecycle state and optional error. */
export async function getServiceState(): Promise<ServiceStatus> {
  return invoke<ServiceStatus>('plugin:background-service|get_service_state');
}

/**
 * Listen to built-in plugin lifecycle events.
 * Your service can emit its own custom events via ctx.app.emit() —
 * subscribe to those separately with tauri's `listen()`.
 */
export async function onPluginEvent(
  handler: (event: PluginEvent) => void
): Promise<UnlistenFn> {
  return listen<PluginEvent>('background-service://event', e => handler(e.payload));
}

// ─── Desktop OS Service Management ────────────────────────────────────
// These commands are only available when the `desktop-service` feature is enabled.
// They are no-ops (command not found) on mobile platforms.

/** Install the background service as an OS-managed daemon (desktop only). */
export async function installService(): Promise<void> {
  await invoke('plugin:background-service|install_service');
}

/** Uninstall the OS-managed background service daemon (desktop only). */
export async function uninstallService(): Promise<void> {
  await invoke('plugin:background-service|uninstall_service');
}
