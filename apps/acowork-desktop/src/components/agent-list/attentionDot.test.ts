/**
 * Regression guard for the sidebar's attention dot.
 *
 * Source-scanned like `nodeGroupHeader.test.ts`, and for the same reason: the
 * behaviour worth protecting here is POSITION and WIRING, not pixels. Two ways
 * this badge silently stops working, both of which have to fail a test:
 *
 * 1. It renders inside the row's content column. That column is `w-0` whenever
 *    the sidebar is collapsed, so a dot there is present-but-invisible — the
 *    exact failure mode the feature exists to remove (a parked approval that
 *    nobody can see).
 * 2. It stops being derived from the session attention rule, or starts
 *    requiring a live agent that is offline — a dead Runtime's retained state
 *    still says `waiting_approval` forever.
 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const read = (p: string) => readFileSync(resolve(process.cwd(), p), "utf8");

const agentList = read("src/components/agent-list/AgentList.tsx");
const tabBar = read("src/components/chat/SessionTabBar.tsx");
const en = JSON.parse(read("src/i18n/locales/en.json")) as Record<string, any>;
const zh = JSON.parse(read("src/i18n/locales/zh-CN.json")) as Record<string, any>;

describe("agent row attention dot", () => {
  it("is derived from the session attention rule, not from a bespoke status check", () => {
    expect(agentList).toContain('import { attentionByAgent } from "../../lib/sessionAttention"');
    expect(agentList).toContain("attentionByAgent(sessionStatesByAgent, visible)");
    // One dot per agent comes from the shared map, so the sidebar and the tab
    // strip cannot disagree about what "parked" means.
    expect(agentList).toContain("attentionByAgentId.get(id)");
  });

  it("never badges an agent that is offline", () => {
    // A stopped Runtime cannot be holding a card, and its retained state is
    // whatever it last published — a red dot there would be permanent.
    const derived = agentList.slice(agentList.indexOf("attentionByAgentId = useMemo"));
    expect(derived.slice(0, derived.indexOf("}, ["))).toContain(
      "if (!isLive(agentId)) derived.delete(agentId)",
    );
    expect(derived).toContain("agentsMap[agentId]?.meta?.alive === true");
  });

  it("does not badge the session that is already on screen", () => {
    // The foreground session's card is in front of the reader; the suppression
    // is passed as `visible`, so it stays one rule for every surface.
    expect(agentList).toContain("activeSessionId ?? null");
  });

  it("sits on the avatar, above the collapsible content column", () => {
    const dot = agentList.indexOf('data-testid="agent-attention-dot"');
    const avatar = agentList.indexOf("<AgentAvatar");
    const content = agentList.indexOf("Content area — width-collapsed");
    expect(dot).toBeGreaterThan(avatar);
    expect(content).toBeGreaterThan(dot);
  });

  it("is red, small and round — the same slot as the group unread badge", () => {
    const start = agentList.indexOf('data-testid="agent-attention-dot"');
    const dot = agentList.slice(start, agentList.indexOf("/>", start));
    expect(dot).toContain("bg-red-500");
    expect(dot).toContain("rounded-full");
    expect(dot).toContain("-right-1 -top-1");
  });

  it("carries a label for hover and screen readers", () => {
    const start = agentList.indexOf('data-testid="agent-attention-dot"');
    const dot = agentList.slice(start, agentList.indexOf("/>", start));
    expect(dot).toContain('t("agentList.dotAttention")');
  });
});

describe("session tab attention dot", () => {
  it("is shown only for a tab that is not in front", () => {
    // The active tab's card is on screen; a dot there would repeat it.
    expect(tabBar).toContain("const attention = !isActive && needsAttention(status)");
    expect(tabBar).toContain('import { needsAttention } from "../../lib/sessionAttention"');
  });

  it("is red and labelled", () => {
    const start = tabBar.indexOf('data-testid="session-tab-attention-dot"');
    expect(start).toBeGreaterThan(-1);
    const dot = tabBar.slice(start, tabBar.indexOf("/>", start));
    expect(dot).toContain("bg-red-500");
    expect(dot).toContain('t("sessionTabBar.attention")');
  });
});

describe("attention labels", () => {
  it("exists in both enforced locales", () => {
    // check-i18n enforces en + zh-CN; ja/ko/zh-TW fall back to en.
    expect(en.agentList.dotAttention).toBe("Waiting for your confirmation");
    expect(zh.agentList.dotAttention).toBe("等待你的确认");
    expect(en.sessionTabBar.attention).toBe("Waiting for your confirmation");
    expect(zh.sessionTabBar.attention).toBe("等待你的确认");
  });
});
