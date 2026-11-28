// @vitest-environment jsdom
/**
 * `docFileBlob.ts` — 文档条目与工作区字节缓存之间的桥。
 *
 * 钉住的行为，每一条一旦漂移都会让**导入条目**预览失败（pdf / docx / pptx /
 * 图片同罪），而不是某个格式单独坏掉：
 *   - 坐标映射：`docId → workspaceId`，`文件名 → relPath`（文件名只来自
 *     `docEntry.ts::docEntryFileName()`，本模块不重新拼 `name + "." + ext`）；
 *   - 地址来源：doc 字节地址只有一个构造者（`doc-api` 的 `getDocFileUrl`），
 *     `/api/doc` 不出现在这个模块里；
 *   - 回写刷新：只有 `fileName` 时也能反推 `name`/`ext` 并刷新缓存。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./config", () => ({ getGatewayUrl: () => "http://gateway:19876/" }));
vi.mock("./attachmentTransfer", () => ({ saveUrlToDisk: vi.fn(async () => true) }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => undefined) }));

import {
    DOC_FILE_AGENT_ID,
    docEntryFileUrl,
    docFileRef,
    primeDocFileBlob,
    primeDocFileBlobFromFileName,
} from "./docFileBlob";
import {
    __resetWorkspaceFileBlobCache,
    fetchWorkspaceFileBlob,
    fetchWorkspaceFileBlobUrl,
} from "./workspaceFileBlob";

const fetchMock = vi.fn();
const createObjectURLMock = vi.fn((b: Blob) => `blob:mock/${b.size}`);

beforeEach(() => {
    __resetWorkspaceFileBlobCache();
    fetchMock.mockReset();
    fetchMock.mockImplementation(async () => ({
        ok: true,
        status: 200,
        blob: async () => new Blob(["bytes"], { type: "application/pdf" }),
        headers: new Headers({ "content-type": "application/pdf" }),
    }));
    vi.stubGlobal("fetch", fetchMock);
    Object.defineProperty(URL, "createObjectURL", { value: createObjectURLMock, configurable: true });
    Object.defineProperty(URL, "revokeObjectURL", { value: vi.fn(), configurable: true });
});

describe("docFileRef — 文档条目映射成字节缓存的坐标", () => {
    it("用哨兵 agentId 承载 docId 与真实文件名", () => {
        const ref = docFileRef({ docId: "doc 1", path: "研发/季度报告.pdf", name: "季度报告", ext: "pdf" });
        expect(ref.agentId).toBe(DOC_FILE_AGENT_ID);
        expect(ref.workspaceId).toBe("doc 1");
        expect(ref.relPath).toBe("季度报告.pdf");
        expect(ref.fileName).toBe("季度报告.pdf");
    });

    it("没有 ext 的条目是 Markdown，路径末段就是文件名", () => {
        const ref = docFileRef({ docId: "d2", path: "笔记/笔记.md", name: "笔记", ext: null });
        expect(ref.fileName).toBe("笔记.md");
    });

    it("路径缺失时退回 `name.ext`（不至于把缓存 key 拼成空串）", () => {
        const ref = docFileRef({ docId: "d3", path: "", name: "季度报告", ext: "PDF" });
        expect(ref.fileName).toBe("季度报告.pdf");
    });
});

describe("docEntryFileUrl — 唯一构造者在 doc-api", () => {
    it("拼出文档库的字节路由", () => {
        expect(docEntryFileUrl("doc-1")).toBe(
            "http://gateway:19876/api/doc/docs/doc-1/file",
        );
        expect(docEntryFileUrl("doc 1")).toContain("doc%201");
    });

    it("与缓存实际 fetch 的地址逐字相同（哨兵分支委托给 doc-api）", async () => {
        const ref = docFileRef({ docId: "d1", path: "deck.pptx", name: "deck", ext: "pptx" });
        await fetchWorkspaceFileBlob(ref.agentId, ref.workspaceId, ref.relPath);
        expect(fetchMock).toHaveBeenCalledWith(docEntryFileUrl("d1"));
    });
});

describe("回写刷新", () => {
    it("primeDocFileBlob 之后同一个坐标返回新字节，不再回源", async () => {
        const entry = { docId: "d3", path: "报告.docx", name: "报告", ext: "docx" };
        primeDocFileBlob(entry, new Uint8Array([1, 2, 3]));
        const ref = docFileRef(entry);
        const url = await fetchWorkspaceFileBlobUrl(ref.agentId, ref.workspaceId, ref.relPath);
        expect(url.blobUrl).toBeTruthy();
        expect(fetchMock).not.toHaveBeenCalled();
    });

    it("只有 fileName 时反推出的 name/ext 与正向映射一致", async () => {
        // viewer 只拿得到 fileName（"报告.docx"），回写必须命中 `docFileRef`
        // 用路径末段算出的同一个 key。
        primeDocFileBlobFromFileName("d4", "报告.docx", new Uint8Array([9]));
        const ref = docFileRef({ docId: "d4", path: "归档/报告.docx", name: "报告", ext: "docx" });
        const url = await fetchWorkspaceFileBlobUrl(ref.agentId, ref.workspaceId, ref.relPath);
        expect(url.blobUrl).toBeTruthy();
        expect(fetchMock).not.toHaveBeenCalled();
    });
});