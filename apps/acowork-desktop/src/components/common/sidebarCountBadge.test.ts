/**
 * The chat sidebar's group banners dress their counts as bubbles.
 *
 * The report: the PM board's column counts were pills (a rounded fill
 * behind the number), while the chat sidebar's Users / Project-groups
 * banners showed the same kind of number as bare dimmed text
 * (`text-xs font-normal opacity-60`) — the same digit read as a different
 * kind of element depending on which pane it was in.
 *
 * The contract:
 *   1. Both banners render the count through the shared constant
 *      (`COUNT_BADGE_NEUTRAL_CN`), not a hand-rolled class list.
 *   2. The constant itself carries the bubble shape the PM board uses —
 *      rounded, padded, small, medium weight, tabular figures — so the
 *      two families cannot drift apart on geometry again.
 *
 * Source scanning, not rendered DOM: AgentList pulls in the Tauri plugin
 * dialog, the gateway API and the agent store, so a render assertion there
 * would be brittle. UserList *is* renderable, but the property under test
 * is "both banners share one constant", which is a cross-file statement —
 * reading the markup is the honest way to assert it. Same trade-off as
 * nodeGroupHeader.test.ts — the markup IS the spec.
 */
import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import {
  COUNT_BADGE_BASE_CN,
  COUNT_BADGE_NEUTRAL_CN,
} from "./sidebarCountBadge";

const __dirname = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(__dirname, rel), "utf8");

const AGENT_LIST = read("../agent-list/AgentList.tsx");
const USER_LIST = read("../user-list/UserList.tsx");

describe("sidebar count bubbles", () => {
  it("the shared constant carries the PM bubble shape", () => {
    // These are the exact classes the PM board's column count used
    // (KanbanColumn.tsx). If the shape changes, it changes in one place.
    expect(COUNT_BADGE_BASE_CN).toContain("rounded-full");
    expect(COUNT_BADGE_BASE_CN).toContain("px-1.5");
    expect(COUNT_BADGE_BASE_CN).toContain("py-px");
    expect(COUNT_BADGE_BASE_CN).toContain("text-10");
    expect(COUNT_BADGE_BASE_CN).toContain("font-medium");
    expect(COUNT_BADGE_BASE_CN).toContain("tabular-nums");
  });

  it("the neutral variant adds the zinc fill on top of the shared shape", () => {
    expect(COUNT_BADGE_NEUTRAL_CN).toContain(COUNT_BADGE_BASE_CN);
    expect(COUNT_BADGE_NEUTRAL_CN).toContain("bg-zinc-200");
    expect(COUNT_BADGE_NEUTRAL_CN).toContain("dark:bg-zinc-700");
  });

  it("the Users banner renders its count through the shared constant", () => {
    expect(USER_LIST).toContain("COUNT_BADGE_NEUTRAL_CN");
    // The old bare-dimmed-text treatment must be gone — that was the
    // whole point of the change.
    expect(USER_LIST).not.toMatch(/font-normal opacity-60/);
  });

  it("the Project-groups banner renders its count through the shared constant", () => {
    expect(AGENT_LIST).toContain("COUNT_BADGE_NEUTRAL_CN");
    expect(AGENT_LIST).not.toMatch(/font-normal opacity-60/);
  });

  it("the PM board column count reuses the same base shape", () => {
    const kanban = read("../../views/pm/KanbanColumn.tsx");
    expect(kanban).toContain("COUNT_BADGE_BASE_CN");
    expect(kanban).not.toContain('"rounded-full px-1.5 py-px text-10 font-medium tabular-nums"');
  });
});
