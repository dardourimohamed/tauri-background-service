// guest-js/index.ts
import { invoke, addPluginListener, type PluginListener } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

/** The operating system platform. */
export type Platform = 'android' | 'ios' | 'windows' | 'macos' | 'linux' | 'unknown';

/** The lifecycle mechanism used by the plugin on the current platform. */
export type LifecycleMode =
  | 'androidForegroundService'
  | 'iosBgTaskScheduler'
  | 'desktopInProcess'
  | 'desktopOsService';

/** Guarantee level for a specific background execution scenario. */
export type LifecycleGuarantee = 'guaranteed' | 'bestEffort' | 'unsupported';

/** Platform-specific background execution capabilities. */
export interface PlatformCapabilities {
  /** The current runtime platform. */
  platform: Platform;
  /** The lifecycle mechanism in use. */
  lifecycleMode: LifecycleMode;
  /** Whether the service survives the app being closed. */
  survivesAppClose: LifecycleGuarantee;
  /** Whether the service survives a device reboot. */
  survivesReboot: LifecycleGuarantee;
  /** Whether the service survives the app being force-quit. */
  survivesForceQuit: LifecycleGuarantee;
  /** Whether background execution is supported. */
  backgroundExecution: LifecycleGuarantee;
  /** Platform-specific limitations (e.g. OEM battery optimization). */
  limitations: string[];
  /** Setup steps required on this platform. */
  requiredSetup: string[];
}

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

/** Detailed lifecycle state of the background service. */
export type LifecycleState =
  | 'idle'
  | 'starting'
  | 'running'
  | 'stopping'
  | 'stopped'
  | 'recovering'
  | 'recoveryPending'
  | 'expired'
  | 'blocked'
  | 'error';

/** Severity level for a validation issue. */
export type Severity = 'error' | 'warning' | 'info';

/** A validation issue found during lifecycle setup checks. */
export interface ValidationIssue {
  severity: Severity;
  code: string;
  message: string;
  fix?: string;
  platform: Platform;
}

/** Reason the service stopped, reported in lifecycle events. */
export type StopReason =
  | 'userStop'
  | 'appStop'
  | 'platformTimeout'
  | 'platformExpiration'
  | 'nativeNotificationStop'
  | 'osRestart'
  | 'bootRecovery'
  | 'taskCompleted'
  | 'error';

/** Complete snapshot of the background service lifecycle status. */
export interface LifecycleStatus {
  state: LifecycleState;
  desiredRunning: boolean;
  recoveryEnabled: boolean;
  recoveryPending: boolean;
  recoveryReason?: string;
  lastStartConfig?: StartConfig;
  lastPlatformState?: string;
  lastPlatformError?: string;
  lastError?: string;
  platform: Platform;
  capabilities: PlatformCapabilities;
  issues: ValidationIssue[];
}

/** Snapshot of the service lifecycle status. */
export interface ServiceStatus {
  /** Current lifecycle state. */
  state: ServiceState;
  /** Last error message, if the service stopped due to an error. */
  lastError: string | null;
  /** Whether the service is desired to be running (persisted across restarts). */
  desiredRunning?: boolean;
  /** Platform-native state as reported by the OS service layer (e.g. "idle", "running", "timeout"). */
  nativeState?: string;
  /** The lifecycle mechanism in use on the current platform (e.g. "androidForegroundService", "desktopInProcess"). */
  platformMode?: string;
  /** Configuration used for the last successful start. */
  lastStartConfig?: StartConfig;
  /** Epoch milliseconds of the last heartbeat received from the service. */
  lastHeartbeatAt?: number;
  /** How many restart attempts have been made since the last clean start. */
  restartAttempt?: number;
  /** Human-readable reason for the current recovery attempt. */
  recoveryReason?: string;
  /** Last platform-specific error message. */
  platformError?: string;
}

/** Built-in plugin lifecycle events */
export type PluginEvent =
  | { type: 'started' }
  | { type: 'stopped';  reason: string }
  | { type: 'error';    message: string };

/** iOS background task scheduling status. */
export interface IOSSchedulingStatus {
  /** Whether a BGAppRefreshTask was successfully scheduled. */
  refreshScheduled: boolean;
  /** Whether a BGProcessingTask was successfully scheduled. */
  processingScheduled: boolean;
  /** Error from BGAppRefreshTask scheduling, if any. */
  refreshError?: string;
  /** Error from BGProcessingTask scheduling, if any. */
  processingError?: string;
}

/** Information about a pending iOS background task that launched the app. */
export interface PendingTaskInfo {
  /** The kind of background task: "refresh" or "processing". */
  taskKind: string;
  /** The task identifier (e.g. "com.example.app.bg-refresh"). */
  identifier: string;
  /** Epoch timestamp (seconds) when the task was received. */
  receivedAt: number;
  /** Epoch timestamp (seconds) when the pending task was consumed by auto-start. */
  consumedAt?: number | null;
}

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

/**
 * Returns the current service lifecycle state and optional error.
 *
 * @deprecated Use `getLifecycleStatus()` instead.
 */
export async function getServiceState(): Promise<ServiceStatus> {
  const status = await getLifecycleStatus();
  return {
    state: mapToServiceState(status.state),
    lastError: status.lastError ?? null,
    desiredRunning: status.desiredRunning,
    nativeState: status.lastPlatformState,
    platformMode: status.capabilities?.lifecycleMode,
    lastStartConfig: status.lastStartConfig,
    recoveryReason: status.recoveryReason,
    platformError: status.lastPlatformError,
  };
}

/** Map detailed LifecycleState to the legacy 4-value ServiceState. */
function mapToServiceState(state: LifecycleState): ServiceState {
  switch (state) {
    case 'idle':
      return 'idle';
    case 'starting':
    case 'recovering':
    case 'recoveryPending':
      return 'initializing';
    case 'running':
    case 'stopping':
      return 'running';
    case 'stopped':
    case 'expired':
    case 'blocked':
    case 'error':
      return 'stopped';
    default:
      return 'idle';
  }
}

/** Returns platform-specific background execution capabilities. */
export async function getPlatformCapabilities(): Promise<PlatformCapabilities> {
  return invoke<PlatformCapabilities>('plugin:background-service|get_platform_capabilities');
}

/**
 * Query the iOS scheduling status from the native layer.
 * Returns scheduling results on iOS, or a default status on other platforms.
 */
export async function getSchedulingStatus(): Promise<IOSSchedulingStatus> {
  return invoke<IOSSchedulingStatus>('plugin:background-service|get_scheduling_status');
}

/**
 * Query the pending iOS background task info.
 * Returns task info on iOS if the app was launched by iOS for a background task,
 * or null on non-iOS platforms / when no pending task exists.
 */
export async function getPendingBgTask(): Promise<PendingTaskInfo | null> {
  return invoke<PendingTaskInfo | null>('plugin:background-service|get_pending_bg_task');
}

/**
 * Listen to built-in plugin lifecycle events.
 * Your service can emit its own custom events via ctx.app.emit() —
 * subscribe to those separately with tauri's `listen()`.
 */
export async function onPluginEvent(
  handler: (event: PluginEvent) => void
): Promise<UnlistenFn> {
  const unlisten = await listen<PluginEvent>('background-service://event', e => handler(e.payload));

  let pluginListener: PluginListener | null = null;
  try {
    pluginListener = await addPluginListener('background-service', 'timeout', (payload: any) => {
      handler({ type: 'stopped', reason: 'timeout' });
    });
  } catch {
    // Plugin listener not available on non-Android platforms
  }

  return () => {
    unlisten();
    pluginListener?.unregister();
  };
}

// ─── Desktop OS Service Management ────────────────────────────────────
// These commands are only available when the `desktop-service` feature is enabled.
// They are no-ops (command not found) on mobile platforms.

/** OS service install/running state. */
export type OsServiceInstallState = 'notInstalled' | 'installed' | 'running';

/** Snapshot of an OS-level service's status. */
export interface OsServiceStatus {
  /** The service label (e.g. "com.example.background-service"). */
  label: string;
  /** The service manager kind (e.g. "systemd", "launchd"). */
  mode: string;
  /** Whether the service is installed and/or running. */
  installed: OsServiceInstallState;
  /** Whether the IPC connection to the service sidecar is active. */
  ipcConnected: boolean;
  /** Path to the Unix domain socket used for IPC. */
  socketPath?: string;
  /** Last error message from the OS service, if any. */
  lastError?: string;
}

/** Install the background service as an OS-managed daemon (desktop only). */
export async function installService(): Promise<void> {
  await invoke('plugin:background-service|install_service');
}

/** Uninstall the OS-managed background service daemon (desktop only). */
export async function uninstallService(): Promise<void> {
  await invoke('plugin:background-service|uninstall_service');
}

/** Start the OS-level background service (desktop only, Unix). */
export async function startOsService(): Promise<void> {
  await invoke('plugin:background-service|start_os_service');
}

/** Stop the OS-level background service (desktop only, Unix). */
export async function stopOsService(): Promise<void> {
  await invoke('plugin:background-service|stop_os_service');
}

/** Restart the OS-level background service (desktop only, Unix). */
export async function restartOsService(): Promise<void> {
  await invoke('plugin:background-service|restart_os_service');
}

/** Get the status of the OS-level background service (desktop only). */
export async function getOsServiceStatus(): Promise<OsServiceStatus> {
  return invoke<OsServiceStatus>('plugin:background-service|get_os_service_status');
}

// ─── Desired-State API ──────────────────────────────────────────────

/** Persisted desired-state for the background service. */
export interface DesiredServiceState {
  /** Whether the user wants the service running (recovery intent). */
  desiredRunning: boolean;
  /** Configuration used for the last start (or enableAutoRestart). */
  lastStartConfig?: StartConfig;
  /** Epoch millis when the service was last started. */
  lastStartEpochMs?: number;
  /** Epoch millis of the last heartbeat from the service task. */
  lastHeartbeatEpochMs?: number;
  /** Last native platform state (e.g. "timeout", "expired"). */
  lastNativeState?: string;
  /** Last platform-specific error message. */
  lastPlatformError?: string;
  /** How many restart attempts have been made since the last clean start. */
  restartAttempt: number;
  /** Whether a recovery is pending (e.g. after boot). */
  recoveryPending: boolean;
  /** Why recovery was initiated. */
  recoveryReason?: string;
}

/**
 * Enable auto-restart for the background service.
 *
 * Persists `desired_running=true` with an optional start config WITHOUT
 * starting the service. Platform recovery mechanisms will use this to
 * automatically restart the service after process kill or device reboot.
 *
 * @deprecated Use `configureRecovery({ enabled: true, config })` instead.
 */
export async function enableAutoRestart(config?: StartConfig): Promise<void> {
  await configureRecovery({ enabled: true, config });
}

/**
 * Disable auto-restart for the background service.
 *
 * Persists `desired_running=false` and clears recovery fields WITHOUT
 * stopping the service if currently running.
 *
 * @deprecated Use `configureRecovery({ enabled: false })` instead.
 */
export async function disableAutoRestart(): Promise<void> {
  await configureRecovery({ enabled: false });
}

/**
 * Get the persisted desired-state for the background service.
 *
 * Returns the current recovery intent and metadata, or null if no
 * persistence backend is configured on the current platform.
 *
 * @deprecated Use `getLifecycleStatus()` instead.
 */
export async function getDesiredServiceState(): Promise<DesiredServiceState | null> {
  const status = await getLifecycleStatus();
  return {
    desiredRunning: status.desiredRunning,
    lastStartConfig: status.lastStartConfig,
    lastNativeState: status.lastPlatformState,
    lastPlatformError: status.lastPlatformError,
    restartAttempt: 0,
    recoveryPending: status.recoveryPending,
    recoveryReason: status.recoveryReason,
  };
}

// ─── Lifecycle API ───────────────────────────────────────────────────

/**
 * Get a complete snapshot of the background service lifecycle status.
 *
 * Returns the current state, recovery intent, platform capabilities,
 * and any validation issues in a single call.
 */
export async function getLifecycleStatus(): Promise<LifecycleStatus> {
  return invoke<LifecycleStatus>('plugin:background-service|get_lifecycle_status');
}

/**
 * Configure recovery behavior for the background service.
 *
 * When enabled, the plugin persists the intent to keep the service running
 * and uses platform-specific recovery mechanisms to restart after process
 * kill or device reboot.
 *
 * @param options.enabled — `true` to enable recovery, `false` to disable.
 * @param options.config — Optional start config to persist for recovery restarts.
 */
export async function configureRecovery(options: {
  enabled: boolean;
  config?: StartConfig;
}): Promise<void> {
  await invoke('plugin:background-service|configure_recovery', {
    enabled: options.enabled,
    config: options.config,
  });
}

// ─── Setup Validator ─────────────────────────────────────────────────

/** A single setup issue found during validation. */
export interface SetupIssue {
  /** Machine-readable error code (e.g. "android_fgs_type"). */
  code: string;
  /** Human-readable description of the issue. */
  message: string;
  /** The platform this issue applies to. */
  platform: string;
  /** Suggested fix for the issue, if available. */
  fix?: string;
}

/** Result of validating background service setup prerequisites. */
export interface SetupValidationReport {
  /** True when errors is empty (warnings do not affect this). */
  ok: boolean;
  /** Blocking issues that prevent the service from working correctly. */
  errors: SetupIssue[];
  /** Non-blocking issues that may cause degraded behavior. */
  warnings: SetupIssue[];
}

/**
 * Validate the background service setup for the current platform.
 *
 * Checks platform-specific prerequisites and returns a report with
 * errors (blocking) and warnings (non-blocking). Call this after
 * initial integration to catch missing permissions, manifest entries,
 * or configuration issues.
 */
export async function validateBackgroundServiceSetup(): Promise<SetupValidationReport> {
  return invoke<SetupValidationReport>('plugin:background-service|validate_setup');
}

// ─── Typed Error Handling ───────────────────────────────────────────

/**
 * Machine-readable error code for background service errors.
 *
 * Each code corresponds to a Rust `ServiceError` variant. The
 * `normalizeBackgroundServiceError()` helper extracts these codes
 * from Tauri invoke rejection strings.
 */
export type BackgroundServiceErrorCode =
  | 'alreadyRunning'
  | 'notRunning'
  | 'init'
  | 'runtime'
  | 'platform'
  | 'pluginInvoke'
  | 'serviceInstall'
  | 'serviceUninstall'
  | 'ipc'
  | 'serviceStart'
  | 'serviceStop'
  | 'unknown';

/** A structured background service error with a typed code. */
export interface BackgroundServiceError {
  /** Machine-readable error code. */
  code: BackgroundServiceErrorCode;
  /** Human-readable error message (the original Tauri rejection string). */
  message: string;
}

/** Ordered list of Rust Display prefixes mapped to JS error codes. */
const _ERROR_PREFIXES: ReadonlyArray<[RegExp, BackgroundServiceErrorCode]> = [
  [/^Service is already running/, 'alreadyRunning'],
  [/^Service is not running/, 'notRunning'],
  [/^Initialisation failed:/, 'init'],
  [/^Runtime error:/, 'runtime'],
  [/^Platform error:/, 'platform'],
  [/^Plugin invoke error:/, 'pluginInvoke'],
  [/^Service installation failed:/, 'serviceInstall'],
  [/^Service uninstallation failed:/, 'serviceUninstall'],
  [/^IPC error:/, 'ipc'],
  [/^Service start failed:/, 'serviceStart'],
  [/^Service stop failed:/, 'serviceStop'],
];

/** Map of serde-serialized Rust variant names to JS error codes. */
const _VARIANT_NAMES: Readonly<Record<string, BackgroundServiceErrorCode>> = {
  AlreadyRunning: 'alreadyRunning',
  NotRunning: 'notRunning',
  Init: 'init',
  Runtime: 'runtime',
  Platform: 'platform',
  PluginInvoke: 'pluginInvoke',
  ServiceInstall: 'serviceInstall',
  ServiceUninstall: 'serviceUninstall',
  Ipc: 'ipc',
  ServiceStart: 'serviceStart',
  ServiceStop: 'serviceStop',
};

/**
 * Normalize a caught Tauri invoke error into a typed `BackgroundServiceError`.
 *
 * This is an **opt-in** helper — it does not change existing promise rejection
 * behavior. Wrap your `catch` blocks to get a typed error code:
 *
 * ```ts
 * try {
 *   await startService();
 * } catch (e) {
 *   const err = normalizeBackgroundServiceError(e);
 *   if (err.code === 'alreadyRunning') { /* … *\/ }
 * }
 * ```
 *
 * @param error — The value caught from a rejected `invoke()` call.
 */
export function normalizeBackgroundServiceError(error: unknown): BackgroundServiceError {
  const message = error instanceof Error ? error.message : String(error);

  // 1. Match against Rust ServiceError Display prefixes
  for (const [pattern, code] of _ERROR_PREFIXES) {
    if (pattern.test(message)) {
      return { code, message };
    }
  }

  // 2. Match against serde-serialized variant names
  //    Handles forms like "AlreadyRunning" or {"Init":"msg"}
  const trimmed = message.trim();

  // Plain variant name string
  if (trimmed in _VARIANT_NAMES) {
    return { code: _VARIANT_NAMES[trimmed], message };
  }

  // JSON object: {"VariantName": "..."}
  try {
    const parsed = JSON.parse(trimmed);
    if (typeof parsed === 'string' && parsed in _VARIANT_NAMES) {
      return { code: _VARIANT_NAMES[parsed], message };
    }
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      for (const key of Object.keys(parsed)) {
        if (key in _VARIANT_NAMES) {
          return { code: _VARIANT_NAMES[key], message };
        }
      }
    }
  } catch {
    // Not JSON — fall through to unknown
  }

  return { code: 'unknown', message };
}
