//! The MCP card's first-time MCP install progress surface (Setup panel).
//!
//! Regression cover for the case that made the feature useless: a server being
//! installed for the first time has **no tool list yet**, so its row is never
//! expandable (`ExpandableRow` renders `children` only while expanded). The
//! log dialog was originally placed inside that body, so clicking the spinner
//! set state but nothing appeared — exactly when the user needed it.
//!
//! These tests therefore mount the card in the real first-install shape
//! (`tools: []`, `ensure.state: "installing"`) and assert the dialog renders
//! through a portal, shows the captured lines, and closes on Escape.

import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import i18n from "../../i18n";
import { EnsureLogsDialog, McpServerCard } from "./ToolsCards";
import type { McpEnsureStatus } from "../../stores/mcpStore";
import type { McpServerView } from "../../lib/types";

const SERVER: McpServerView = {
  name: "playwright",
  transport: "stdio",
  command: "npx",
} as McpServerView;

function ensure(over: Partial<McpEnsureStatus> = {}): McpEnsureStatus {
  return { state: "installing", ...over };
}

function renderCard(over: Partial<Parameters<typeof McpServerCard>[0]> = {}) {
  return render(
    <McpServerCard
      server={SERVER}
      isChecked
      tools={[]}
      loading={false}
      onToggleTool={() => {}}
      onToggleServer={() => {}}
      switchDisabled={false}
      toolSwitchDisabled={false}
      ensure={ensure()}
      onRetry={() => {}}
      {...over}
    />,
  );
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("MCP install progress", () => {
  it("opens the log dialog while the server has no tool list yet", () => {
    renderCard();

    expect(screen.queryByTestId("mcp-ensure-logs-dialog")).toBeNull();

    fireEvent.click(screen.getByTestId("mcp-ensure-playwright"));

    expect(screen.getByTestId("mcp-ensure-logs-dialog")).toBeTruthy();
  });

  it("renders every captured line, not just the last one", () => {
    renderCard({
      ensure: ensure({
        logs: ["[progress] Checking MCP server availability", "[stdout] npm http fetch", "[stderr] warn"],
      }),
    });

    fireEvent.click(screen.getByTestId("mcp-ensure-playwright"));

    const text = screen.getByTestId("mcp-ensure-logs-text").textContent ?? "";
    expect(text).toContain("Checking MCP server availability");
    expect(text).toContain("npm http fetch");
    expect(text).toContain("warn");
  });

  it("offers an entry point while the pool is still connecting (no ensure yet)", () => {
    // The regression reported on 2026-11-02: a user-added `npx` server had no
    // ensure attempt at all, so the card fell back to a bare `loading` spinner
    // — a spinner with nothing to click. Every waiting state must open the
    // dialog.
    renderCard({ ensure: undefined, loading: true });

    fireEvent.click(screen.getByTestId("mcp-ensure-playwright"));

    expect(screen.getByTestId("mcp-ensure-logs-dialog")).toBeTruthy();
    // …and it is honest about why the log is empty.
    expect(screen.getByText(i18n.t("agentSetup.ensureLogsHint"))).toBeTruthy();
  });

  it("opens the dialog from a failed install to show why it failed", () => {
    renderCard({ ensure: { state: "failed", error: "install command exited with code 1" } });

    fireEvent.click(screen.getByTestId("mcp-ensure-playwright"));

    expect(screen.getByTestId("mcp-ensure-logs-dialog")).toBeTruthy();
  });

  it("does not open the dialog from the spinner while the server is usable", () => {
    renderCard({ ensure: { state: "usable" } });

    expect(screen.queryByTestId("mcp-ensure-playwright")).toBeNull();
    expect(screen.queryByTestId("mcp-ensure-logs-dialog")).toBeNull();
  });

  it("closes on Escape", () => {
    const onClose = vi.fn();
    render(<EnsureLogsDialog serverName="playwright" logs={[]} onClose={onClose} />);

    fireEvent.keyDown(window, { key: "Escape" });

    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("shows the waiting placeholder before the first line arrives", () => {
    render(<EnsureLogsDialog serverName="playwright" logs={[]} onClose={() => {}} />);

    expect(screen.getByTestId("mcp-ensure-logs-text").textContent).toBe(
      i18n.t("agentSetup.ensureLogsWaiting"),
    );
  });
});

// ── Connect state (2026-11-26) ────────────────────────────────────────
//
// A server whose connect failed keeps `tools: []` (reconcile is against the
// live `tools/list`), which used to render as a healthy enabled switch with an
// empty body — no error, no action. The Runtime now reports each server's live
// connect state, and the card has to state it and offer the reconnect.
describe("MCP connect state", () => {
  it("names the failure and offers a retry", () => {
    const onRetry = vi.fn();
    renderCard({
      ensure: undefined,
      status: { state: "failed", error: "tcp connect error: os error 10051" },
      onRetry,
    });

    expect(screen.getByText(/os error 10051/)).toBeTruthy();

    fireEvent.click(screen.getByTestId("mcp-retry-playwright"));

    expect(onRetry).toHaveBeenCalledTimes(1);
  });

  it("shows a spinner, not a retry, while the Runtime is connecting", () => {
    renderCard({ ensure: undefined, status: { state: "connecting" } });

    expect(screen.getByTestId("mcp-connecting-playwright")).toBeTruthy();
    expect(screen.queryByTestId("mcp-retry-playwright")).toBeNull();
  });

  it("offers a neutral retry when the server connects with no tools", () => {
    // Connected, exposes nothing: not a failure, and not healthy either — the
    // user still needs a way to re-check.
    renderCard({ ensure: undefined });

    expect(screen.getByTestId("mcp-retry-playwright")).toBeTruthy();
    expect(screen.queryByText(/os error/)).toBeNull();
  });

  it("offers no retry for a server that is switched off", () => {
    renderCard({ ensure: undefined, isChecked: false });

    expect(screen.queryByTestId("mcp-retry-playwright")).toBeNull();
  });

  it("offers no retry while the reconcile read is still in flight", () => {
    renderCard({ ensure: undefined, loading: true });

    expect(screen.queryByTestId("mcp-retry-playwright")).toBeNull();
  });
});
