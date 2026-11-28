/**
 * 扩展名表的单一来源校验（P4 / 设计 §7 R4）。
 *
 * 服务端的 `core/acowork-doc/src/types.rs::IMPORTABLE_EXTS` 决定「哪些格式能进
 * 文档库」，桌面的 `DOC_IMPORT_EXTS` 决定「上传对话框让用户选哪些」。两份表分开
 * 是必然的（一边是 Rust 常量、一边是文件选择器），但**不允许漂移**：
 *
 *   - 服务端多一个、桌面少一个 → 用户选不到一个其实支持的格式（功能悄悄缺一块）；
 *   - 桌面多一个、服务端少一个 → 用户选中后被拒收（错误出现在最晚的时刻）。
 *
 * 两种都是静默的，只有测试能兜住。这个用例直接读 Rust 源文件解析那张表 —— 不是
 * 复制一份期望值，否则漂移只是被测试一起搬走了。
 */
import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { DOC_IMPORT_ACCEPT, DOC_IMPORT_EXTS } from "./doc-api";

const here = dirname(fileURLToPath(import.meta.url));
const TYPES_RS = resolve(here, "../../../../core/acowork-doc/src/types.rs");

/** 从 `IMPORTABLE_EXTS` 条目里取扩展名：`("pdf", ImportKind { … })`。 */
function serverExts(): string[] {
    const source = readFileSync(TYPES_RS, "utf8");
    const start = source.indexOf("pub const IMPORTABLE_EXTS");
    expect(start, "IMPORTABLE_EXTS 不在 types.rs 里了？").toBeGreaterThan(-1);
    const end = source.indexOf("];", start);
    const table = source.slice(start, end);
    return [...table.matchAll(/\(\s*"([a-z0-9]+)"\s*,\s*ImportKind/g)].map((m) => m[1]);
}

describe("文档库扩展名表", () => {
    it("桌面与 acowork-doc 的扩展名集合完全一致", () => {
        const server = serverExts();
        // 解析本身先自证：解析不出来时下面会「两边都空」而假绿。
        expect(server.length).toBeGreaterThan(10);
        expect([...DOC_IMPORT_EXTS].sort()).toEqual([...server].sort());
    });

    it("没有重复项，且都是小写不带点", () => {
        const exts = [...DOC_IMPORT_EXTS];
        expect(new Set(exts).size).toBe(exts.length);
        for (const ext of exts) expect(ext).toBe(ext.toLowerCase());
        for (const ext of exts) expect(ext.startsWith(".")).toBe(false);
    });

    it("accept 串覆盖每一个扩展名（用户都选得到）", () => {
        const accept = DOC_IMPORT_ACCEPT.split(",");
        for (const ext of DOC_IMPORT_EXTS) {
            expect(accept, `${ext} 没进 accept 串`).toContain(`.${ext}`);
        }
    });
});