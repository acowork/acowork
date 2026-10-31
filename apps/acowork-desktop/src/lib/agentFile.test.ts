// @vitest-environment jsdom
/**
 * `agentFile.ts` — the execution surface's file address space.
 *
 * Two things are worth pinning here, because both are the reason the module
 * exists: the endpoint is spelled once (so a row and a viewer cannot build it
 * differently), and a failed read throws instead of handing back a `null` that
 * every caller would have to interpret again.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const saveUrlToDiskMock = vi.fn(async (..._args: unknown[]) => true);
vi.mock("./attachmentTransfer", () => ({
  saveUrlToDisk: (...args: unknown[]) => saveUrlToDiskMock(...(args as [])),
}));
vi.mock("./config", () => ({ getGatewayUrl: () => "http://gateway:19876" }));

import { fetchAgentFileBlob, saveAgentFile } from "./agentFile";

beforeEach(() => {
  saveUrlToDiskMock.mockClear();
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, blob: async () => new Blob(["img"]) })));
});

describe("fetchAgentFileBlob", () => {
  it("reads the agent's file route, with both id segments escaped", async () => {
    await fetchAgentFileBlob("agent/1", "doc 2");
    const url = String(vi.mocked(fetch).mock.calls[0][0]);
    expect(url).toBe("http://gateway:19876/api/agents/agent%2F1/files/doc%202");
  });

  it("passes the caller's abort signal through", async () => {
    const ctrl = new AbortController();
    await fetchAgentFileBlob("a", "d", { signal: ctrl.signal });
    expect(vi.mocked(fetch).mock.calls[0][1]).toEqual({ signal: ctrl.signal });
  });

  it("throws on a non-2xx response", async () => {
    vi.mocked(fetch).mockResolvedValueOnce({ ok: false, status: 404 } as Response);
    await expect(fetchAgentFileBlob("a", "d")).rejects.toThrow(/404/);
  });
});

describe("saveAgentFile", () => {
  it("saves through the shared transfer, with the URL it built itself", async () => {
    await saveAgentFile({ agentId: "a", documentId: "d" }, "shot.png");
    expect(saveUrlToDiskMock).toHaveBeenCalledWith(
      "http://gateway:19876/api/agents/a/files/d",
      "shot.png",
      undefined,
    );
  });
});
