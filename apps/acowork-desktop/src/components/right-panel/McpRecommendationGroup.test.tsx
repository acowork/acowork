//! ADR-104: behaviour of the Tools-panel "recommended MCP servers" group.
//!
//! The group is the *reason* a package is allowed to declare
//! `[[mcp_servers]]` at all — a declaration the user cannot see or act on
//! would be decoration. So these tests pin the four observable outcomes the
//! design promises, one per state, plus the two failure paths:
//!
//!   1. visible            → the author's `why` and the gained tool names render
//!   2. one-click install  → `POST /api/mcp-catalog/from-declaration`
//!   3. changes tool set   → activation goes through the store's single path
//!   4. the gap is visible → a missing secret is named, with a way to fix it
//!
//! Nothing here asserts on derived *lifecycle* state: that is computed by the
//! Runtime and arrives as `state` on each row, so a test that recomputed it
//! would be testing a second implementation that does not exist.

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import i18n from "../../i18n";
import { McpRecommendationGroup } from "./McpRecommendationGroup";
import { useMcpStore } from "../../stores/mcpStore";
import { useLayoutStore } from "../../stores/layoutStore";
import type { McpRecommendationView } from "../../lib/types";

const AGENT = "agent-1";

function rec(over: Partial<McpRecommendationView> = {}): McpRecommendationView {
  return {
    key: "brave-search",
    source: "preset",
    preset: "brave-search",
    tools: ["brave_web_search"],
    required_secrets: ["BRAVE_API_KEY"],
    required: false,
    why: "web research",
    state: "not_installed",
    ...over,
  };
}

interface FetchCall {
  url: string;
  method: string;
  body: unknown;
}

let calls: FetchCall[] = [];
let materializeStatus = 201;
let materializeError: string | null = null;

function stubFetch() {
  calls = [];
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string, init?: RequestInit) => {
      calls.push({
        url,
        method: init?.method ?? "GET",
        body: init?.body ? JSON.parse(String(init.body)) : undefined,
      });
      if (url.includes("/from-declaration")) {
        if (materializeError) {
          return Promise.resolve({
            ok: false,
            status: 400,
            json: () => Promise.resolve({ error: materializeError }),
          });
        }
        return Promise.resolve({
          ok: true,
          status: materializeStatus,
          json: () => Promise.resolve({ operation_id: "op", state: "committed" }),
        });
      }
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({ servers: [] }),
      });
    }),
  );
}

/** Replace the store actions the group calls, so the test observes the
 *  decision (which action, which arguments) rather than the store's HTTP. */
function stubStoreActions() {
  useMcpStore.setState({
    toggleServer: vi.fn(async () => {}),
    putRecommendation: vi.fn(async () => {}),
    loadCatalog: vi.fn(async () => {}),
  });
}

function renderGroup(
  rows: McpRecommendationView[],
  onReload: () => void = () => {},
) {
  return render(
    <McpRecommendationGroup agentId={AGENT} recommendations={rows} onReload={onReload} />,
  );
}

/** The card's own header — the only element in this subtree that publishes
 *  `aria-expanded` (row action buttons have none). */
function cardHeader(): HTMLElement {
  const headers = screen
    .getAllByRole("button")
    .filter((el) => el.hasAttribute("aria-expanded"));
  expect(headers).toHaveLength(1);
  return headers[0];
}

function isCardExpanded(): boolean {
  return cardHeader().getAttribute("aria-expanded") === "true";
}

/** Open the card the way a user does: a collapsed card keeps its rows out of
 *  the DOM, so any assertion about row content has to start here. */
function expandCard() {
  if (!isCardExpanded()) fireEvent.click(cardHeader());
}

describe("McpRecommendationGroup", () => {
  beforeEach(() => {
    materializeStatus = 201;
    materializeError = null;
    stubFetch();
    stubStoreActions();
    useLayoutStore.setState({ harnessTabRequest: null, harnessTabRequestSeq: 0 });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("renders nothing when the package declares no recommendations", () => {
    const { container } = renderGroup([]);
    // An empty "recommended" card would be noise on every ordinary agent.
    expect(container.textContent).toBe("");
  });

  it("shows the author's rationale and the tools that would be gained", () => {
    renderGroup([rec()]);
    expect(screen.getByText("web research")).toBeTruthy();
    expect(
      screen.getByText(i18n.t("agentSetup.mcpRecommendTools", { tools: "brave_web_search" })),
    ).toBeTruthy();
  });

  it("materializes a not_installed declaration through the Gateway", async () => {
    const onReload = vi.fn();
    renderGroup([rec()], onReload);

    fireEvent.click(screen.getByRole("button", { name: i18n.t("agentSetup.mcpRecommendInstall") }));

    await waitFor(() => expect(calls.some((c) => c.url.includes("/from-declaration"))).toBe(true));
    const call = calls.find((c) => c.url.includes("/from-declaration"))!;
    expect(call.method).toBe("POST");
    // The key and the preset both travel: the Gateway rejects a mismatch,
    // because the key is how manifest / agent_mcp.json / catalog join.
    expect(call.body).toMatchObject({
      key: "brave-search",
      preset_id: "brave-search",
      agent_id: AGENT,
    });
    // The catalog is reloaded globally and the panel re-reads state from the
    // Runtime — no optimistic local patch of the lifecycle state.
    expect(useMcpStore.getState().loadCatalog).toHaveBeenCalled();
    await waitFor(() => expect(onReload).toHaveBeenCalled());
  });

  it("sends the inline URL instead of a preset id for an inline declaration", async () => {
    renderGroup([
      rec({
        key: "acme-internal",
        source: "inline",
        preset: undefined,
        url: "https://mcp.acme.internal/rpc",
        transport: "http",
        required_secrets: [],
      }),
    ]);

    fireEvent.click(screen.getByRole("button", { name: i18n.t("agentSetup.mcpRecommendInstall") }));

    await waitFor(() => expect(calls.some((c) => c.url.includes("/from-declaration"))).toBe(true));
    const call = calls.find((c) => c.url.includes("/from-declaration"))!;
    expect(call.body).toMatchObject({
      key: "acme-internal",
      inline: { transport: "http", url: "https://mcp.acme.internal/rpc" },
    });
    expect((call.body as { preset_id?: string }).preset_id).toBeUndefined();
  });

  it("offers no install button for a hosted server the platform publishes", () => {
    renderGroup([rec({ key: "pm", source: "hosted", preset: undefined, hosted: "pm" })]);
    expect(screen.queryByRole("button", { name: i18n.t("agentSetup.mcpRecommendInstall") })).toBeNull();
  });

  it("activates an installed server through the store's single activation path", async () => {
    const onReload = vi.fn();
    renderGroup([rec({ state: "installed_inactive" })], onReload);

    fireEvent.click(screen.getByRole("button", { name: i18n.t("agentSetup.mcpRecommendActivate") }));

    await waitFor(() =>
      expect(useMcpStore.getState().toggleServer).toHaveBeenCalledWith(AGENT, "brave-search"),
    );
    await waitFor(() => expect(onReload).toHaveBeenCalled());
  });

  it("shows an active recommendation as in use with no action", () => {
    renderGroup([rec({ state: "active" })]);
    // Fully handled → the card starts collapsed (see the default-state suite);
    // the row is reachable one click away.
    expandCard();
    expect(screen.getByText(i18n.t("agentSetup.mcpRecommendInUse"))).toBeTruthy();
    expect(screen.queryByRole("button", { name: i18n.t("agentSetup.mcpRecommendInstall") })).toBeNull();
    expect(screen.queryByRole("button", { name: i18n.t("agentSetup.mcpRecommendIgnore") })).toBeNull();
  });

  it("hides dismissed rows behind a disclosure and restores them", async () => {
    renderGroup([rec({ state: "dismissed" })]);
    // A list of nothing but ignored rows is fully handled → collapsed.
    expandCard();

    // Not visible until the disclosure is opened.
    expect(screen.queryByRole("button", { name: i18n.t("agentSetup.mcpRecommendRestore") })).toBeNull();

    fireEvent.click(
      screen.getByText(i18n.t("agentSetup.mcpRecommendedShowIgnored", { count: 1 })),
    );
    fireEvent.click(screen.getByRole("button", { name: i18n.t("agentSetup.mcpRecommendRestore") }));

    await waitFor(() =>
      expect(useMcpStore.getState().putRecommendation).toHaveBeenCalledWith(
        AGENT,
        "brave-search",
        false,
      ),
    );
  });

  it("dismisses a live suggestion", async () => {
    renderGroup([rec()]);
    fireEvent.click(screen.getByRole("button", { name: i18n.t("agentSetup.mcpRecommendIgnore") }));

    await waitFor(() =>
      expect(useMcpStore.getState().putRecommendation).toHaveBeenCalledWith(
        AGENT,
        "brave-search",
        true,
      ),
    );
  });

  it("names the missing secret and routes the user to where it is configured", () => {
    renderGroup([rec()]);
    expect(
      screen.getByText(i18n.t("agentSetup.mcpRecommendNeedsSecrets", { secrets: "BRAVE_API_KEY" })),
    ).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: i18n.t("agentSetup.mcpRecommendConfigure") }));

    // Both halves of the request: the view AND the tab, so the user lands on
    // the MCP tab rather than the harness default.
    expect(useLayoutStore.getState().harnessTabRequest).toBe("mcp");
    expect(useLayoutStore.getState().navViewRequest).toBe("harness");
  });

  it("surfaces a failed install instead of silently doing nothing", async () => {
    materializeError = "unknown MCP preset 'brave-search'";
    renderGroup([rec()]);

    fireEvent.click(screen.getByRole("button", { name: i18n.t("agentSetup.mcpRecommendInstall") }));

    await waitFor(() =>
      expect(
        screen.getByText(
          i18n.t("agentSetup.mcpRecommendActionFailed", {
            error: "unknown MCP preset 'brave-search'",
          }),
        ),
      ).toBeTruthy(),
    );
  });

  it("treats an already-present entry (200) as success", async () => {
    materializeStatus = 200;
    const onReload = vi.fn();
    renderGroup([rec()], onReload);

    fireEvent.click(screen.getByRole("button", { name: i18n.t("agentSetup.mcpRecommendInstall") }));

    await waitFor(() => expect(onReload).toHaveBeenCalled());
    expect(screen.queryByText(/Action failed/)).toBeNull();
  });
});

// ── Default expand state ────────────────────────────────────────────────
//
// ADR-104 §8 ("the recommendation group becoming noise"): the card opens by
// default only while some suggestion still needs a decision, and the header
// keeps the count so a collapsed group stays discoverable. The rule that maps
// lifecycle states to "handled" lives in `lib/mcpRecommendations.ts` and has
// its own suite — what is asserted here is the wiring: WHEN the default is
// decided, and that the user's own toggle is never re-decided.
describe("McpRecommendationGroup — default expand state", () => {
  beforeEach(() => {
    materializeStatus = 201;
    materializeError = null;
    stubFetch();
    stubStoreActions();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("starts expanded while a suggestion is still installable", () => {
    renderGroup([rec({ state: "not_installed" })]);
    expect(isCardExpanded()).toBe(true);
  });

  it("starts expanded while an installed suggestion is not started yet", () => {
    // `installed_inactive` is still one click away from done — the user has to
    // see it without hunting for the card.
    renderGroup([rec({ state: "installed_inactive" })]);
    expect(isCardExpanded()).toBe(true);
  });

  it("starts expanded when only one of several suggestions is unhandled", () => {
    renderGroup([
      rec({ key: "a", state: "active" }),
      rec({ key: "b", state: "dismissed" }),
      rec({ key: "c", state: "installed_inactive" }),
    ]);
    expect(isCardExpanded()).toBe(true);
  });

  it("starts collapsed once every suggestion is active or ignored", () => {
    renderGroup([
      rec({ key: "a", state: "active" }),
      rec({ key: "b", state: "dismissed" }),
    ]);
    // Collapsed, not hidden: the header still reports the declaration count.
    expect(isCardExpanded()).toBe(false);
    expect(
      cardHeader().textContent,
    ).toContain(i18n.t("agentSetup.mcpRecommended", { count: 2 }));
  });

  it("keeps the user's collapse when a reload re-delivers the same unhandled list", () => {
    // The default for this list is "expanded"; the user closed it anyway, and
    // the reload after any row action must not pop it back open.
    const { rerender } = renderGroup([rec({ state: "installed_inactive" })]);
    expect(isCardExpanded()).toBe(true);
    fireEvent.click(cardHeader());
    expect(isCardExpanded()).toBe(false);

    rerender(
      <McpRecommendationGroup
        agentId={AGENT}
        recommendations={[rec({ state: "installed_inactive" })]}
        onReload={() => {}}
      />,
    );
    expect(isCardExpanded()).toBe(false);
  });

  it("stays open while the user works through the list", () => {
    // Activating the last pending row flips the list to "all handled"; the
    // default for that list is collapsed, but the card must not slam shut under
    // the cursor that just clicked it.
    const { rerender } = renderGroup([rec({ state: "not_installed" })]);
    expect(isCardExpanded()).toBe(true);

    rerender(
      <McpRecommendationGroup
        agentId={AGENT}
        recommendations={[rec({ state: "active" })]}
        onReload={() => {}}
      />,
    );
    expect(isCardExpanded()).toBe(true);
  });

  it("decides again for a different agent instead of reusing the previous answer", () => {
    // The host empties the list while the next agent's `/tools` is in flight
    // (see ToolsCards: the list is stamped with its agent), so the card is
    // unmounted in between and its successor decides from its own rows.
    const { rerender } = renderGroup([rec({ state: "not_installed" })]);
    expect(isCardExpanded()).toBe(true);

    rerender(<McpRecommendationGroup agentId="agent-2" recommendations={[]} onReload={() => {}} />);
    expect(screen.queryAllByRole("button")).toHaveLength(0);

    rerender(
      <McpRecommendationGroup
        agentId="agent-2"
        recommendations={[rec({ state: "active" })]}
        onReload={() => {}}
      />,
    );
    expect(isCardExpanded()).toBe(false);
  });
});
