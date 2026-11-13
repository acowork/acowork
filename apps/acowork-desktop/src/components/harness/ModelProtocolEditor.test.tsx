//! ADR-103 D11 position 3+4 — the per-model protocol editor.
//!
//! The D11 red line lives here: `Responses` is offered for **every** model,
//! whether or not the offline data tagged it (DeepSeek / MiniMax / custom
//! gateways are covered by no dataset, so hiding the option would hide the only
//! path to it). The second guard is that a failed selection reads as "selected
//! but not enabled" with a reason, never as a usable protocol.

import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import i18n from "../../i18n";
import { ModelProtocolEditor } from "./ModelProtocolEditor";

function renderEditor(overrides: Partial<Parameters<typeof ModelProtocolEditor>[0]> = {}) {
  const props = {
    modelId: "m1",
    selection: "follow" as const,
    onSelectionChange: vi.fn(),
    baseUrl: "",
    onBaseUrlChange: vi.fn(),
    showBaseUrl: false,
    showProtocolDropdown: true,
    badge: { kind: "none" } as const,
    verifying: false,
    onVerify: vi.fn(),
    canVerify: true,
    ...overrides,
  };
  const utils = render(<ModelProtocolEditor {...props} />);
  return { ...utils, props };
}

describe("ModelProtocolEditor", () => {
  it("labels the dropdown and shows the current selection", () => {
    renderEditor({ selection: "responses" });
    expect(screen.getByText(i18n.t("harness.protocol"))).toBeTruthy();
    // The themed Dropdown shows the current value in its trigger, while the
    // hidden native <select> still carries the same text in its <option> —
    // query the visible control rather than a bare text match (which is now
    // ambiguous by design).
    expect(
      screen.getByRole("button", { name: i18n.t("harness.shapeResponses") }),
    ).toBeTruthy();
  });

  it("hides the base_url input unless Responses is selected", () => {
    const { unmount } = renderEditor({ showBaseUrl: false });
    expect(screen.queryByText(i18n.t("harness.protocolBaseUrl"))).toBeNull();
    unmount();
    renderEditor({ showBaseUrl: true });
    expect(screen.getByText(i18n.t("harness.protocolBaseUrl"))).toBeTruthy();
  });

  it("disables re-verify until the provider is saved", () => {
    renderEditor({ canVerify: false });
    const button = screen.getByRole("button", { name: i18n.t("harness.probeButton") });
    expect((button as HTMLButtonElement).disabled).toBe(true);
    expect(button.getAttribute("title")).toBe(i18n.t("harness.probeNeedsSaveFirst"));
  });

  it("shows the running label while a probe is in flight", () => {
    renderEditor({ verifying: true });
    expect(screen.getByRole("button", { name: i18n.t("harness.probeRunning") })).toBeTruthy();
  });

  it("explains a blocked selection with its reason and detail", () => {
    renderEditor({ badge: { kind: "blocked", reason: "failed", detail: "HTTP 401" } });
    expect(screen.getByText(new RegExp(i18n.t("harness.probeBlockedReason_failed")))).toBeTruthy();
    expect(screen.getByText(/HTTP 401/)).toBeTruthy();
  });

  it("distinguishes degraded from verified in the status area", () => {
    const { unmount } = renderEditor({
      badge: { kind: "degraded", tiers: ["L1", "L2"], stripped: [] },
    });
    expect(screen.getByText(/L1 \/ L2/)).toBeTruthy();
    unmount();
    renderEditor({ badge: { kind: "verified" } });
    expect(screen.getByText(i18n.t("harness.probeVerified"))).toBeTruthy();
  });

  it("spells out the dropped field when the verdict is degraded", () => {
    renderEditor({ badge: { kind: "degraded", stripped: ["reasoning"] } });
    expect(
      screen.getByText(
        i18n.t("harness.probeDegradedFields", { fields: "reasoning" }),
      ),
    ).toBeTruthy();
  });

  it("fires the re-verify action", () => {
    const { props } = renderEditor();
    fireEvent.click(screen.getByRole("button", { name: i18n.t("harness.probeButton") }));
    expect(props.onVerify).toHaveBeenCalledTimes(1);
  });

  // ADR-103 D14: a single-protocol provider has nothing to choose between,
  // so the dropdown is replaced by a read-only statement.
  it("hides the dropdown for a single-protocol provider", () => {
    renderEditor({ showProtocolDropdown: false });
    expect(screen.queryByText(i18n.t("harness.shapeResponses"))).toBeNull();
    expect(screen.getByText(i18n.t("harness.protocolSingleProvider"))).toBeTruthy();
    // The re-verify action stays — the provider-level protocol (Anthropic)
    // has a probe shape of its own. Which protocol to probe is decided by the
    // caller (`ModelMultiSelect`), not here.
    expect(screen.getByRole("button", { name: i18n.t("harness.probeButton") })).toBeTruthy();
  });

  it("keeps the dropdown when the provider protocol is unknown", () => {
    renderEditor({ showProtocolDropdown: true });
    expect(screen.getByText(i18n.t("harness.shapeResponses"))).toBeTruthy();
  });

  it("offers the Anthropic Messages option", () => {
    // ADR-103 D14: the probe speaks the Messages shape, so the UI can select
    // it — an untestable claim would leave the new probe line unreachable.
    renderEditor();
    expect(screen.getByText(i18n.t("harness.protocolAnthropicMessages"))).toBeTruthy();
  });
});