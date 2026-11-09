//! Setup-panel card composition (the Tools / Debug panel merge).
//!
//! The Setup panel absorbed three cards from the two panels that no longer
//! exist as rail entries:
//!
//!   - "Prompts & Skills" — was the top card of the Debug tab
//!   - "Builtin Tools" / "MCP Servers" — were the whole of the Tools tab
//!
//! Two things are worth pinning, and neither is visible from the card
//! components themselves:
//!
//!   1. ORDER — the three migrated cards land BELOW the four original ones,
//!      so the panel's reading order is stable (see `agentSetupCards.ts`).
//!   2. DEFAULT EXPAND — seven cards all open is a scroll wall, so only the
//!      first three start expanded. `ExpandableRow` publishes that as
//!      `aria-expanded`, which is what this test reads.
//!
//! The cards themselves are rendered for real (not stubbed): the point is that
//! the migrated cards actually mount inside this panel with their own header
//! rows, not that their internals work — those have their own suites.

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import i18n from "../../i18n";
import { useAgentStore, type AgentStorage } from "../../stores/agentStore";
import type { AgentInfo, McpRecommendationView } from "../../lib/types";
import { AgentSetupTab } from "./AgentSetupTab";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => ({})) }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(async () => null) }));
vi.mock("../../lib/gateway-api", () => ({ fetchProviders: vi.fn(async () => []) }));
vi.mock("../../lib/avatar", async (importOriginal) => ({
  // Keep the pure helpers (icon-id picking, URL resolution) real — the user
  // profile store calls them while this module graph initialises.
  ...(await importOriginal<typeof import("../../lib/avatar")>()),
  // Only the network surface is stubbed.
  fetchAvatarAssets: vi.fn(async () => ({ assets: [] })),
  fetchAvatarConfig: vi.fn(async () => null),
  updateAvatarConfig: vi.fn(async () => ({})),
  deleteAvatarFile: vi.fn(async () => ({})),
}));

const AGENT = "agent-1";

/** Builtin tools the stubbed `/tools` endpoint reports. */
let builtinToolNames: string[] = ["load_skill"];

/** ADR-104 recommendations the stubbed `/tools` endpoint reports. */
let recommendedRows: McpRecommendationView[] = [];

function recommended(state: McpRecommendationView["state"]): McpRecommendationView {
  return {
    key: "brave-search",
    source: "preset",
    preset: "brave-search",
    tools: ["brave_web_search"],
    required_secrets: [],
    required: false,
    why: "web research",
    state,
  };
}

function meta(over: Partial<AgentInfo> = {}): AgentInfo {
  return {
    instance_id: "inst-1",
    agent_id: AGENT,
    name: "Tester",
    version: "1.0.0",
    alive: true,
    ...over,
  } as AgentInfo;
}

function seedAgent(profile: Record<string, unknown> = {}) {
  useAgentStore.setState({
    agents: {
      [AGENT]: { meta: meta(), profile, sessions: [] },
    } as unknown as Record<string, AgentStorage>,
    selectedAgentId: AGENT,
  });
}

/** Minimal gateway surface: the tools cards read `/tools` + `/mcp-tools`, the
 *  panel itself reads `/config`, and the prompts card reads the workspace tree
 *  (any 404 is handled by that card, which is fine — its body starts
 *  collapsed anyway). */
function stubFetch() {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const json = (body: unknown) =>
        Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(body) });
      if (url.endsWith("/mcp-tools")) return json({ servers: {} });
      if (url.endsWith("/tools")) {
        return json({
          tools: builtinToolNames.map((name) => ({ name, enabled: true })),
          mcp_servers: [],
          mcp_servers_defs: [],
          recommended: recommendedRows,
        });
      }
      if (url.endsWith("/config")) return json({});
      return Promise.resolve({ ok: false, status: 404, json: () => Promise.resolve({}) });
    }),
  );
}

/** The recommendation group's header (ADR-104) — present only while the
 *  package declares suggestions. */
function recommendationHeader(): HTMLElement {
  return screen.getByRole("button", {
    name: i18n.t("agentSetup.mcpRecommended", { count: recommendedRows.length }),
  });
}

/** Every level-1 card header in the panel, top to bottom. */
function cardHeaders(): HTMLElement[] {
  return Array.from(document.querySelectorAll<HTMLElement>('[role="button"][aria-expanded]'));
}

describe("AgentSetupTab card composition", () => {
  beforeEach(() => {
    builtinToolNames = ["load_skill"];
    recommendedRows = [];
    seedAgent();
    stubFetch();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.clearAllMocks();
  });

  it("renders the seven cards in registry order, migrated ones last", async () => {
    render(<AgentSetupTab />);

    await waitFor(() => expect(cardHeaders().length).toBe(7));

    expect(cardHeaders().map((el) => el.getAttribute("aria-label"))).toEqual([
      i18n.t("agentSetup.sectionAgentInfo"),
      i18n.t("agentSetup.sectionSessionParams"),
      i18n.t("agentSetup.sectionModelParams"),
      i18n.t("agentSetup.sectionChatRouter"),
      i18n.t("debugFiles.title"),
      i18n.t("agentSetup.builtinTools", { count: 1 }),
      i18n.t("agentSetup.mcpServers", { count: 0 }),
    ]);
  });

  it("starts only the first three cards expanded", async () => {
    render(<AgentSetupTab />);

    await waitFor(() => expect(cardHeaders().length).toBe(7));

    expect(cardHeaders().map((el) => el.getAttribute("aria-expanded"))).toEqual([
      "true",
      "true",
      "true",
      "false",
      "false",
      "false",
      "false",
    ]);
  });

  it("lists every builtin tool without an inner scroll cap", async () => {
    // The card used to be a nine-row scroll box back when it owned the whole
    // Tools tab. In the Setup panel its rows are part of the panel's single
    // scrolling column, so the card must not introduce a second scroll
    // position — every tool is rendered in full.
    builtinToolNames = Array.from({ length: 12 }, (_, i) => `tool_${i + 1}`);
    render(<AgentSetupTab />);
    await waitFor(() => expect(cardHeaders().length).toBe(7));

    const header = screen.getByLabelText(i18n.t("agentSetup.builtinTools", { count: 12 }));
    fireEvent.click(header);

    await waitFor(() => expect(screen.getByLabelText("tool_12")).toBeTruthy());
    for (const name of builtinToolNames) expect(screen.getByLabelText(name)).toBeTruthy();

    // No scroll host between the card header and its rows: both resolve to the
    // SAME `.overlay-scrollbar-viewport` (the panel's own one). A re-introduced
    // inner `OverlayScrollbar` would put a second viewport in between. (The
    // thumb/track is not queryable here — it only mounts when the viewport
    // actually overflows, which jsdom never reports.)
    const panelViewport = header.closest(".overlay-scrollbar-viewport");
    expect(panelViewport).toBeTruthy();
    expect(screen.getByLabelText("tool_1").closest(".overlay-scrollbar-viewport")).toBe(panelViewport);
  });

  // ── ADR-104 recommendation card: default expand state ────────────────
  //
  // The rule itself lives in `lib/mcpRecommendations.ts`; what only an
  // integration test can prove is the TIMING — the panel starts with an empty
  // recommendation list and fills it when `/tools` answers, and the card must
  // not be painted in a state it will immediately contradict.

  it("does not paint the recommendation card before its list has loaded", async () => {
    recommendedRows = [recommended("active")];
    render(<AgentSetupTab />);

    // First frame: `/tools` has not answered, so there is nothing to be wrong
    // about — the group is absent rather than expanded-then-collapsed.
    expect(cardHeaders().length).toBe(7);

    await waitFor(() => expect(cardHeaders().length).toBe(8));
    // ...and the card appears already collapsed (every suggestion handled).
    expect(recommendationHeader().getAttribute("aria-expanded")).toBe("false");
  });

  it("opens the recommendation card when a suggestion is still unhandled", async () => {
    recommendedRows = [recommended("installed_inactive")];
    render(<AgentSetupTab />);

    await waitFor(() => expect(cardHeaders().length).toBe(8));
    expect(recommendationHeader().getAttribute("aria-expanded")).toBe("true");
  });

  it("renders exactly one Prompts & Skills card (no duplicate after the move)", async () => {
    // The card moved here from the Debug tab. A leftover copy there — or a
    // second mount inside this panel — would show up as two matches.
    render(<AgentSetupTab />);
    await waitFor(() =>
      expect(screen.getAllByLabelText(i18n.t("debugFiles.title"))).toHaveLength(1),
    );
  });
});

/**
 * The Model Parameters card's default-reasoning list.
 *
 * ADR-110's first draft offered TWO options whose label said 自动 — "" (follow
 * the model's own default) and "auto" (an explicit value that suppresses it and
 * sends no reasoning parameter). Same word in one list twice, so the deletion
 * is pinned here: a fresh profile is offered exactly one, and a config that
 * already persisted `auto` still renders it (the runtime honours it: persisted
 * `auto` wins the first level of `resolve_effective_reasoning_effort`) with a
 * localized label instead of the raw wire token.
 */
describe("AgentSetupTab default reasoning effort options", () => {
  const inheritLabel = () => i18n.t("agentSetup.defaultReasoningInherit");
  const autoLabel = () => i18n.t("chatPanel.reasoningAuto");

  /** The default-reasoning <select>: the one carrying the "no default" row. */
  function effortSelect(): HTMLSelectElement {
    const sel = Array.from(document.querySelectorAll<HTMLSelectElement>("select")).find(
      (s) => Array.from(s.options).some((o) => (o.textContent ?? "") === inheritLabel()),
    );
    if (!sel) throw new Error("default-reasoning select not found");
    return sel;
  }

  const optionLabels = (): string[] =>
    Array.from(effortSelect().options).map((o) => o.textContent ?? "");

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.clearAllMocks();
  });

  it("offers no second 自动 next to 「自动（跟随模型默认）」", async () => {
    seedAgent();
    stubFetch();
    render(<AgentSetupTab />);
    await waitFor(() => expect(cardHeaders().length).toBe(7));

    // Exact match on the removed option's label: a substring check would
    // also hit 「自动（跟随模型默认）」 and pass no matter what.
    expect(optionLabels()).not.toContain(autoLabel());
    expect(optionLabels()).toContain(inheritLabel());
  });

  it("still renders a legacy `auto` default, with a readable label", async () => {
    seedAgent({ defaultReasoningEffort: "auto" });
    stubFetch();
    render(<AgentSetupTab />);

    await waitFor(() => expect(effortSelect().value).toBe("auto"));
    // Not the raw token the option list would otherwise append unchanged.
    expect(effortSelect().selectedOptions[0]?.textContent).toBe(autoLabel());
  });
});
