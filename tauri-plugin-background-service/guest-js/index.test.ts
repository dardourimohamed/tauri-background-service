import { describe, it, expect, vi, beforeEach } from "vitest";

const { mockInvoke, mockUnregister, capturedCallbackRef } = vi.hoisted(() => {
  const mockInvoke = vi.fn().mockResolvedValue(undefined);
  const mockUnregister = vi.fn();
  const capturedCallbackRef: { current: ((payload: unknown) => void) | null } = {
    current: null,
  };
  return { mockInvoke, mockUnregister, capturedCallbackRef };
});

vi.mock("@tauri-apps/api/core", () => ({
  invoke: mockInvoke,
  addPluginListener: vi.fn(
    (_plugin: string, _event: string, callback: (payload: unknown) => void) => {
      capturedCallbackRef.current = callback;
      return Promise.resolve({ unregister: mockUnregister });
    }
  ),
}));

const noopUnlisten = () => {};
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(() => Promise.resolve(noopUnlisten)),
}));

import { startNativeLifecycleBridge, onPlatformError } from "./index.js";

describe("startNativeLifecycleBridge", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    capturedCallbackRef.current = null;
  });

  it("returns an async function that resolves to an unlisten function", async () => {
    const unlisten = await startNativeLifecycleBridge();
    expect(typeof unlisten).toBe("function");
  });

  it("invokes native_lifecycle_event with correct payload when event received", async () => {
    await startNativeLifecycleBridge();

    expect(capturedCallbackRef.current).not.toBeNull();

    const payload = { type: "androidNotificationStop" };
    capturedCallbackRef.current!(payload);

    expect(mockInvoke).toHaveBeenCalledWith(
      "plugin:background-service|native_lifecycle_event",
      { event: { type: "androidNotificationStop" } }
    );
  });

  it("forwards fgsType in the payload when present", async () => {
    await startNativeLifecycleBridge();

    const payload = { type: "androidTimeout", fgsType: "remoteMessaging" };
    capturedCallbackRef.current!(payload);

    expect(mockInvoke).toHaveBeenCalledWith(
      "plugin:background-service|native_lifecycle_event",
      { event: { type: "androidTimeout", fgsType: "remoteMessaging" } }
    );
  });

  it("unregisters the plugin listener when unlisten is called", async () => {
    const unlisten = await startNativeLifecycleBridge();
    unlisten();
    expect(mockUnregister).toHaveBeenCalled();
  });

  it("returns a no-op unlisten when addPluginListener throws", async () => {
    const { addPluginListener } = await import("@tauri-apps/api/core");
    vi.mocked(addPluginListener).mockRejectedValueOnce(
      new Error("Plugin not available")
    );

    const unlisten = await startNativeLifecycleBridge();
    expect(typeof unlisten).toBe("function");
    unlisten(); // should not throw
  });
});

describe("onPlatformError", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    capturedCallbackRef.current = null;
  });

  it("invokes the handler with the error string from the payload", async () => {
    const seen: string[] = [];
    await onPlatformError((e) => seen.push(e));

    expect(capturedCallbackRef.current).not.toBeNull();
    capturedCallbackRef.current!({ error: "fgs_restricted: not allowed" });

    expect(seen).toEqual(["fgs_restricted: not allowed"]);
  });

  it("unregisters the plugin listener when unlisten is called", async () => {
    const unlisten = await onPlatformError(() => {});
    unlisten();
    expect(mockUnregister).toHaveBeenCalled();
  });

  it("returns a no-op unlisten when addPluginListener throws", async () => {
    const { addPluginListener } = await import("@tauri-apps/api/core");
    vi.mocked(addPluginListener).mockRejectedValueOnce(
      new Error("Plugin not available")
    );

    const unlisten = await onPlatformError(() => {});
    expect(typeof unlisten).toBe("function");
    unlisten(); // should not throw
  });
});
import type {
  StopReason,
  NotificationPermissionStatus,
} from "./index.js";
import {
  getNotificationPermissionStatus,
  requestNotificationPermission,
  onPluginEvent,
} from "./index.js";

// ── WIRE-01: scalar notification-permission contract ─────────────────
// The Rust commands return the inner String (not the `{status}` object the
// Kotlin side emits), so the TS getters/requesters expose the scalar
// `NotificationPermissionStatus` string union directly.
describe("WIRE-01: notification permission scalar contract", () => {
  beforeEach(() => {
    mockInvoke.mockReset();
  });

  it("getNotificationPermissionStatus resolves to the bare scalar string", async () => {
    // Simulate the Rust command returning the scalar string directly.
    mockInvoke.mockResolvedValue("denied");
    const result = await getNotificationPermissionStatus();
    expect(result).toBe("denied");
    // Type-level: result must be assignable to the declared union.
    const _check: NotificationPermissionStatus = result;
    expect(_check).toBe("denied");
    expect(mockInvoke).toHaveBeenCalledWith(
      "plugin:background-service|get_notification_permission_status"
    );
  });

  it("getNotificationPermissionStatus forwards every known scalar variant", async () => {
    for (const v of ["granted", "denied", "notDetermined"] as const) {
      mockInvoke.mockResolvedValue(v);
      expect(await getNotificationPermissionStatus()).toBe(v);
    }
  });

  it("requestNotificationPermission resolves to the scalar string (not void)", async () => {
    // Previously this returned Promise<void> and discarded the Rust result.
    mockInvoke.mockResolvedValue("granted");
    const result = await requestNotificationPermission();
    expect(result).toBe("granted");
    expect(mockInvoke).toHaveBeenCalledWith(
      "plugin:background-service|request_notification_permission"
    );
  });

  it("requestNotificationPermission forwards every known scalar variant", async () => {
    for (const v of ["granted", "denied"] as const) {
      mockInvoke.mockResolvedValue(v);
      expect(await requestNotificationPermission()).toBe(v);
    }
  });
});

// ── WIRE-02: StopReason vocabulary + timeout mapping ─────────────────
describe("WIRE-02: StopReason contract", () => {
  it("'processExit' is part of the StopReason union", () => {
    // Compile-time: every serialized Rust variant must be assignable here.
    const reasons: StopReason[] = [
      "userStop",
      "appStop",
      "platformTimeout",
      "platformExpiration",
      "nativeNotificationStop",
      "osRestart",
      "bootRecovery",
      "taskCompleted",
      "error",
      "processExit",
    ];
    expect(reasons).toContain("processExit");
    expect(new Set(reasons).size).toBe(reasons.length);
  });

  it("onPluginEvent maps native 'timeout' to 'platformTimeout' (not the invalid 'timeout')", async () => {
    // The native 'timeout' plugin event must be translated into a
    // PluginEvent with reason 'platformTimeout' — the Rust vocabulary has
    // no 'timeout' variant.
    const captured: { event: unknown | null } = { event: null };
    const unlisten = await onPluginEvent(event => {
      captured.event = event;
    });
    // The mock capturedCallbackRef captured the 'timeout' listener callback.
    expect(capturedCallbackRef.current).not.toBeNull();
    capturedCallbackRef.current?.({ kind: "timeout" });
    expect(captured.event).toEqual({ type: "stopped", reason: "platformTimeout" });
    unlisten();
  });
});

