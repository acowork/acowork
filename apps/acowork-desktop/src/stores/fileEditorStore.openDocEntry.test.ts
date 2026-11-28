/**
 * `fileEditorStore.openDocEntry` — 文档条目的**唯一入口**，以及它按扩展名分派
 * 的那一次判定。
 *
 * 为什么值得一个 store 级用例（而不是只测 `docEntry.ts` 的纯函数）：
 * 判定错了不会抛异常，只会让 tab 长成另一个样子 —— 导入条目被当成 Markdown
 * 去 `getDoc` 拿正文，拿到空的 `content`，用户看到的是**空白编辑器**（这正是
 * 「点 docx 没反应」那个 bug 的形状）；反过来 Markdown 被当字节 viewer，就会
 * 去请求一个不存在的字节路由。两条都在这里钉住。
 *
 * 顺带钉住 tab 身份：`doc:${docId}`，与路径无关 —— 改名 / 移动之后重开必须
 * 复用同一个 tab，而不是再开一个。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const getDocMock = vi.fn();

vi.mock("../lib/doc-api", async () => {
    const actual = await vi.importActual<typeof import("../lib/doc-api")>("../lib/doc-api");
    return { ...actual, getDoc: (...args: unknown[]) => getDocMock(...args) };
});

vi.mock("../lib/logger", () => ({
    log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
    setLevel: () => {},
    getLevel: () => "off" as const,
}));

import { useFileEditorStore } from "./fileEditorStore";
import type { DocEntryRef } from "../lib/docEntry";

function entry(patch: Partial<DocEntryRef> = {}): DocEntryRef {
    return { docId: "doc-1", path: "研发/纪要.md", name: "纪要", ext: null, ...patch };
}

function tab(docId: string) {
    return useFileEditorStore.getState().openFiles.find((f) => f.id === `doc:${docId}`);
}

beforeEach(() => {
    useFileEditorStore.setState({ openFiles: [], activeFileId: null });
    getDocMock.mockReset();
    getDocMock.mockResolvedValue({
        meta: {
            doc_id: "doc-1",
            name: "季度报告",
            version: 7,
            created_at: "2026-01-01T00:00:00Z",
            updated_at: "2026-01-02T00:00:00Z",
            deleted: false,
            ext: null,
        },
        content: "# 正文",
        path: "研发/季度报告.md",
    });
});

describe("Markdown 条目", () => {
    it("先建 tab（带 spinner）再拉正文，回来后填 content 与最新路径", async () => {
        useFileEditorStore.getState().openDocEntry(entry());
        const pending = tab("doc-1");
        expect(pending?.kind).toBe("doc");
        expect(pending?.loading).toBe(true);

        await vi.waitFor(() => expect(tab("doc-1")?.loading).toBe(false));
        const loaded = tab("doc-1");
        expect(loaded?.content).toBe("# 正文");
        expect(loaded?.docVersion).toBe(7);
        // 服务端返回的人读路径才是权威（可能刚被改名 / 移动过）。
        expect(loaded?.docEntry?.path).toBe("研发/季度报告.md");
        expect(loaded?.fileName).toBe("季度报告.md");
    });

    it("拉正文失败时 tab 不空白，错误落在 saveError 上", async () => {
        getDocMock.mockRejectedValue(new Error("denied"));
        useFileEditorStore.getState().openDocEntry(entry());
        await vi.waitFor(() => expect(tab("doc-1")?.saveError).toBe("denied"));
        expect(tab("doc-1")?.loading).toBe(false);
    });
});

describe("导入文件条目", () => {
    it("不拉正文、不挂 spinner，直接把条目交给字节 viewer", () => {
        useFileEditorStore.getState().openDocEntry(
            entry({ docId: "doc-pdf", path: "研发/季度报告.pdf", name: "季度报告", ext: "pdf" }),
        );
        const created = tab("doc-pdf");
        expect(created?.loading).toBe(false);
        expect(created?.content).toBe("");
        expect(created?.docEntry).toEqual({
            docId: "doc-pdf",
            path: "研发/季度报告.pdf",
            name: "季度报告",
            ext: "pdf",
        });
        expect(created?.fileName).toBe("季度报告.pdf");
        expect(getDocMock).not.toHaveBeenCalled();
    });

    it("扩展名只写在路径里（聊天卡片拿不到 ext）时同样判成字节 viewer", () => {
        useFileEditorStore.getState().openDocEntry(
            entry({ docId: "doc-pptx", path: "汇报/季度复盘.pptx", name: "季度复盘", ext: null }),
        );
        const created = tab("doc-pptx");
        expect(created?.loading).toBe(false);
        expect(getDocMock).not.toHaveBeenCalled();
    });
});

describe("tab 身份", () => {
    it("同一个 docId 重开只切焦点，不重复拉正文", async () => {
        useFileEditorStore.getState().openDocEntry(entry());
        await vi.waitFor(() => expect(tab("doc-1")?.loading).toBe(false));
        useFileEditorStore.getState().openDocEntry(entry({ path: "改名后的路径.md" }));
        expect(useFileEditorStore.getState().openFiles).toHaveLength(1);
        expect(useFileEditorStore.getState().activeFileId).toBe("doc:doc-1");
        expect(getDocMock).toHaveBeenCalledTimes(1);
    });
});