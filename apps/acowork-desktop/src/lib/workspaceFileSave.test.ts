// @vitest-environment jsdom
/**
 * `workspaceFileSave.ts` — the binary write-back of a hosted app's visual
 * canvas (ADR-121).
 *
 * Pinned: the bytes ride as `contentBase64` (the Runtime's binary save
 * channel — the text `content` field would corrupt a `.docx`), the URL is
 * the plain text-save endpoint with the file's identity in the querystring,
 * a refused write throws instead of reporting a save that did not happen,
 * and the cached preview blob is dropped so the next open re-reads the disk.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./config", () => ({ getGatewayUrl: () => "http://gateway:19876/" }));

const prime = vi.fn();
vi.mock("./workspaceFileBlob", () => ({
    primeWorkspaceFileBlob: (...args: unknown[]) => prime(...args),
}));

import { saveWorkspaceFileAs, saveNameSegment, saveWorkspaceFileBytes } from "./workspaceFileSave";

const fetchMock = vi.fn();
vi.stubGlobal("fetch", fetchMock);

const file = { agentId: "agent 1", workspaceId: "ws/1", relPath: "docs/a b.docx" };

function decode(body: string): Uint8Array {
    return Uint8Array.from(atob(JSON.parse(body).contentBase64), (c) => c.charCodeAt(0));
}

beforeEach(() => {
    fetchMock.mockReset();
    prime.mockReset();
});

describe("saveWorkspaceFileBytes", () => {
    it("PUTs the bytes as base64 to the workspace file URL", async () => {
        fetchMock.mockResolvedValue({
            ok: true,
            json: async () => ({ size: 3, modified: "2026-01-01T00:00:00Z" }),
        });
        // 0x00/0xff survive the base64 round trip; a text field would mangle them.
        const bytes = Uint8Array.from([0x50, 0x4b, 0x00, 0xff, 0x10]);

        const saved = await saveWorkspaceFileBytes(file, bytes);

        expect(saved).toEqual({ size: 3, modified: "2026-01-01T00:00:00Z" });
        const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
        expect(url).toBe(
            "http://gateway:19876/api/agents/agent%201/workspaces/file?workspace_id=ws%2F1&path=docs%2Fa+b.docx",
        );
        expect(init.method).toBe("PUT");
        expect(decode(String(init.body))).toEqual(bytes);
        expect(prime).toHaveBeenCalledWith("agent 1", "ws/1", "docs/a b.docx", bytes);
    });

    it("encodes a document larger than one fromCharCode chunk", async () => {
        fetchMock.mockResolvedValue({ ok: true, json: async () => ({}) });
        const bytes = new Uint8Array(0x8000 * 2 + 7).map((_, i) => i % 256);

        await saveWorkspaceFileBytes(file, bytes);

        const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
        expect(decode(String(init.body))).toEqual(bytes);
    });

    it("throws the Runtime's message and keeps the cache when the write fails", async () => {
        fetchMock.mockResolvedValue({
            ok: false,
            status: 404,
            text: async () => "file does not exist: docs/a b.docx",
        });

        await expect(saveWorkspaceFileBytes(file, new Uint8Array(1))).rejects.toThrow(
            "file does not exist: docs/a b.docx",
        );
        expect(prime).not.toHaveBeenCalled();
    });
});
describe("saveNameSegment", () => {
    it("keeps a plain file name", () => {
        expect(saveNameSegment("report.docx", "fallback.docx")).toBe("report.docx");
    });

    it("drops the directories an app might prepend", () => {
        expect(saveNameSegment("../../etc/passwd", "fallback")).toBe("passwd");
        expect(saveNameSegment(String.raw`C:\Users\me\a.pdf`, "fallback")).toBe("a.pdf");
    });

    it("falls back when the name is empty or a traversal token", () => {
        expect(saveNameSegment("", "doc.xlsx")).toBe("doc.xlsx");
        expect(saveNameSegment("..", "doc.xlsx")).toBe("doc.xlsx");
        expect(saveNameSegment("a/", "doc.xlsx")).toBe("doc.xlsx");
    });
});

describe("saveWorkspaceFileAs", () => {
    it("overwrites the bound file when the app keeps its name", async () => {
        fetchMock.mockResolvedValue({ ok: true, json: async () => ({}) });

        const saved = await saveWorkspaceFileAs(file, "a b.docx", new Uint8Array(2));

        expect(saved.relPath).toBe("docs/a b.docx");
        const [url] = fetchMock.mock.calls[0] as [string];
        expect(url).toContain("path=docs%2Fa+b.docx");
    });

    it("writes a sibling file when the app saves as another name or format", async () => {
        fetchMock.mockResolvedValue({ ok: true, json: async () => ({}) });

        const saved = await saveWorkspaceFileAs(file, "export.pdf", new Uint8Array(2));

        expect(saved.relPath).toBe("docs/export.pdf");
        const [url] = fetchMock.mock.calls[0] as [string];
        expect(url).toContain("path=docs%2Fexport.pdf");
        expect(prime).toHaveBeenCalledWith("agent 1", "ws/1", "docs/export.pdf", new Uint8Array(2));
    });

    it("writes into the workspace root when the ref has no directory", async () => {
        fetchMock.mockResolvedValue({ ok: true, json: async () => ({}) });

        const saved = await saveWorkspaceFileAs({ ...file, relPath: "" }, "Book.xlsx", new Uint8Array(2));

        expect(saved.relPath).toBe("Book.xlsx");
        const [url] = fetchMock.mock.calls[0] as [string];
        expect(url).toContain("path=Book.xlsx");
    });

    it("refuses a save the app gave no name for", async () => {
        await expect(
            saveWorkspaceFileAs({ ...file, relPath: "" }, "  ", new Uint8Array(1)),
        ).rejects.toThrow(/did not name/);
        expect(fetchMock).not.toHaveBeenCalled();
    });
});

describe("saveWorkspaceFileAs create-on-404", () => {
    it("creates the file a Save As names, then writes the bytes", async () => {
        fetchMock
            .mockResolvedValueOnce({ ok: false, status: 404, text: async () => '{"error":"not found: file does not exist: new.docx"}' })
            .mockResolvedValueOnce({ ok: true, json: async () => ({}) })
            .mockResolvedValueOnce({ ok: true, json: async () => ({ size: 2 }) });

        const saved = await saveWorkspaceFileAs({ ...file, relPath: "" }, "new.docx", new Uint8Array(2));

        expect(saved.relPath).toBe("new.docx");
        const [createUrl, createInit] = fetchMock.mock.calls[1] as [string, RequestInit];
        expect(createInit.method).toBe("POST");
        expect(createUrl).toBe("http://gateway:19876/api/agents/agent%201/workspaces/file?workspace_id=ws%2F1");
        expect(JSON.parse(String(createInit.body))).toEqual({ path: "new.docx", overwrite: true });
        expect((fetchMock.mock.calls[2] as [string, RequestInit])[1].method).toBe("PUT");
    });

    it("does not create on any other refusal", async () => {
        fetchMock.mockResolvedValue({ ok: false, status: 400, text: async () => "path escapes the workspace" });

        await expect(saveWorkspaceFileAs(file, "a b.docx", new Uint8Array(1))).rejects.toThrow(
            /path escapes the workspace/,
        );
        expect(fetchMock).toHaveBeenCalledTimes(1);
    });
});

describe("saveHostedAppFile", () => {
    beforeEach(() => {
        vi.resetModules();
    });

    function stubStores(): { settle: (t: unknown) => void; opened: () => Array<{ agentId: string; defaultWorkspaceId: string; name: string }> } {
        const opened: Array<{ agentId: string; defaultWorkspaceId: string; name: string }> = [];
        vi.doMock("../stores/saveAsStore", () => ({
            openSaveAs: (req: { agentId: string; defaultWorkspaceId: string; name: string }) => {
                opened.push(req);
                return Promise.resolve((globalThis as { __target?: unknown }).__target ?? null);
            },
        }));
        vi.doMock("../stores/agentStore", () => ({
            useAgentStore: { getState: () => ({ selectedAgentId: "agent 1" }) },
        }));
        vi.doMock("../stores/chatStore", () => ({
            useChatStore: { getState: () => ({ getActiveSessionId: () => "s1" }) },
        }));
        vi.doMock("../stores/workspaceStore", () => ({
            useWorkspaceStore: { getState: () => ({ getSessionWorkspaceId: () => "ws-session" }) },
        }));
        vi.doMock("../stores/fileTree/fileTreeStore", () => ({ refreshTreeNode: vi.fn() }));
        return { settle: (t) => ((globalThis as { __target?: unknown }).__target = t), opened: () => opened };
    }

    it("overwrites the bound file on a plain Save — no dialog", async () => {
        const { opened } = stubStores();
        fetchMock.mockResolvedValue({ ok: true, json: async () => ({}) });
        const mod = await import("./workspaceFileSave");

        await mod.saveHostedAppFile({ bound: file, saveAs: false, name: "a b.docx", bytes: new Uint8Array(2) });

        expect(opened()).toHaveLength(0);
        expect((fetchMock.mock.calls[0] as [string, RequestInit])[1].method).toBe("PUT");
    });

    it("asks where a Save As goes and writes to the picked path", async () => {
        const { opened, settle } = stubStores();
        settle({ agentId: "agent 1", workspaceId: "ws/1", relPath: "reports/Q3.docx" });
        fetchMock.mockResolvedValue({ ok: true, json: async () => ({}) });
        const mod = await import("./workspaceFileSave");

        await mod.saveHostedAppFile({ bound: file, saveAs: true, name: "Q3.docx", bytes: new Uint8Array(2) });

        expect(opened()).toEqual([{ agentId: "agent 1", defaultWorkspaceId: "ws/1", name: "Q3.docx" }]);
        const [url] = fetchMock.mock.calls[0] as [string];
        expect(url).toContain("path=reports%2FQ3.docx");
    });

    it("writes nothing when the dialog is cancelled", async () => {
        const { settle, opened } = stubStores();
        settle(null);
        const mod = await import("./workspaceFileSave");

        await expect(
            mod.saveHostedAppFile({ bound: null, saveAs: true, name: "x.docx", bytes: new Uint8Array(1) }),
        ).rejects.toBeInstanceOf(mod.SaveCancelled);
        expect(opened()).toHaveLength(1);
        expect(fetchMock).not.toHaveBeenCalled();
    });
});
