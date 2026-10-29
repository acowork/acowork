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
    expect(screen.getByText(i18n.t("agentSetup.mcpRecommendInUse"))).toBeTruthy();
    expect(screen.queryByRole("button", { name: i18n.t("agentSetup.mcpRecommendInstall") })).toBeNull();
    expect(screen.queryByRole("button", { name: i18n.t("agentSetup.mcpRecommendIgnore") })).toBeNull();
  });

  it("hides dismissed rows behind a disclosure and restores them", async () => {
    renderGroup([rec({ state: "dismissed" })]);

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