import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { CHROME_WIDTH } from "./panelClamp";

const src = (path: string) => readFileSync(resolve(__dirname, "..", "..", path), "utf8");

describe("chat pane spacing", () => {
  it("keeps 2px gaps, 4px resize targets and matching viewport accounting", () => {
    const layout = src("components/layout/AppLayout.tsx");
    expect(layout.match(/group relative z-10 w-0\.5 shrink-0 cursor-col-resize/g)).toHaveLength(2);
    expect(layout.match(/absolute inset-y-0 -left-px w-1 group-hover/g)).toHaveLength(2);
    for (const file of ["right-panel/RightPanel.tsx", "chat/GroupMembersPanel.tsx", "chat/UserGroupMembersPanel.tsx"]) {
      const panel = src(`components/${file}`);
      expect(panel).toContain("ml-0.5");
      expect(panel).toContain("-left-[3px] top-0 bottom-0");
      expect(panel).toContain("w-1 cursor-col-resize");
    }
    expect(CHROME_WIDTH).toBe(48 + 40 + 2 + 2 + 2);
  });

  it("only changes default chat pane borders, including dark mode", () => {
    const css = src("styles/globals.css");
    expect(css).toContain(":root:not([data-theme-pack]) .chat-pane-row > :is(.bg-nav-surface, .bg-chat-body, .bg-right-panel)");
    expect(css).toContain(":root:not([data-theme-pack]) .chat-pane-row > .relative > .bg-right-panel");
    expect(css).toContain(":root.dark:not([data-theme-pack]) .chat-pane-row > .relative > .bg-right-panel");
    expect(css).toContain("border-color: hsl(0 0% 75%)");
    expect(css).toContain("border-color: var(--color-border-outer)");
  });
});
