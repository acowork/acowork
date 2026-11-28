/**
 * `lib/linkResolver.ts` — 一个 href 归属谁，以及库内地址解析成什么。
 *
 * 这里钉的是**判定矩阵**，因为链接触发的问题全是静默的：判错归属只是「点了没
 * 反应」，而 `denied` 与 `missing` 合流会把权限问题报成「文件不存在」，让人去
 * 找一个根本打错方向的错字。
 *
 * 网络只到 `doc-api` 一层 —— `resolveDocEntries` 被 mock，不做 HTTP。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const resolveDocEntriesMock = vi.fn();

vi.mock("./doc-api", async () => {
    const actual = await vi.importActual<typeof import("./doc-api")>("./doc-api");
    return { resolveDocEntries: (...args: unknown[]) => resolveDocEntriesMock(...args) };
});

import {
    classifyMarkdownLink,
    docLinkRefs,
    parseChatLinkHref,
    resolveDocLink,
    resolveLibraryPath,
} from "./linkResolver";

beforeEach(() => {
    resolveDocEntriesMock.mockReset();
});

describe("classifyMarkdownLink — 归属判定", () => {
    const cases: Array<[string, string, Record<string, unknown>]> = [
        ["空 href 是锚点（什么都不做）", "", { kind: "anchor" }],
        ["页内锚点", "#section", { kind: "anchor" }],
        ["http", "http://example.com/a.md", { kind: "external", url: "http://example.com/a.md" }],
        ["https 大小写不敏感", "HTTPS://example.com/a.md", { kind: "external" }],
        ["mailto 透传", "mailto:a@b.c", { kind: "passthrough" }],
        ["data 透传", "data:text/plain,hi", { kind: "passthrough" }],
        ["blob 透传", "blob:http://gw/abc", { kind: "passthrough" }],
        ["库内相对路径", "研发/纪要.md", { kind: "path", path: "研发/纪要.md" }],
        ["导入文件路径也是 path", "研发/设计.pdf", { kind: "path", path: "研发/设计.pdf" }],
        ["盘符路径", "D:/repo/a.md", { kind: "path", path: "D:/repo/a.md" }],
        ["URL 化的盘符路径去掉前导斜杠", "/D:/repo/a.md", { kind: "path", path: "D:/repo/a.md" }],
        ["行号锚点", "src/foo.ts#L42", { kind: "path", path: "src/foo.ts", line: 42 }],
        ["非行号锚点不产生行号", "研发/纪要.md#小结", { kind: "path", path: "研发/纪要.md" }],
    ];

    for (const [title, href, expected] of cases) {
        it(title, () => {
            expect(classifyMarkdownLink(href)).toMatchObject(expected);
        });
    }

    it("原样保留 href，提示文案才能回显用户点的那一条", () => {
        expect(classifyMarkdownLink("研发/纪要.md#L3").href).toBe("研发/纪要.md#L3");
    });
});

describe("parseChatLinkHref — 单一实现（原本在 markdownLinkResolver）", () => {
    it("行号区间取起始行", () => {
        expect(parseChatLinkHref("a/b.ts#L42-L67")).toEqual({ path: "a/b.ts", line: 42 });
    });

    it("空锚点不产生行号", () => {
        expect(parseChatLinkHref("a/b.md#")).toEqual({ path: "a/b.md", line: undefined });
    });

    it("解掉 react-markdown 对中文路径做的百分号编码", () => {
        expect(parseChatLinkHref("../%E8%AE%BE%E8%AE%A1/%E6%96%B9%E6%A1%88.md")).toEqual({
            path: "../设计/方案.md",
            line: undefined,
        });
    });

    it("含裸 `%` 的非法编码原样保留（交给服务端报非法地址）", () => {
        expect(parseChatLinkHref("研发/100%完成.md")).toEqual({
            path: "研发/100%完成.md",
            line: undefined,
        });
    });
});

describe("resolveDocLink — 库内地址解析", () => {
    it("found 映射成可打开的条目", async () => {
        resolveDocEntriesMock.mockResolvedValue([
            {
                ref: "研发/设计.pdf",
                status: "found",
                doc_id: "doc-9",
                path: "研发/设计.pdf",
                name: "设计",
                ext: "pdf",
            },
        ]);
        const outcome = await resolveDocLink("研发/设计.pdf");
        expect(outcome).toEqual({
            status: "found",
            entry: { docId: "doc-9", path: "研发/设计.pdf", name: "设计", ext: "pdf" },
        });
        expect(resolveDocEntriesMock).toHaveBeenCalledWith(["研发/设计.pdf"]);
    });

    it("Markdown 文档的 ext 是 null —— 与「缺失」不是同一件事", async () => {
        resolveDocEntriesMock.mockResolvedValue([
            { ref: "研发/纪要.md", status: "found", doc_id: "doc-1", path: "研发/纪要.md", name: "纪要", ext: null },
        ]);
        const outcome = await resolveDocLink("研发/纪要.md");
        expect(outcome.status).toBe("found");
        expect(outcome.status === "found" && outcome.entry.ext).toBeNull();
    });

    it("denied 单独成一个状态（条目存在但无权查看）", async () => {
        resolveDocEntriesMock.mockResolvedValue([{ ref: "研发/机密.md", status: "denied" }]);
        expect(await resolveDocLink("研发/机密.md")).toEqual({ status: "denied" });
    });

    it("not_found 与 invalid 对点击者是同一件事", async () => {
        resolveDocEntriesMock.mockResolvedValueOnce([{ ref: "x", status: "not_found" }]);
        expect(await resolveDocLink("x")).toEqual({ status: "missing" });
        resolveDocEntriesMock.mockResolvedValueOnce([{ ref: "x", status: "invalid" }]);
        expect(await resolveDocLink("x")).toEqual({ status: "missing" });
    });

    it("服务异常按 missing 处理，点击处理器不抛", async () => {
        resolveDocEntriesMock.mockRejectedValue(new Error("gateway down"));
        expect(await resolveDocLink("研发/纪要.md")).toEqual({ status: "missing" });
    });

    it("空地址不问服务端，直接 missing（不做模糊匹配）", async () => {
        expect(await resolveDocLink("   ")).toEqual({ status: "missing" });
        expect(resolveDocEntriesMock).not.toHaveBeenCalled();
    });

    it("路径渲染失败时退回用户输入的那一条，不回退成内部 id", async () => {
        resolveDocEntriesMock.mockResolvedValue([
            { ref: "doc-3", status: "found", doc_id: "doc-3", name: "纪要", ext: null },
        ]);
        const outcome = await resolveDocLink("doc-3");
        expect(outcome.status === "found" && outcome.entry.path).toBe("doc-3");
    });
});
describe("resolveLibraryPath — 正文里的相对链接", () => {
    it("同级引用按当前文档所在目录解析", () => {
        expect(resolveLibraryPath("项目管理/p-1/评审/a.md", "./b.md")).toBe("项目管理/p-1/评审/b.md");
        expect(resolveLibraryPath("项目管理/p-1/评审/a.md", "b.md")).toBe("项目管理/p-1/评审/b.md");
    });

    it("上一级引用", () => {
        expect(resolveLibraryPath("项目管理/p-1/评审/a.md", "../设计/d.md")).toBe("项目管理/p-1/设计/d.md");
    });

    it("根起（前导斜杠）不受当前目录影响", () => {
        expect(resolveLibraryPath("项目管理/p-1/评审/a.md", "/研发/纪要.md")).toBe("研发/纪要.md");
    });

    it("越过库根的 `..` 被钳制，不产出非法地址", () => {
        expect(resolveLibraryPath("a.md", "../../../x.md")).toBe("x.md");
    });

    it("重复斜杠与 `.` 归一", () => {
        expect(resolveLibraryPath("研发/a.md", "./子//b.md")).toBe("研发/子/b.md");
    });

    it("空 href 得到空地址（调用方据此报「没有」）", () => {
        expect(resolveLibraryPath("研发/a.md", "   ")).toBe("");
    });
});

describe("docLinkRefs — 两种合法读法，去重", () => {
    it("相对写法先按当前目录，再按库根读一次（后者可能只是根下同名条目）", () => {
        expect(docLinkRefs("项目管理/p-1/评审/a.md", "b.md")).toEqual([
            "项目管理/p-1/评审/b.md",
            "b.md",
        ]);
    });

    it("两种读法不同时都保留（顺序即优先级）", () => {
        expect(docLinkRefs("项目管理/p-1/a.md", "评审/b.md")).toEqual([
            "项目管理/p-1/评审/b.md",
            "评审/b.md",
        ]);
    });

    it("行号锚点先被剥掉", () => {
        expect(docLinkRefs("", "研发/a.md#L10")).toEqual(["研发/a.md"]);
    });
});
