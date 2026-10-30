/**
 * `downloadChatAttachment` contract check.
 *
 * Regression guard for the "download button spins forever" bug: the
 * attachment bytes must never be handed to `invoke`. Tauri only sends
 * raw bytes when the *whole* payload is an `ArrayBuffer`; a named
 * `Uint8Array` argument is serialized into a JSON array with one decimal
 * number per byte (`process-ipc-message-fn.js`), which froze the WebView
 * main thread for seconds on a multi-MB attachment and left the `invoke`
 * promise unsettled. Rust (`download_attachment`) performs the transfer
 * now, so the only things crossing the bridge are the attachment URL and
 * the path the user picked.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  // Stand-in for Tauri's `Channel`: records the handler so the test can
  // replay what Rust would push.
  Channel: class {
    onmessage: ((message: unknown) => void) | null = null;
  },
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ save: vi.fn() }));
vi.mock("./config", () => ({ getGatewayUrl: () => "http://127.0.0.1:19876" }));

import { invoke } from "@tauri-apps/api/core";
import type { Channel } from "@tauri-apps/api/core";
import { save as showSaveDialog } from "@tauri-apps/plugin-dialog";
import { downloadChatAttachment, downloadChatAttachmentBySession } from "./user-chat-api";
import type { ChatAttachment } from "./types";

const ATTACHMENT: ChatAttachment = {
  id: "att-1",
  filename: "报告 v2.pdf",
  mime: "application/pdf",
  size: 4096,
};

beforeEach(() => {
  vi.mocked(invoke).mockReset().mockResolvedValue(undefined);
  vi.mocked(showSaveDialog).mockReset();
});

describe("downloadChatAttachment", () => {
  it("hands Rust the attachment URL and the chosen path — never the bytes", async () => {
    vi.mocked(showSaveDialog).mockResolvedValue("D:\\Downloads\\报告 v2.pdf");

    await expect(downloadChatAttachment("u-1", "chat-1", ATTACHMENT)).resolves.toBe(true);

    expect(invoke).toHaveBeenCalledTimes(1);
    const [cmd, args] = vi.mocked(invoke).mock.calls[0] as [string, Record<string, unknown>];
    expect(cmd).toBe("download_attachment");
    expect(args.url).toBe("http://127.0.0.1:19876/api/users/u-1/chats/chat-1/files/att-1");
    expect(args.path).toBe("D:\\Downloads\\报告 v2.pdf");
    // The regression: any argument holding the file bytes (a
    // `Uint8Array`, an array, anything array-like) goes over IPC one
    // decimal number per byte and hangs the webview.
    expect(Object.keys(args).sort()).toEqual(["onProgress", "path", "url"]);
    for (const value of Object.values(args)) {
      expect(Array.isArray(value) || ArrayBuffer.isView(value)).toBe(false);
    }
  });

  it("reports byte counts pushed from Rust to the caller", async () => {
    vi.mocked(showSaveDialog).mockResolvedValue("C:\\tmp\\out.pdf");
    const seen: number[] = [];

    await downloadChatAttachment("u-1", "chat-1", ATTACHMENT, (received) => seen.push(received));

    // Rust writes into the channel it was handed; the caller's callback
    // must be wired to it, otherwise the bar never moves.
    const [, args] = vi.mocked(invoke).mock.calls[0] as [string, { onProgress: Channel<number> }];
    args.onProgress.onmessage?.(1024);
    args.onProgress.onmessage?.(4096);
    expect(seen).toEqual([1024, 4096]);
  });

  it("does nothing when the user cancels the save dialog", async () => {
    vi.mocked(showSaveDialog).mockResolvedValue(null);

    await expect(downloadChatAttachment("u-1", "chat-1", ATTACHMENT)).resolves.toBe(false);
    expect(invoke).not.toHaveBeenCalled();
  });

  it("seeds the dialog with the filename and its extension filter", async () => {
    vi.mocked(showSaveDialog).mockResolvedValue("C:\\tmp\\out.pdf");

    await downloadChatAttachment("u-1", "chat-1", ATTACHMENT);

    expect(showSaveDialog).toHaveBeenCalledWith({
      defaultPath: "报告 v2.pdf",
      filters: [{ name: "报告 v2.pdf", extensions: ["pdf"] }],
    });
  });
});

/**
 * The session-scoped twin. The group surface never holds the owning
 * `userId`, so it addresses the same blob through
 * `/api/chat-sessions/{sid}/files/{aid}` (participation-checked server-side).
 * One transfer implementation, two addresses — the split is only about which
 * identity the caller happens to have.
 */
describe("downloadChatAttachmentBySession", () => {
  it("addresses the session route and still hands Rust only url + path", async () => {
    vi.mocked(showSaveDialog).mockResolvedValue("D:\Downloads\报告 v2.pdf");

    await expect(
      downloadChatAttachmentBySession("chat-1", ATTACHMENT),
    ).resolves.toBe(true);

    expect(invoke).toHaveBeenCalledTimes(1);
    const [cmd, args] = vi.mocked(invoke).mock.calls[0] as [string, Record<string, unknown>];
    expect(cmd).toBe("download_attachment");
    expect(args.url).toBe("http://127.0.0.1:19876/api/chat-sessions/chat-1/files/att-1");
    expect(Object.keys(args).sort()).toEqual(["onProgress", "path", "url"]);
  });

  it("does nothing when the user cancels", async () => {
    vi.mocked(showSaveDialog).mockResolvedValue(null);
    await expect(
      downloadChatAttachmentBySession("chat-1", ATTACHMENT),
    ).resolves.toBe(false);
    expect(invoke).not.toHaveBeenCalled();
  });
});
