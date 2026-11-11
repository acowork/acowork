/**
 * PM 徽章的「实底胶囊」语言 —— 样式守卫。
 *
 * 用户要求：状态标签与优先级别签从「浅底 + 彩字」改成「实底胶囊 + 白字」，
 * 底色取全局高亮色体系里的红 / 绿 / 蓝（外加琥珀与中性灰，见下面的映射），
 * 右上角「新建任务」按钮用全局高亮色作背景。
 *
 * 为什么钉源码而不是渲染 DOM：底色是 Tailwind token 工具类
 * （`bg-pill-red` → `var(--color-pill-red)`），jsdom 既不解析也不比较计算
 * 样式；类名字符串本身就是这里的规格。同 `capsule.test.tsx` /
 * `menuItemSelected.test.tsx` 的做法。
 *
 * 三条不能退化的事实：
 *   1. 白字对比度靠的是 token 的**加深档**，不是预设原色 —— rose/green/blue
 *      预设（#f43f5e / #00C375 / #3b82f6）配白字只有 2.3–3.7:1，11px 直接不
 *      合格；所以颜色只在 globals.css 的 `--color-pill-*` 里出现一次。
 *   2. 胶囊里的状态小圆点必须反白，否则同族色底上根本看不见。
 *   3. 六档状态 / 四档优先级压进三个色相会撞色，所以 submitted（待人工审核）
 *      与 high 用琥珀、pending / cancelled / low 用中性灰 —— 语义是「等待」和
 *      「无颜色含义」，不是第四个强调色。
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const here = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(here, "..", "..", rel), "utf8");

/** 从 `start` 到下一个单独一行的 `}`（对象字面量结尾），取一段源码。 */
function block(src: string, start: string): string {
  const from = src.indexOf(start);
  expect(from, `找不到 ${start}`).toBeGreaterThanOrEqual(0);
  const to = src.indexOf("\n}", from);
  expect(to, `${start} 的块结尾没找到`).toBeGreaterThan(from);
  return src.slice(from, to);
}

const PILL_TOKENS = [
  "--color-pill-red",
  "--color-pill-green",
  "--color-pill-blue",
  "--color-pill-amber",
  "--color-pill-neutral",
] as const;

describe("实底胶囊的底色 token", () => {
  const css = read("styles/globals.css");

  it("五枚 token 在 @theme（浅色）与 .dark（加深一档）各定义一次", () => {
    for (const token of PILL_TOKENS) {
      const defs = css.match(new RegExp(`^\\s*${token}:`, "gm")) ?? [];
      expect(defs, `${token} 应当有浅色 + 深色两个值`).toHaveLength(2);
    }
  });

  it("底色只写在 token 里，视图层不出现裸 hex", () => {
    const badge = read("views/pm/PriorityBadge.tsx");
    const labels = read("views/pm/taskListLabels.ts");
    expect(badge).not.toMatch(/#[0-9a-fA-F]{6}/);
    expect(labels).not.toMatch(/#[0-9a-fA-F]{6}/);
  });
});

describe("优先级别签", () => {
  const styles = block(read("views/pm/PriorityBadge.tsx"), "const STYLES: Record<Priority, string> = {");

  it("四档全部走 bg-pill-*，浅底彩字（bg-*-100 / text-*-700）不得回来", () => {
    expect(styles).toMatch(/urgent:\s*"bg-pill-red"/);
    expect(styles).toMatch(/high:\s*"bg-pill-amber"/);
    expect(styles).toMatch(/normal:\s*"bg-pill-blue"/);
    expect(styles).toMatch(/low:\s*"bg-pill-neutral"/);
    expect(styles).not.toMatch(/-100|-200|-700|-950|dark:/);
  });

  it("胶囊是圆头、白字", () => {
    const base = read("views/pm/PriorityBadge.tsx");
    expect(base).toMatch(/rounded-full[^"]*text-white|text-white[^"]*rounded-full/);
  });
});

describe("状态胶囊", () => {
  const labelsSrc = read("views/pm/taskListLabels.ts");
  const pills = block(labelsSrc, "export const STATUS_PILL_CLASS: Record<TaskStatus, string> = {");

  it("六档全部走 bg-pill-*，并且语义分档：done=绿、in_progress=蓝、submitted=琥珀、rejected=红、pending/cancelled=灰", () => {
    expect(pills).toMatch(/pending:\s*"bg-pill-neutral"/);
    expect(pills).toMatch(/in_progress:\s*"bg-pill-blue"/);
    expect(pills).toMatch(/submitted:\s*"bg-pill-amber"/);
    expect(pills).toMatch(/done:\s*"bg-pill-green"/);
    expect(pills).toMatch(/rejected:\s*"bg-pill-red"/);
    expect(pills).toMatch(/cancelled:\s*"bg-pill-neutral"/);
    expect(pills).not.toMatch(/-100|-200|-700|-950|dark:/);
  });

  it("小圆点反白，调用点不再按状态取色（同族色底上彩色点看不见）", () => {
    expect(labelsSrc).toMatch(/export const STATUS_DOT_CLASS = "bg-white\/70"/);
    for (const file of ["views/pm/TaskListRows.tsx", "views/pm/TaskListView.tsx"]) {
      expect(read(file)).not.toMatch(/STATUS_DOT_CLASS\[/);
    }
  });

  it("状态胶囊也是圆头 + 白字", () => {
    const rows = read("views/pm/TaskListRows.tsx");
    expect(rows).toMatch(/rounded-full[^"]*text-white/);
  });
});

describe("右上角「新建任务」", () => {
  const header = read("views/pm/ProjectHeader.tsx");
  const from = header.indexOf("onClick={onNewTask}");

  it("底色是全局高亮色（btn-accent），不再是写死的 zinc", () => {
    expect(from).toBeGreaterThanOrEqual(0);
    const button = header.slice(from, header.indexOf(">", from));
    expect(button).toContain("btn-accent");
    expect(button).not.toMatch(/bg-zinc-[78]00|text-white/);
  });
});
