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
 * every other label in the same section — the connection-kind radios,
 * `Server address:` — is `text-xs` (12px), so the toggle read
 * ~17% smaller than its neighbours, and `text-text-tertiary` dimmed a
 * control label down to the meta-line contrast used for read-only values.
 *
 * What this pins, derived rather than hardcoded where possible:
 *   1. The label's text step EQUALS the relay-entry label's text step (they
 *      are label peers in one card, so they must move together if the card's
 *      step ever changes) and is not one of the micro steps.
 *   2. The label carries no explicit text color, so it inherits
 *      `--color-text` — unlike the two read-only value lines in the same card
 *      (`Device ID:`, the address hint), which are meta lines and stay dim.
 *   3. The `?` hint is still inside the row (the reason the layout is
 *      hand-rolled in the first place).
 *
 * Note on the reference peer: the card used to hold a `Remote (LAN)` radio
 * whose label carried the "control contrast, no color class" pairing. ADR-115
 * removed the mode radios (the mode is derived from the address); the
 * connection card is now radio-sectioned, and the toggle lives in the
 * "remote" section next to the `Server address:` label of the shared
 * `GatewayAddressPicker` — that label is the size peer, and the "no color
 * class" half is asserted directly instead of by comparison.
 *
 * Rendered DOM rather than source scanning: `GatewayTab` mounts under
 * jsdom, and the assertion is about what a user sees.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { useSettingsStore } from "../../stores/settingsStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { GatewayTab } from "./SettingsPage";

/** The row exists for a non-loopback address only (a loopback address
 *  auto-spawns the node, and relay is not validated for this path yet). */
const autoStartLabel = () => screen.getByText("Auto-start local Node on launch");

/** A label peer in the same section body (the address picker's field
 *  label), used as the reference scale. */
const serverAddressLabel = () => screen.getByText("Server address");

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
  // Non-loopback address → derived mode is `remote`, which is the only mode
  // that shows the auto-start-Node row.
  useSettingsStore.setState({
    gatewayMode: "remote",
    relayActive: false,
    gatewayUrl: "http://10.0.0.5:19876",
  });
  useGatewayStore.setState({ status: "connected" });
  // Everything `fetchAll` pulls (relay status, agents, nodes, health) plus
  // the address picker's probes: empty.
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
  it("uses the same text step as the server-address label, not a micro step", async () => {
    render(<GatewayTab />);
    await waitFor(() => expect(autoStartLabel()).toBeTruthy());

    expect(sizeTokens(autoStartLabel())).toEqual(sizeTokens(serverAddressLabel()));
    expect(sizeTokens(autoStartLabel())).toEqual(["text-xs"]);
  });

  it("does not override the inherited text color", async () => {
    render(<GatewayTab />);
    await waitFor(() => expect(autoStartLabel()).toBeTruthy());

    // A control label inherits `--color-text`; only read-only meta lines
    // (`Device ID:`, the address hint) go tertiary.
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
