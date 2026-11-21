// @vitest-environment jsdom
/**
 * `usePreviewBridge` — the two "the page asked for something the app cannot do"
 * paths, which are the only ones that produce *visible* feedback.
 *
 * The download case is a regression guard: Rust refuses the download (no
 * download shelf in the app) and the refusal used to end there, so the link
 * looked broken. The contract is that the refusal reaches the user as a toast
 * carrying an action that hands the URL to the system browser.
 */
import { renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const bridge = vi.hoisted(() => ({
  state: null as null | ((s: unknown) => void),
  newWindow: null as null | ((r: unknown) => void),
  download: null as null | ((r: { url: string; from: string }) => void),
  unlistenCalls: 0,
}));

const openExternal = vi.hoisted(() => vi.fn(() => Promise.resolve()));

vi.mock("../lib/previewHost", () => {
  const share = (slot: "state" | "newWindow" | "download") => (handler: never) => {
    (bridge as Record<string, unknown>)[slot] = handler;
    return Promise.resolve(() => {
      bridge.unlistenCalls += 1;
    });
  };
  return {
    subscribePreviewState: share("state"),
    subscribePreviewNewWindow: share("newWindow"),
    subscribePreviewDownload: share("download"),
  };
});

vi.mock("../lib/openExternal", () => ({ openExternal }));

import { usePreviewBridge } from "./usePreviewBridge";
import { TOAST_EVENT } from "../components/common/ToastProvider";

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
});

afterEach(() => vi.clearAllMocks());

describe("usePreviewBridge: a refused download", () => {
  it("is reported as a warning that offers the system browser", async () => {
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

  it("does not stall the bridge when opening the browser fails", async () => {
    openExternal.mockImplementationOnce(() => Promise.reject(new Error("no browser")));
    const toasts = captureToasts();
    await mounted(null);

    bridge.download!({ url: "https://example.com/report.pdf", from: "preview-2" });

    expect(() => toasts[0].action!.onClick()).not.toThrow();
  });
});

describe("usePreviewBridge: teardown", () => {
  it("releases every subscription when the panel unmounts", async () => {
    const { unmount } = await mounted("agent-1");

    unmount();

    await waitFor(() => expect(bridge.unlistenCalls).toBe(3));
  });
});