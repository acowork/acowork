/**
 * 聊天里的 Markdown 链接 → 文档库（doc 域收敛 P3 的行为契约）。
 *
 * 这是用户报的那个 bug 的正面：agent 交付物写的是**文档库路径**
 * （`项目管理/p-x/评审/y.md`、`….pdf`、`….pptx`），而点击处理器过去只认工作区
 * 文件，于是链接全部落到「未找到」，观感就是「点了没反应」。
 *
 * 钉住的四件事：
 *   1. 工作区阶段不适用时（无会话 / 无 agent）**不是终点** —— 继续问文档库；
 *   2. 命中 → `openDocEntry`（由它按扩展名决定文本视图还是字节 viewer）；
 *   3. `denied` → 权限文案，且不打开任何 tab；
 *   4. `missing` → 「工作区和文档库都没有」的文案，不打开任何 tab。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const h = vi.hoisted(() => ({
    openDocEntry: vi.fn(),
    openUrl: vi.fn(),
    openFile: vi.fn(),
    openPreview: vi.fn(),
    showToast: vi.fn(),
    resolveDocEntries: vi.fn(),
}));

vi.mock("../../stores/agentStore", () => ({
    useAgentStore: { getState: () => ({ selectedAgentId: h.agent.id }) },
}));
vi.mock("../../stores/chatStore", () => ({
    useChatStore: { getState: () => ({ getActiveSessionId: () => h.chat.sessionId }) },
}));
vi.mock("../../stores/workspaceStore", () => ({
    useWorkspaceStore: { getState: () => ({ getSessionWorkspaceId: () => "ws1" }) },
}));
vi.mock("../../stores/fileTree", () => ({ getCachedWorkspaceRoot: () => "/root/ws1" }));
vi.mock("../../stores/fileEditorStore", () => ({
    useFileEditorStore: {
        getState: () => ({
            openDocEntry: h.openDocEntry,
            openUrl: h.openUrl,
            openFile: h.openFile,
            openPreview: h.openPreview,
        }),
    },
}));
vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://gw.test" }));
vi.mock("../../lib/logger", () => ({
    log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));
vi.mock("../../lib/doc-api", async () => {
    const actual = await vi.importActual<typeof import("../../lib/doc-api")>("../../lib/doc-api");
    return {
        ...actual,
        resolveDocEntries: (...args: unknown[]) => h.resolveDocEntries(...args),
    };
});
vi.mock("../common/ToastProvider", () => ({ showToast: h.showToast }));
vi.mock("../../i18n", () => ({
    default: { t: (key: string, params?: Record<string, unknown>) => `${key}:${JSON.stringify(params)}` },
}));

// `h` is read lazily by the mocks above, so the mutable knobs live here.
import { handleChatMarkdownLinkClick } from "./markdownLinkResolver";

Object.assign(h, { agent: { id: null as string | null }, chat: { sessionId: null as string | null } });

beforeEach(() => {
    h.agent.id = "a1";
    h.chat.sessionId = null;
    h.openDocEntry.mockReset();
    h.openUrl.mockReset();
    h.openFile.mockReset();
    h.openPreview.mockReset();
    h.showToast.mockReset();
    h.resolveDocEntries.mockReset();
});

const settle = () => new Promise((r) => setTimeout(r, 0));

describe("聊天链接 → 文档库兜底", () => {
    it("没有活动会话也照样问文档库（不再早退）", async () => {
        h.resolveDocEntries.mockResolvedValue([
            {
                ref: "项目管理/p-1/评审/spec.md",
                status: "found",
                doc_id: "doc-1",
                path: "项目管理/p-1/评审/spec.md",
                name: "spec",
                ext: null,
            },
        ]);
        handleChatMarkdownLinkClick("项目管理/p-1/评审/spec.md");
        await settle();
        expect(h.resolveDocEntries).toHaveBeenCalledWith(["项目管理/p-1/评审/spec.md"]);
        expect(h.openDocEntry).toHaveBeenCalledWith({
            docId: "doc-1",
            path: "项目管理/p-1/评审/spec.md",
            name: "spec",
            ext: null,
        });
        expect(h.showToast).not.toHaveBeenCalled();
    });

    it("导入文件（pdf/pptx）同样交给 openDocEntry，由它决定用哪个 viewer", async () => {
        h.resolveDocEntries.mockResolvedValue([
            {
                ref: "项目管理/p-1/汇报/复盘.pptx",
                status: "found",
                doc_id: "doc-7",
                path: "项目管理/p-1/汇报/复盘.pptx",
                name: "复盘",
                ext: "pptx",
            },
        ]);
        handleChatMarkdownLinkClick("项目管理/p-1/汇报/复盘.pptx");
        await settle();
        expect(h.openDocEntry).toHaveBeenCalledWith({
            docId: "doc-7",
            path: "项目管理/p-1/汇报/复盘.pptx",
            name: "复盘",
            ext: "pptx",
        });
    });

    it("中文路径的百分号编码被解回原文再问服务端", async () => {
        h.resolveDocEntries.mockResolvedValue([
            { ref: "研发/纪要.md", status: "found", doc_id: "doc-2", path: "研发/纪要.md", name: "纪要", ext: null },
        ]);
        handleChatMarkdownLinkClick("%E7%A0%94%E5%8F%91/%E7%BA%AA%E8%A6%81.md");
        await settle();
        expect(h.resolveDocEntries).toHaveBeenCalledWith(["研发/纪要.md"]);
        expect(h.openDocEntry).toHaveBeenCalled();
    });

    it("denied 单独文案，且不打开任何 tab", async () => {
        h.resolveDocEntries.mockResolvedValue([{ ref: "研发/机密.md", status: "denied" }]);
        handleChatMarkdownLinkClick("研发/机密.md");
        await settle();
        expect(h.openDocEntry).not.toHaveBeenCalled();
        expect(h.showToast).toHaveBeenCalledTimes(1);
        expect(String(h.showToast.mock.calls[0][0].message)).toContain("fileEditor.markdownLinkDenied");
    });

    it("两处都没有 → 「工作区和文档库都没有」文案", async () => {
        h.resolveDocEntries.mockResolvedValue([{ ref: "研发/没有这个.md", status: "not_found" }]);
        handleChatMarkdownLinkClick("研发/没有这个.md");
        await settle();
        expect(h.openDocEntry).not.toHaveBeenCalled();
        expect(String(h.showToast.mock.calls[0][0].message)).toContain("fileEditor.markdownLinkNotFound");
    });

    it("http(s) 仍然直接开 URL tab，不问文档库", async () => {
        handleChatMarkdownLinkClick("https://example.com/doc");
        await settle();
        expect(h.openUrl).toHaveBeenCalledWith("a1", "https://example.com/doc");
        expect(h.resolveDocEntries).not.toHaveBeenCalled();
    });

    it("页内锚点与透传协议什么都不做", async () => {
        handleChatMarkdownLinkClick("#summary");
        handleChatMarkdownLinkClick("mailto:a@b.c");
        await settle();
        expect(h.resolveDocEntries).not.toHaveBeenCalled();
        expect(h.openUrl).not.toHaveBeenCalled();
        expect(h.showToast).not.toHaveBeenCalled();
    });
});