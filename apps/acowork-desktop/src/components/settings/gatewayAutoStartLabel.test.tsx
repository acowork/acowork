/**
 * The auto-start-Node toggle must label itself at the card's CONTENT scale,
 * not at the component-internal micro scale.
 *
 * What went wrong: the row is hand-rolled (it needs a HelpHint `<button>`,
 * which must not sit inside `<Switch>`'s own `<label>` — clicking the hint
 * would also flip the switch), and the hand-rolled version copied Switch's
 * internal label classes verbatim: `text-10 font-medium text-text-tertiary`.
 * Those are a *component default* for dense inline labels, not a
 * caller-facing token. At the default `--ui-font-size` that is 10px while
 * every other label in the same card — the mode radios, `Gateway URL:`,
 * `Device ID:`, `Status` — is `text-xs` (12px), so the toggle read ~17%
 * smaller than its neighbours, and `text-text-tertiary` dimmed a control
 * label down to the meta-line contrast used for read-only values.
 *
 * What this pins, derived rather than hardcoded where possible:
 *   1. The label's text step EQUALS the mode radios' text step (they are
 *      label peers in one card, so they must move together if the card's
 *      step ever changes) and is not one of the micro steps.
 *   2. The label carries no explicit text color, so it inherits
 *      `--color-text` exactly like the radios do.
 *   3. The `?` hint is still inside the row (the reason the layout is
 *      hand-rolled in the first place).
 *
 * Rendered DOM rather than source scanning: `GatewayTab` mounts under
 * jsdom, and the assertion is about what a user sees.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { useSettingsStore } from "../../stores/settingsStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { GatewayTab } from "./SettingsPage";

/** The row exists in remote mode only (local mode auto-spawns the node,
 *  relay is not validated for this path yet). */
const autoStartLabel = () => screen.getByText("Auto-start local Node on launch");

/** A label peer in the same card body, used as the reference scale. */
const remoteModeLabel = () => screen.getByText("Remote (LAN)");

/** Every `text-*` class token that sets a FONT SIZE (as opposed to a
 *  color, which is spelled `text-text-*`). Arbitrary px escapes
 *  (`text-[10px]`) count too — those are the frozen-size regressions the
 *  `--text-*` tokens exist to prevent. */
const SIZE_TOKEN = /^text-(9|10|11|xs|sm|base|lg|xl|2xl|\[.*\])$/;

function sizeTokens(el: Element): string[] {
  return (el.getAttribute("class") ?? "").split(/\s+/).filter((c) => SIZE_TOKEN.test(c));
}

function colorTokens(el: Element): string[] {
  return (el.getAttribute("class") ?? "").split(/\s+/).filter((c) => /^text-text-/.test(c));
}

beforeEach(() => {
  useSettingsStore.setState({ gatewayMode: "remote", gatewayUrl: "http://127.0.0.1:19876" });
  useGatewayStore.setState({ status: "connected" });
  // Everything `fetchAll` pulls (relay status, agents, nodes, health): empty.
  vi.spyOn(globalThis, "fetch").mockImplementation(
    async () => new Response("[]", { status: 200, headers: { "Content-Type": "application/json" } }),
  );
});

afterEach(async () => {
  // `fetchAll` settles its `setState` calls after some tests finish; drain
  // them before teardown or React updates an unmounted tree.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
  cleanup();
  vi.restoreAllMocks();
});

describe("auto-start-Node toggle label typography", () => {
  it("uses the same text step as the mode radios, not a micro step", async () => {
    render(<GatewayTab />);
    await waitFor(() => expect(autoStartLabel()).toBeTruthy());

    expect(sizeTokens(autoStartLabel())).toEqual(sizeTokens(remoteModeLabel()));
    expect(sizeTokens(autoStartLabel())).toEqual(["text-xs"]);
  });

  it("does not override the inherited text color", async () => {
    render(<GatewayTab />);
    await waitFor(() => expect(autoStartLabel()).toBeTruthy());

    // Radio labels carry no color class either — both inherit `--color-text`,
    // so the control label reads at control contrast, not meta contrast.
    expect(colorTokens(remoteModeLabel())).toEqual([]);
    expect(colorTokens(autoStartLabel())).toEqual([]);
  });

  it("keeps the help hint inside the row", async () => {
    render(<GatewayTab />);
    await waitFor(() => expect(autoStartLabel()).toBeTruthy());

    expect(
      screen.getByRole("button", { name: /resume this machine's enrolled Node/i }),
    ).toBeTruthy();
  });
});
