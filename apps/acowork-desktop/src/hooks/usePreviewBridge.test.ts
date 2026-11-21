// @vitest-environment jsdom
/**
 * `usePreviewBridge` — the two "the page asked for something the app cannot do"
 * paths, which are the only ones that produce *visible* feedback.
 *
 * The download case is a regression guard. Rust refuses the download (no download
 * shelf in the app) and the refusal used to end there, so the link looked broken.
 * The contract is that the refusal reaches the user as a toast whose action hands
 * an *openable* URL to the system browser: the download URL when it is `http(s)`,
 * the page itself when the download was generated in memory, and no action at all
 * when neither is openable (a dead button is the failure being fixed).
 */
import { renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const bridge = vi.hoisted(() => ({
  state: null as null | ((s: unknown) => void),
  newWindow: null as null | ((r: unknown) => void),
  download: null as null | ((r: { url: string; from: string }) => void),
  unlistenCalls: 0,
}));

const openExternal = vi.hoisted(() => vi.fn(() => Promise.resolve()));

vi.mock("../lib/previewHost", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/previewHost")>();
  const share = (slot: "state" | "newWindow" | "download") => (handler: never) => {
    (bridge as Record<string, unknown>)[slot] = handler;
    return Promise.resolve(() => {
      bridge.unlistenCalls += 1;
    });
  };
  return {
    // Keep the pure helpers real: `downloadActionUrl` is the behaviour under test.
    downloadActionUrl: actual.downloadActionUrl,
    subscribePreviewState: share("state"),
    subscribePreviewNewWindow: share("newWindow"),
    subscribePreviewDownload: share("download"),
  };
});

vi.mock("../lib/openExternal", () => ({ openExternal }));

import { usePreviewBridge } from "./usePreviewBridge";
import { TOAST_EVENT } from "../components/common/ToastProvider";
import { usePreviewSessionStore } from "../stores/previewSessionStore";

interface CapturedToast {
  type: string;
  message: string;
  action?: { label: string; onClick: () => void };
}

function captureToasts(): CapturedToast[] {
  const seen: CapturedToast[] = [];
  window.addEventListener(TOAST_EVENT, (event) => {
    seen.push((event as CustomEvent<CapturedToast>).detail);
  });
  return seen;
}

/** Seed the store with the session a `preview://download` event refers to. */
function liveSession(label: string, url: string): void {
  usePreviewSessionStore.getState().attach(`tab-${label}`, {
    label,
    url,
    title: null,
    phase: "loaded",
    visible: true,
  });
}

async function mounted(agentId: string | null) {
  const rendered = renderHook(() => usePreviewBridge(agentId));
  await waitFor(() => expect(bridge.download).toBeTypeOf("function"));
  return rendered;
}

beforeEach(() => {
  bridge.download = null;
  bridge.newWindow = null;
  bridge.state = null;
  bridge.unlistenCalls = 0;
  openExternal.mockClear();
  usePreviewSessionStore.getState().reset();
});

describe("usePreviewBridge: a refused download", () => {
  it("reports the refusal as a warning that opens the download URL", async () => {
    const toasts = captureToasts();
    await mounted("agent-1");

    bridge.download!({ url: "https://example.com/report.pdf", from: "preview-2" });

    expect(toasts).toHaveLength(1);
    expect(toasts[0].type).toBe("warning");
    const action = toasts[0].action;
    expect(action).toBeDefined();
    action!.onClick();
    expect(openExternal).toHaveBeenCalledWith("https://example.com/report.pdf");
  });

  it("opens the page, not the blob, when the file was generated in memory", async () => {
    liveSession("preview-3", "https://example.com/files");
    const toasts = captureToasts();
    await mounted("agent-1");

    bridge.download!({ url: "blob:https://example.com/9f1e-2b", from: "preview-3" });

    expect(toasts).toHaveLength(1);
    toasts[0].action!.onClick();
    expect(openExternal).toHaveBeenCalledWith("https://example.com/files");
  });

  it("offers no action when neither the download nor the page is openable", async () => {
    const toasts = captureToasts();
    await mounted("agent-1");

    // No session in the store (evicted between the download and the event).
    bridge.download!({ url: "blob:https://example.com/9f1e-2b", from: "preview-9" });

    expect(toasts).toHaveLength(1);
    expect(toasts[0].action).toBeUndefined();
  });

  it("keeps the warning when opening the browser fails", async () => {
    openExternal.mockImplementationOnce(() => Promise.reject(new Error("no handler")));
    const toasts = captureToasts();
    await mounted(null);

    bridge.download!({ url: "https://example.com/report.pdf", from: "preview-2" });

    // A rejected open must not escape the click handler (it would surface as an
    // unhandled rejection) and must not remove the toast the user is reading.
    expect(() => toasts[0].action!.onClick()).not.toThrow();
    expect(openExternal).toHaveBeenCalledTimes(1);
    expect(toasts).toHaveLength(1);
  });
});

describe("usePreviewBridge: teardown", () => {
  it("releases every subscription when the panel unmounts", async () => {
    const { unmount } = await mounted("agent-1");

    unmount();

    await waitFor(() => expect(bridge.unlistenCalls).toBe(3));
  });

  it("does not re-subscribe when the agent changes", async () => {
    // The subscriptions are installed once on purpose: re-creating them opens a
    // window in which a download request has no listener, and a dropped toast is
    // indistinguishable from the silent refusal this hook exists to fix.
    const { rerender } = renderHook(
      ({ agent }: { agent: string | null }) => usePreviewBridge(agent),
      { initialProps: { agent: "agent-1" as string | null } },
    );
    await waitFor(() => expect(bridge.download).toBeTypeOf("function"));
    const installed = bridge.download;
    const stateHandler = bridge.state;

    rerender({ agent: "agent-2" });
    rerender({ agent: null });

    expect(bridge.download).toBe(installed);
    expect(bridge.state).toBe(stateHandler);
    expect(bridge.unlistenCalls).toBe(0);
  });
});