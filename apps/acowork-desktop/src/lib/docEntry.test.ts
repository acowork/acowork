/**
 * `docEntry.ts` — 桌面侧唯一的文档条目形状与它的两条规则。
 *
 * 这些用例钉的是**判定**，不是实现：扩展名从哪来、条目该进文本视图还是字节
 * viewer、文件名（显示名 / 缓存 key）叫什么。它们一旦漂移，症状是「点开一个
 * docx 得到空白编辑器」或「tab 叫 `报告.pdf`、缓存 key 叫 `报告`」这类
 * 静默错位，而不是一个显眼的异常 —— 所以逐条钉住。
 */
import { describe, expect, it } from "vitest";
import {
    docEntryExt,
    docEntryFileName,
    docEntryFromMeta,
    docEntryFromRead,
    docEntryKind,
    isImportedEntry,
    type DocEntryRef,
} from "./docEntry";
import type { DocMeta, DocRead } from "./doc-types";

function meta(patch: Partial<DocMeta> = {}): DocMeta {
    return {
        doc_id: "doc-1",
        name: "纪要",
        version: 3,
        created_at: "2026-01-01T00:00:00Z",
        updated_at: "2026-01-02T00:00:00Z",
        deleted: false,
        ext: null,
        ...patch,
    };
}

function read(path: string, patch: Partial<DocMeta> = {}): DocRead {
    return { meta: meta(patch), content: "# 正文", path };
}

describe("docEntryFileName — 文件名只有一条规则", () => {
    it("取人读路径的末段（服务端渲染的 file_name），不重新拼 name + ext", () => {
        expect(
            docEntryFileName({ docId: "d", path: "项目管理/p-1/评审/v1.2 复盘.pdf", name: "v1.2 复盘", ext: "pdf" }),
        ).toBe("v1.2 复盘.pdf");
    });

    it("路径只有文件名时同样成立", () => {
        expect(docEntryFileName({ docId: "d", path: "纪要.md", name: "纪要", ext: null })).toBe("纪要.md");
    });

    it("路径缺失时退回 `name.ext`，ext 缺省为 md 且小写", () => {
        expect(docEntryFileName({ docId: "d", path: "", name: "纪要", ext: null })).toBe("纪要.md");
        expect(docEntryFileName({ docId: "d", path: "", name: "报告", ext: "PDF" })).toBe("报告.pdf");
    });
});

describe("docEntryExt / docEntryKind — 该进哪个视图", () => {
    const cases: Array<[string, DocEntryRef, string | null, "markdown" | "file"]> = [
        ["显式 ext 优先", { docId: "d", path: "研发/汇报.pptx", name: "汇报", ext: "pptx" }, "pptx", "file"],
        ["没有 ext 字段时从路径末段推", { docId: "d", path: "研发/汇报.xlsx", name: "汇报" }, "xlsx", "file"],
        ["md 文档（ext 为 null）", { docId: "d", path: "研发/纪要.md", name: "纪要", ext: null }, "md", "markdown"],
        ["无扩展名的旧条目按 Markdown 处理", { docId: "d", path: "研发/纪要", name: "纪要" }, null, "markdown"],
        ["大写扩展名归一", { docId: "d", path: "研发/扫描.PNG", name: "扫描", ext: "PNG" }, "png", "file"],
        ["markdown 长写形式也是文本", { docId: "d", path: "研发/纪要.markdown", name: "纪要" }, "markdown", "markdown"],
    ];

    for (const [title, entry, ext, kind] of cases) {
        it(title, () => {
            expect(docEntryExt(entry)).toBe(ext);
            expect(docEntryKind(entry)).toBe(kind);
            expect(isImportedEntry(entry)).toBe(kind === "file");
        });
    }

    it("名字里带点不会把标题误判成扩展名", () => {
        // "1.2 计划" 是标题，不是 ext；扩展名只能来自显式字段或**末段后缀**。
        const entry: DocEntryRef = { docId: "d", path: "研发/1.2 计划.md", name: "1.2 计划", ext: null };
        expect(docEntryExt(entry)).toBe("md");
        expect(isImportedEntry(entry)).toBe(false);
    });
});

describe("从服务端形状构造条目", () => {
    it("docEntryFromRead 把人读路径与元数据合成一个条目", () => {
        const entry = docEntryFromRead(read("研发/季度报告.pdf", { name: "季度报告", ext: "pdf" }));
        expect(entry).toEqual({ docId: "doc-1", path: "研发/季度报告.pdf", name: "季度报告", ext: "pdf" });
        expect(isImportedEntry(entry)).toBe(true);
    });

    it("docEntryFromMeta 的 ext 缺省归一成 null（不是 undefined）", () => {
        const entry = docEntryFromMeta(meta({ ext: undefined }), "研发/纪要.md");
        expect(entry.ext).toBeNull();
        expect(docEntryKind(entry)).toBe("markdown");
    });
});