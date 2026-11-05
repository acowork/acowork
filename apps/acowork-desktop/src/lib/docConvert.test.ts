/**
 * docConvert — the converter client's caching and error-shape contract.
 *
 * What is pinned here is exactly what the preview degradation UX depends
 * on, none of it the conversion itself (that is Rust's):
 *   - the engine probe runs ONCE per process and is shared by concurrent
 *     tabs — the shared promise is never bound to a caller signal (the
 *     workspaceFileBlob AbortError lesson);
 *   - a probe that cannot run answers `none` instead of rejecting, so no
 *     document tab can end up in an error state over a missing Office;
 *   - "no engine" surfaces as `DocConvertUnavailableError` — the one error
 *     type the viewer turns into a banner rather than a failure;
 *   - results are cached per file VERSION, failures are not cached, and
 *     the LRU cap keeps six open tabs from holding six PDFs in memory.
 *
 * `workspaceFileBlob` is stubbed: the URL it builds is its own test's
 * problem, and the converter's job starts at "hand Rust a fetch target".
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

const h = vi.hoisted(() => ({
  invoke: vi.fn(),
  urlFor: vi.fn(
    (agentId: string, workspaceId: string, relPath: string) =>
      `http://gw.test/workspace-files/${agentId}/${workspaceId}/${relPath}`,
  ),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: h.invoke }));
vi.mock("./workspaceFileBlob", () => ({ workspaceFileUrlForRust: h.urlFor }));
vi.mock("./logger", () => ({
  log: { trace: () => {}, debug: () => {}, info: () => {}, warn: vi.fn(), error: () => {} },
}));

import {
    convertWorkspaceFileToPdf,
    detectDocConvertEngine,
    DocConvertUnavailableError,
    __resetDocConvertCache,
} from "./docConvert";

const ENGINE: unknown = { engine: "libreoffice", path: "C:/LO/soffice.exe" };

function pdfBytes(seed: number): ArrayBuffer {
    return new Uint8Array([0x25, 0x50, 0x44, 0x46, seed]).buffer; // "%PDF" + tag
}

beforeEach(() => {
    h.invoke.mockReset();
    h.urlFor.mockClear();
    __resetDocConvertCache();
});

describe("detectDocConvertEngine", () => {
    it("probes once and serves every later caller from the cache", async () => {
        h.invoke.mockResolvedValue(ENGINE);
        const first = await detectDocConvertEngine();
        const second = await detectDocConvertEngine();
        expect(first).toEqual({ engine: "libreoffice", path: "C:/LO/soffice.exe" });
        expect(second).toEqual(first);
        expect(h.invoke).toHaveBeenCalledTimes(1);
        expect(h.invoke.mock.calls[0][0]).toBe("detect_doc_convert_engine");
    });

    it("shares one in-flight probe between concurrent tabs", async () => {
        let release: (v: unknown) => void = () => {};
        h.invoke.mockImplementation(() => new Promise((resolve) => { release = resolve; }));
        const a = detectDocConvertEngine();
        const b = detectDocConvertEngine();
        release(ENGINE);
        expect(await a).toBe(await b);
        expect(h.invoke).toHaveBeenCalledTimes(1);
    });

    it("answers none for an unrecognised engine instead of guessing", async () => {
        h.invoke.mockResolvedValue({ engine: "openoffice", path: null });
        expect(await detectDocConvertEngine()).toEqual({ engine: "none", path: null });
    });

    it("treats a failed probe as none, but re-probes next time", async () => {
        h.invoke.mockRejectedValueOnce("ipc unavailable");
        expect(await detectDocConvertEngine()).toEqual({ engine: "none", path: null });
        h.invoke.mockResolvedValue(ENGINE);
        expect(await detectDocConvertEngine()).toEqual({ engine: "libreoffice", path: "C:/LO/soffice.exe" });
        expect(h.invoke).toHaveBeenCalledTimes(2);
    });
});

describe("convertWorkspaceFileToPdf", () => {
    it("hands Rust the Gateway URL, the name and the version, and returns bytes", async () => {
        h.invoke.mockImplementation((cmd: string) =>
            Promise.resolve(cmd === "detect_doc_convert_engine" ? ENGINE : pdfBytes(1)),
        );
        const bytes = await convertWorkspaceFileToPdf("a1", "ws1", "docs/报告 v1.docx", "报告 v1.docx", {
            size: 1234,
            mtimeMs: 999,
            seq: 2,
        });
        expect(bytes).toBeInstanceOf(Uint8Array);
        expect(Array.from(bytes)).toEqual([0x25, 0x50, 0x44, 0x46, 1]);

        const call = h.invoke.mock.calls.find((c) => c[0] === "convert_workspace_file_to_pdf");
        expect(call?.[1]).toEqual({
            // The stubbed builder's output, verbatim — encoding is pinned
            // in workspaceFileBlob.test.ts, not here.
            url: "http://gw.test/workspace-files/a1/ws1/docs/报告 v1.docx",
            filename: "报告 v1.docx",
            size: 1234,
            etag: "999#2",
        });
        expect(h.urlFor).toHaveBeenCalledWith("a1", "ws1", "docs/报告 v1.docx");
    });

    it("reuses one conversion per file version and re-runs on a new version", async () => {
        h.invoke.mockImplementation((cmd: string) =>
            Promise.resolve(cmd === "detect_doc_convert_engine" ? ENGINE : pdfBytes(1)),
        );
        const v1 = { size: 100, mtimeMs: 10 };
        const first = await convertWorkspaceFileToPdf("a", "w", "d.docx", "d.docx", v1);
        const again = await convertWorkspaceFileToPdf("a", "w", "d.docx", "d.docx", v1);
        expect(again).toBe(first);
        expect(h.invoke.mock.calls.filter((c) => c[0] === "convert_workspace_file_to_pdf")).toHaveLength(1);

        h.invoke.mockImplementation((cmd: string) =>
            Promise.resolve(cmd === "detect_doc_convert_engine" ? ENGINE : pdfBytes(2)),
        );
        const edited = await convertWorkspaceFileToPdf("a", "w", "d.docx", "d.docx", { ...v1, mtimeMs: 11 });
        expect(Array.from(edited)).toEqual([0x25, 0x50, 0x44, 0x46, 2]);
        expect(h.invoke.mock.calls.filter((c) => c[0] === "convert_workspace_file_to_pdf")).toHaveLength(2);
    });

    it("dedupes concurrent requests for the same file into one conversion", async () => {
        let release: (v: unknown) => void = () => {};
        h.invoke.mockImplementation((cmd: string) =>
            cmd === "detect_doc_convert_engine"
                ? Promise.resolve(ENGINE)
                : new Promise((resolve) => { release = resolve; }),
        );
        const conversions = () =>
            h.invoke.mock.calls.filter((c) => c[0] === "convert_workspace_file_to_pdf").length;
        const a = convertWorkspaceFileToPdf("a", "w", "d.docx", "d.docx", { size: 1 });
        const b = convertWorkspaceFileToPdf("a", "w", "d.docx", "d.docx", { size: 1 });
        // The second caller joins the shared promise; the conversion itself
        // only reaches IPC one microtask later (after the cached engine
        // answer), so wait for it before releasing it.
        await vi.waitFor(() => expect(conversions()).toBe(1));
        release(pdfBytes(7));
        expect(await a).toBe(await b);
        expect(conversions()).toBe(1);
    });

    it("does not convert at all when the machine has no engine", async () => {
        h.invoke.mockResolvedValue({ engine: "none", path: null });
        await expect(convertWorkspaceFileToPdf("a", "w", "d.docx", "d.docx")).rejects.toBeInstanceOf(
            DocConvertUnavailableError,
        );
        expect(h.invoke.mock.calls.filter((c) => c[0] === "convert_workspace_file_to_pdf")).toHaveLength(0);
    });

    it("maps Rust's engine-unavailable error to the same unavailable type", async () => {
        h.invoke.mockImplementation((cmd: string) =>
            cmd === "detect_doc_convert_engine"
                ? Promise.resolve({ engine: "ms-office-com", path: null })
                : Promise.reject("engine-unavailable"),
        );
        await expect(convertWorkspaceFileToPdf("a", "w", "d.pptx", "d.pptx")).rejects.toBeInstanceOf(
            DocConvertUnavailableError,
        );
    });

    it("keeps a real failure a plain error and does not cache it", async () => {
        h.invoke.mockImplementation((cmd: string) =>
            cmd === "detect_doc_convert_engine"
                ? Promise.resolve(ENGINE)
                : Promise.reject("soffice exited with code 1"),
        );
        await expect(convertWorkspaceFileToPdf("a", "w", "d.docx", "d.docx", { size: 5 })).rejects.toThrow(
            /soffice exited/,
        );
        h.invoke.mockImplementation((cmd: string) =>
            cmd === "detect_doc_convert_engine"
                ? Promise.resolve(ENGINE)
                : Promise.resolve(pdfBytes(3)),
        );
        const ok = await convertWorkspaceFileToPdf("a", "w", "d.docx", "d.docx", { size: 5 });
        expect(Array.from(ok)).toEqual([0x25, 0x50, 0x44, 0x46, 3]);
    });

    it("tolerates a Uint8Array response (older / channel-shaped IPC)", async () => {
        h.invoke.mockImplementation((cmd: string) =>
            cmd === "detect_doc_convert_engine"
                ? Promise.resolve(ENGINE)
                : Promise.resolve(new Uint8Array([0x25, 0x50, 0x44, 0x46, 9])),
        );
        const bytes = await convertWorkspaceFileToPdf("a", "w", "u.docx", "u.docx", { size: 1 });
        expect(bytes).toBeInstanceOf(Uint8Array);
        expect(Array.from(bytes)).toEqual([0x25, 0x50, 0x44, 0x46, 9]);
    });

    it("evicts the least recently used conversion past the cap", async () => {
        h.invoke.mockImplementation((cmd: string) =>
            cmd === "detect_doc_convert_engine"
                ? Promise.resolve(ENGINE)
                : Promise.resolve(pdfBytes(1)),
        );
        const conversions = () =>
            h.invoke.mock.calls.filter((c) => c[0] === "convert_workspace_file_to_pdf").length;
        for (let i = 0; i < 6; i++) {
            await convertWorkspaceFileToPdf("a", "w", `f${i}.docx`, `f${i}.docx`, { size: i });
        }
        expect(conversions()).toBe(6);
        // The first file left the cache, so it converts again; the last
        // five are still held.
        await convertWorkspaceFileToPdf("a", "w", "f0.docx", "f0.docx", { size: 0 });
        expect(conversions()).toBe(7);
        await convertWorkspaceFileToPdf("a", "w", "f5.docx", "f5.docx", { size: 5 });
        expect(conversions()).toBe(7);
    });
});
