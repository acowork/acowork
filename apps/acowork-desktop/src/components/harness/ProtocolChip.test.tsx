//! ADR-103 D11 position 1 — the `Ⓡ Responses` chip must never claim more than
//! the Gateway decided.
//!
//! Three tiers plus the yellow "selected but not enabled" mark. The red line is
//! that `R` means *usable*: a blocked selection MUST NOT render the glyph, and a
//! merely-declared one (offline data claims support, never probed) must read as
//! muted rather than confident. These tests pin the glyph, the tone class and
//! the degraded dot — the three things the eye actually uses to tell them apart.

import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import i18n from "../../i18n";
import { ProtocolChip } from "./ProtocolChip";
import type { ProtocolBadge } from "../../lib/modelProtocol";

function renderBadge(badge: ProtocolBadge): HTMLElement {
  const { container } = render(<ProtocolChip badge={badge} />);
  const span = container.firstElementChild as HTMLElement | null;
  if (!span) throw new Error("ProtocolChip rendered nothing");
  return span;
}

describe("ProtocolChip", () => {
  it("renders the solid Ⓡ chip for a verified model", () => {
    const chip = renderBadge({ kind: "verified" });
    expect(chip.textContent).toContain("Ⓡ");
    expect(chip.textContent).toContain(i18n.t("harness.shapeResponses"));
    expect(chip.className).toContain("text-text-secondary");
    expect(chip.getAttribute("title")).toBe(i18n.t("harness.probeVerifiedTip"));
  });

  it("adds the yellow dot for a degraded model but keeps the glyph", () => {
    const chip = renderBadge({ kind: "degraded" });
    expect(chip.textContent).toContain("Ⓡ");
    // The dot is the only visual difference from `verified`.
    expect(chip.querySelector(".bg-amber-500")).not.toBeNull();
    expect(chip.getAttribute("title")).toBe(i18n.t("harness.probeDegradedTip"));
  });

  it("mutes a merely-declared model", () => {
    const chip = renderBadge({ kind: "declared" });
    expect(chip.textContent).toContain("Ⓡ");
    expect(chip.className).toContain("text-text-tertiary");
    expect(chip.querySelector(".bg-amber-500")).toBeNull();
    expect(chip.getAttribute("title")).toBe(i18n.t("harness.probeDeclaredTip"));
  });

  it("renders the yellow blocked mark without the R glyph", () => {
    const chip = renderBadge({ kind: "blocked", reason: "failed", detail: "404" });
    expect(chip.className).toContain("text-amber-500");
    // `R` means usable — a blocked selection must not show it.
    expect(chip.textContent).not.toContain("Ⓡ");
    expect(chip.textContent).toBe(i18n.t("harness.probeBlockedShort"));
    expect(chip.getAttribute("title")).toBe(i18n.t("harness.probeBlockedReason_failed"));
  });

  it("renders nothing when there is nothing to say", () => {
    const { container } = render(<ProtocolChip badge={{ kind: "none" }} />);
    expect(container.firstElementChild).toBeNull();
  });

  it("never renders a legend row — the tier is legible from the chip alone", () => {
    // Guards the D11 "no legend required" rule: the chip carries no extra text
    // beyond the glyph, the protocol name and (for degraded) the dot.
    renderBadge({ kind: "verified" });
    expect(screen.queryByText(/legend/i)).toBeNull();
  });
});