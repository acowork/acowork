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
    expect(screen.getByText(i18n.t("harness.shapeResponses"))).toBeTruthy();
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
    renderEditor({ badge: { kind: "blocked", reason: "gate_off", detail: "responses_api_enabled=false" } });
    expect(screen.getByText(new RegExp(i18n.t("harness.probeBlockedReason_gate_off")))).toBeTruthy();
    expect(screen.getByText(/responses_api_enabled=false/)).toBeTruthy();
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
});