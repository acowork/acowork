/**
 * `DocMarkdownView` — 文档正文里的链接。
 *
 * 这篇文档的正文是另一份 markdown，里面写的是**库内相对链接**
 * （`./设计.md`、`../评审/y.md`）。它们过去被渲染成 `target="_blank"` 的裸 `<a>`，
 * 点了既不打开也不报错 —— 与「聊天里点 doc 链接没反应」是同一个症状。
 *
 * 钉住的分派：
 *   - 库内路径 → 拦下导航，交给 `openDocLinkOrNotify`，基准是**当前文档路径**；
 *   - 外部 http(s) / `mailto:` → 不拦，保持新窗口行为；
 *   - 页内锚点 → 什么都不做。
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";

const h = vi.hoisted(() => ({ openDocLink: vi.fn(async () => {}) }));

vi.mock("../../components/editor/markdownLinkResolver", () => ({
    openDocLinkOrNotify: h.openDocLink,
}));
vi.mock("../../components/chat/CodeBlock", () => ({
    CodeBlock: () => <pre data-testid="code-block" />,
}));

import { DocMarkdownView } from "./DocMarkdownView";

beforeEach(() => {
    h.openDocLink.mockClear();
});

function renderWithLink(href: string) {
    render(
        <DocMarkdownView
            content={`见 [目标](${href})`}
            basePath="项目管理/p-1/评审/纪要.md"
        />,
    );
    return screen.getByText("目标");
}

describe("DocMarkdownView 链接分派", () => {
    it("库内相对路径带上当前文档作为基准（中文路径已解码）", () => {
        const a = renderWithLink("../设计/方案.md");
        fireEvent.click(a);
        expect(h.openDocLink).toHaveBeenCalledWith("../设计/方案.md", {
            basePath: "项目管理/p-1/评审/纪要.md",
        });
    });

    it("外部链接不拦截（保持新窗口打开）", () => {
        const a = renderWithLink("https://example.com/x");
        fireEvent.click(a);
        expect(h.openDocLink).not.toHaveBeenCalled();
        expect(a.getAttribute("target")).toBe("_blank");
    });

    it("页内锚点不拦截", () => {
        const a = renderWithLink("#小结");
        fireEvent.click(a);
        expect(h.openDocLink).not.toHaveBeenCalled();
    });

    it("透传协议（mailto）不拦截", () => {
        const a = renderWithLink("mailto:a@b.c");
        fireEvent.click(a);
        expect(h.openDocLink).not.toHaveBeenCalled();
    });
});