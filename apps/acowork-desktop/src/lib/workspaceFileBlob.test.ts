// @vitest-environment jsdom
/**
 * `workspaceFileBlob.ts` — the preview bytes source + LRU cache.
 *
 * Pinned behaviours, all of them the reason the module exists:
 *   - the Gateway raw route is spelled once, with per-segment encoding;
 *   - a bare URL never escapes (only Blob / blob: URL come out);
 *   - cache hit / force / invalidate semantics and concurrent dedupe;
 *   - eviction revokes the blob: URL (otherwise bytes leak).
 *
 * `URL.createObjectURL` is not implemented by jsdom, so both it and
 * `revokeObjectURL` are stubbed and asserted directly.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./config", () => ({ getGatewayUrl: () => "http://gateway:19876/" }));
// The Tauri bridge must not load in jsdom — the transfer helpers are
// asserted by their arguments only (same stub pattern as agentFile.test).
const saveUrlToDiskMock = vi.fn(async (..._args: unknown[]) => true);
const invokeMock = vi.fn(async () => undefined);
vi.mock("./attachmentTransfer", () => ({
    saveUrlToDisk: (...args: unknown[]) => saveUrlToDiskMock(...(args as [])),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...(args as [])) }));

import {
    __resetWorkspaceFileBlobCache,
    fetchWorkspaceFileBlob,
    fetchWorkspaceFileBlobUrl,
    invalidateWorkspaceFileBlob,
    openWorkspaceFileExternally,
    saveWorkspaceFileToDisk,
    workspaceFileUrlForRust,
} from "./workspaceFileBlob";

let objectUrlSeq = 0;
const createObjectURLMock = vi.fn(() => `blob:mock/${++objectUrlSeq}`);
const revokeObjectURLMock = vi.fn();
const fetchMock = vi.fn();

function jsonResponse(body: string, contentType = "application/pdf") {
    const blob = new Blob([body], { type: contentType });
    return {
        ok: true,
        status: 200,
        blob: async () => blob,
        headers: new Headers({ "content-type": contentType }),
    };
}

beforeEach(() => {
    objectUrlSeq = 0;
    createObjectURLMock.mockClear();
    revokeObjectURLMock.mockClear();
    fetchMock.mockReset();
    saveUrlToDiskMock.mockClear();
    invokeMock.mockClear();
    fetchMock.mockImplementation(async () => jsonResponse("bytes"));
    vi.stubGlobal("fetch", fetchMock);
    Object.defineProperty(URL, "createObjectURL", { value: createObjectURLMock, configurable: true });
    Object.defineProperty(URL, "revokeObjectURL", { value: revokeObjectURLMock, configurable: true });
    __resetWorkspaceFileBlobCache();
});

describe("URL construction", () => {
    it("encodes each path segment while keeping / separators", async () => {
        await fetchWorkspaceFileBlob("agent/1", "ws 1", "docs/报告 a.pdf");
        expect(fetchMock.mock.calls[0][0]).toBe(
            "http://gateway:19876/workspace-files/agent%2F1/ws%201/docs/%E6%8A%A5%E5%91%8A%20a.pdf",
        );
    });

    it("normalizes backslashes in workspace-relative paths", async () => {
        await fetchWorkspaceFileBlob("a", "w", "sub\\x.xlsx");
        expect(fetchMock.mock.calls[0][0]).toBe(
            "http://gateway:19876/workspace-files/a/w/sub/x.xlsx",
        );
    });

    it("throws on a non-2xx response so callers can render a retry", async () => {
        fetchMock.mockResolvedValueOnce({ ok: false, status: 413 } as Response);
        await expect(fetchWorkspaceFileBlob("a", "w", "big.pdf")).rejects.toThrow(/413/);
    });

    it("hands Rust's converter the exact route the blob cache fetches", () => {
        // `docConvert.ts` addresses the same file from the other side of
        // the bridge; one builder, so the two can never drift apart.
        expect(workspaceFileUrlForRust("agent/1", "ws 1", "docs/报告 a.pdf")).toBe(
            "http://gateway:19876/workspace-files/agent%2F1/ws%201/docs/%E6%8A%A5%E5%91%8A%20a.pdf",
        );
    });

    it("stops only the aborted caller's wait — the shared fetch runs on", async () => {
        // The StrictMode-remount regression: round one aborts, round two
        // joins the SAME in-flight. If the caller signal reached the
        // underlying fetch, round two would inherit its AbortError and
        // every viewer would show "signal is aborted without reason".
        let release: () => void;
        const gate = new Promise<void>((res) => { release = res; });
        fetchMock.mockImplementation(async () => {
            await gate;
            return jsonResponse("bytes");
        });
        const ctrl = new AbortController();
        const p1 = fetchWorkspaceFileBlob("a", "w", "abort.pdf", { signal: ctrl.signal });
        const p2 = fetchWorkspaceFileBlob("a", "w", "abort.pdf");
        ctrl.abort();
        await expect(p1).rejects.toThrow(/aborted/i);
        release!();
        const r2 = await p2;
        expect(r2.blob).toBeInstanceOf(Blob);
        expect(fetchMock).toHaveBeenCalledTimes(1);
        // No signal was ever handed to the network layer itself.
        expect(fetchMock.mock.calls[0][1]).toBeUndefined();
    });

    it("a caller that aborts mid-flight still leaves the bytes cached", async () => {
        let release: () => void;
        const gate = new Promise<void>((res) => { release = res; });
        fetchMock.mockImplementation(async () => {
            await gate;
            return jsonResponse("bytes");
        });
        const ctrl = new AbortController();
        const p1 = fetchWorkspaceFileBlobUrl("a", "w", "late.pdf", { signal: ctrl.signal });
        ctrl.abort();
        await expect(p1).rejects.toThrow(/aborted/i);
        release!();
        // Let the abandoned download settle, then read again: cache hit.
        await new Promise((res) => setTimeout(res, 0));
        const hit = await fetchWorkspaceFileBlobUrl("a", "w", "late.pdf");
        expect(hit.blobUrl).toMatch(/^blob:mock\//);
        expect(fetchMock).toHaveBeenCalledTimes(1);
    });
});

describe("cache", () => {
    it("serves a second read from cache without re-fetching, same blob URL", async () => {
        const a = await fetchWorkspaceFileBlobUrl("a", "w", "dup.pdf");
        const b = await fetchWorkspaceFileBlobUrl("a", "w", "dup.pdf");
        expect(fetchMock).toHaveBeenCalledTimes(1);
        expect(b.blobUrl).toBe(a.blobUrl);
    });

    it("deduplicates concurrent reads of the same key into one fetch", async () => {
        let release: () => void;
        const gate = new Promise<void>((res) => { release = res; });
        fetchMock.mockImplementation(async () => {
            await gate;
            return jsonResponse("bytes");
        });
        const p1 = fetchWorkspaceFileBlob("a", "w", "race.pdf");
        const p2 = fetchWorkspaceFileBlob("a", "w", "race.pdf");
        release!();
        const [r1, r2] = await Promise.all([p1, p2]);
        expect(fetchMock).toHaveBeenCalledTimes(1);
        expect(r2.blob).toBe(r1.blob);
    });

    it("force re-fetches and revokes the superseded blob URL", async () => {
        const first = await fetchWorkspaceFileBlobUrl("a", "w", "f.pdf");
        const second = await fetchWorkspaceFileBlobUrl("a", "w", "f.pdf", { force: true });
        expect(fetchMock).toHaveBeenCalledTimes(2);
        expect(second.blobUrl).not.toBe(first.blobUrl);
        expect(revokeObjectURLMock).toHaveBeenCalledWith(first.blobUrl);
    });

    it("invalidate drops the entry and revokes its blob URL", async () => {
        const first = await fetchWorkspaceFileBlobUrl("a", "w", "inv.pdf");
        invalidateWorkspaceFileBlob("a", "w", "inv.pdf");
        expect(revokeObjectURLMock).toHaveBeenCalledWith(first.blobUrl);
        await fetchWorkspaceFileBlobUrl("a", "w", "inv.pdf");
        expect(fetchMock).toHaveBeenCalledTimes(2);
    });

    it("evicts the least-recently-used entry once the byte budget overflows", async () => {
        __resetWorkspaceFileBlobCache(10); // 10-byte budget
        // "bytes" = 5 bytes each: two fit, the third evicts the oldest.
        const p1 = await fetchWorkspaceFileBlobUrl("a", "w", "one.pdf");
        await fetchWorkspaceFileBlobUrl("a", "w", "two.pdf");
        await fetchWorkspaceFileBlobUrl("a", "w", "three.pdf");
        expect(revokeObjectURLMock).toHaveBeenCalledWith(p1.blobUrl);
        // The evicted file re-fetches on the next read.
        const before = fetchMock.mock.calls.length;
        await fetchWorkspaceFileBlobUrl("a", "w", "one.pdf");
        expect(fetchMock.mock.calls.length).toBe(before + 1);
    });

    it("keeps an entry alive when it is read again before eviction", async () => {
        __resetWorkspaceFileBlobCache(10);
        const p1 = await fetchWorkspaceFileBlobUrl("a", "w", "hot.pdf");
        await fetchWorkspaceFileBlobUrl("a", "w", "other.pdf");
        // Re-touch hot.pdf so it becomes the most-recently-used entry...
        const again = await fetchWorkspaceFileBlobUrl("a", "w", "hot.pdf");
        expect(again.blobUrl).toBe(p1.blobUrl);
        // ...then push it out with two more reads instead.
        await fetchWorkspaceFileBlobUrl("a", "w", "x.pdf");
        await fetchWorkspaceFileBlobUrl("a", "w", "y.pdf");
        expect(revokeObjectURLMock).toHaveBeenCalledWith(p1.blobUrl);
    });
});

describe("mime", () => {
    it("strips charset parameters from the response Content-Type", async () => {
        fetchMock.mockResolvedValueOnce(jsonResponse("x", "text/csv; charset=utf-8"));
        const { mime } = await fetchWorkspaceFileBlob("a", "w", "m1.csv");
        expect(mime).toBe("text/csv");
    });

    it("falls back to octet-stream when nothing declares a type", async () => {
        const blob = new Blob(["x"]);
        Object.defineProperty(blob, "type", { value: "" });
        fetchMock.mockResolvedValueOnce({
            ok: true,
            status: 200,
            blob: async () => blob,
            headers: new Headers(),
        });
        const { mime } = await fetchWorkspaceFileBlob("a", "w", "m2.bin");
        expect(mime).toBe("application/octet-stream");
    });
});

describe("transfer helpers", () => {
    it("saves through the shared URL-carrying transfer, same route as the fetch", async () => {
        await saveWorkspaceFileToDisk("a/1", "w 2", "d x.pdf", "x.pdf");
        expect(saveUrlToDiskMock).toHaveBeenCalledWith(
            "http://gateway:19876/workspace-files/a%2F1/w%202/d%20x.pdf",
            "x.pdf",
            undefined,
        );
    });

    it("opens externally via the Rust download-and-launch command", async () => {
        await openWorkspaceFileExternally("a", "w", "big.doc", "big.doc");
        expect(invokeMock).toHaveBeenCalledWith("open_file_externally", {
            url: "http://gateway:19876/workspace-files/a/w/big.doc",
            filename: "big.doc",
        });
    });
});
