import { describe, it, expect, vi, beforeEach } from "vitest";

const { mockInvoke, mockUnregister, capturedCallbackRef } = vi.hoisted(() => {
  const mockInvoke = vi.fn().mockResolvedValue(undefined);
  const mockUnregister = vi.fn();
  const capturedCallbackRef: { current: ((payload: any) => void) | null } = {
    current: null,
  };
  return { mockInvoke, mockUnregister, capturedCallbackRef };
});

vi.mock("@tauri-apps/api/core", () => ({
  invoke: mockInvoke,
  addPluginListener: vi.fn(
    (_plugin: string, _event: string, callback: (payload: any) => void) => {
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
