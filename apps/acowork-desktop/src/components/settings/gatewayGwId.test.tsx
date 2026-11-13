/**
 * The Gateway's relay device id must be reachable from the relay section.
 *
 * Why this file exists:
 *   The gw-id is what a user needs BEFORE they can fill in a relay address
 *   — the address IS `https://<gw-id>.<relay-domain>`. It used to render
 *   only inside `RelayTunnelPanel`, which was gated behind
 *   `gatewayMode === "relay"`, and that panel polls the configured Gateway
 *   URL. So the only place the id appeared was unreachable at the exact
 *   moment it was needed: relay mode rejects a non-https address, and the
 *   https address does not resolve until you have the id.
 *
 * What this pins:
 *   1. The connection card is radio-sectioned (直连 / 中继 — transport
 *      kind, NOT local vs remote: localhost and LAN addresses share one
 *      list in the direct section). The radio is a UI section picker,
 *      never a mode write (ADR-115: the mode stays derived from the
 *      committed address) — so opening the relay section from ANY topology
 *      must not touch the store. The gw-id row renders in that section for
 *      a loopback address, a LAN address and a relay device domain alike.
 *   2. It reports the id the Gateway returned, and an em-dash when the
 *      Gateway has none (tunnel never enabled).
 *   3. The help trigger shows the stock `Tooltip` on hover — not an inline
 *      expander, not a click popup, not a toast — with content that depends
 *      on whether the Gateway has an id, and it never reflows the row.
 *   4. The dedicated relay entry validates the TLS-only scheme INLINE
 *      (`role="alert"` + disabled Connect) rather than committing a
 *      `relay + http://` pair that Rust's `relay_mqtt_wss_url` would reject
 *      forever while `/health` still answered.
 *
 * Why rendered DOM rather than source scanning:
 *   The regression is a conditional-render bug. `capsule.test.tsx` scans
 *   source because its subjects cannot mount (Tauri / MQTT / monaco
 *   stores); `GatewayTab` renders under jsdom, so assert on what a user
 *   would actually see.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useSettingsStore } from "../../stores/settingsStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { GatewayTab } from "./SettingsPage";

const GW_ID = "0f1e2d3c-4b5a-4678-9abc-def012345678";
const LOOPBACK_URL = "http://127.0.0.1:19876";
const LAN_URL = "http://10.0.0.5:19876";
const RELAY_URL = `https://${GW_ID}.relay.example.com`;

/** The row is labelled by the i18n string "Device ID" (en.json
 *  `settings.relayGwId`), so tests match the rendered label rather than a
 *  testid the production code has no reason to carry. */
const deviceIdRow = () => screen.getByText(/^Device ID:/);

/** The connection-kind radio group (`connection.viaRelay` → "Relay"). */
const relayRadio = () => screen.getByRole("radio", { name: "Relay" });

/** The gw-id row and the relay entry live in the relay SECTION of the
 *  connection card. Opening the section is a pure UI action — it must not
 *  write the store (the mode stays address-derived). */
const openRelaySection = () => fireEvent.click(relayRadio());

/** The relay entry's field — labelled by `connection.relayEntry`. */
const relayInput = () =>
  screen.getByLabelText(/connect via relay|connect via relay/i) as HTMLInputElement;

/** The relay entry's commit button (shares the `connection.connect` label
 *  with the address picker's own button, so scope by the relay row). */
const relayConnectButton = () => {
  const btn = relayInput().parentElement!.querySelector("button");
  if (!btn) throw new Error("relay Connect button not found");
  return btn as HTMLButtonElement;
};

beforeEach(() => {
  useSettingsStore.setState({
    gatewayMode: "local",
    relayActive: false,
    gatewayUrl: LOOPBACK_URL,
  });
  useGatewayStore.setState({ status: "connected" });
});

afterEach(async () => {
  // `GatewayTab` fires `fetchAll` on mount, whose node-fetch `finally` calls
  // `setNodesLoading(false)`. Tests that assert synchronously exit before
  // that promise settles, so it lands *after* the environment is torn down
  // and surfaces as an unhandled rejection ("window is not defined" from
  // React trying to update an unmounted tree). Drain the microtask queue
  // before cleanup so every test — not just the ones that happen to await —
  // lets the in-flight fetches finish against a live environment.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
  cleanup();
  vi.restoreAllMocks();
});

function mockRelayStatus(gwId: string | null) {
  return vi.spyOn(globalThis, "fetch").mockImplementation(async (input) => {
    const url = String(input);
    if (url.endsWith("/api/relay/status")) {
      return new Response(
        JSON.stringify({
          enabled: gwId !== null,
          relay_url: null,
          gw_id: gwId,
          connected: false,
          session_id: null,
          last_error: null,
          connected_at: null,
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    }
    // Everything else the tab pulls (agents, nodes, health) plus the address
    // picker's probes — empty.
    return new Response("[]", {
      status: 200,
      headers: { "Content-Type": "application/json" } });
  });
}

describe("gw-id visibility", () => {
  /** ADR-115: passed as an ADDRESS, not a mode flag. */
  const topologies = [
    ["a loopback address", LOOPBACK_URL, false],
    ["a LAN address", LAN_URL, false],
    ["a relay device domain", RELAY_URL, true],
  ] as const;

  it.each(topologies)("shows the row for %s", async (_label, url, relayActive) => {
    mockRelayStatus(GW_ID);
    useSettingsStore.setState({ gatewayUrl: url, relayActive });
    render(<GatewayTab />);

    // The regression: this row did not exist outside relay mode at all.
    // Now the relay section opens from any topology.
    openRelaySection();
    await waitFor(() => expect(deviceIdRow()).toBeTruthy());
    expect(deviceIdRow().textContent).toContain(GW_ID);
    // Opening the section is pure UI — the derived mode is untouched
    // (ADR-115: the radio never writes the mode).
    expect(useSettingsStore.getState().gatewayUrl).toBe(url);
  });

  it("shows an em-dash when the Gateway has no device id", async () => {
    mockRelayStatus(null);
    render(<GatewayTab />);
    openRelaySection();

    await waitFor(() => expect(deviceIdRow().textContent).toContain("—"));
    expect(deviceIdRow().textContent).not.toContain(GW_ID);
  });

  it("keeps the row when the Gateway is unreachable", async () => {
    vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("ECONNREFUSED"));
    render(<GatewayTab />);
    openRelaySection();

    // The row is part of the relay section's always-on chrome, not of the
    // connection status — it must not disappear because a fetch failed.
    await waitFor(() => expect(deviceIdRow()).toBeTruthy());
  });
});

describe("relay entry validates the TLS-only scheme inline", () => {
  const HINT = /must be the https:\/\/ device domain/i;

  it("rejects a plain-http address with an inline alert and a disabled Connect", async () => {
    mockRelayStatus(GW_ID);
    render(<GatewayTab />);
    openRelaySection();
    await waitFor(() => expect(deviceIdRow()).toBeTruthy());

    // Nothing to complain about before the user types.
    expect(screen.queryByRole("alert")).toBeNull();
    expect(relayConnectButton().disabled).toBe(true);

    fireEvent.change(relayInput(), { target: { value: "http://gw.example.com:19876" } });

    expect(screen.getByRole("alert").textContent).toMatch(HINT);
    expect(relayConnectButton().disabled).toBe(true);
    // And the illegal pair was never committed.
    expect(useSettingsStore.getState().relayActive).toBe(false);
  });

  it("accepts an https device domain: no alert, Connect enabled", async () => {
    mockRelayStatus(GW_ID);
    render(<GatewayTab />);
    openRelaySection();
    await waitFor(() => expect(deviceIdRow()).toBeTruthy());

    fireEvent.change(relayInput(), { target: { value: RELAY_URL } });

    expect(screen.queryByRole("alert")).toBeNull();
    expect(relayConnectButton().disabled).toBe(false);
  });

  it("commits relay mode only through this entry", async () => {
    mockRelayStatus(GW_ID);
    render(<GatewayTab />);
    openRelaySection();
    await waitFor(() => expect(deviceIdRow()).toBeTruthy());

    fireEvent.change(relayInput(), { target: { value: RELAY_URL } });
    await act(async () => {
      fireEvent.click(relayConnectButton());
      await Promise.resolve();
    });

    const s = useSettingsStore.getState();
    expect(s.relayActive).toBe(true);
    expect(s.gatewayMode).toBe("relay");
    expect(s.gatewayUrl).toBe(RELAY_URL);
  });
});

describe("gw-id help", () => {
  beforeEach(() => { vi.useFakeTimers({ shouldAdvanceTime: true }); });
  afterEach(() => { vi.useRealTimers(); });

  /** The stock `Tooltip` on a hover trigger — the same hint surface every
   *  other hint in the app uses. Not a click popup, not a toast, and never
   *  inline: the row's own DOM must not change shape when help opens (the
   *  inline-expander version reflowed the two label/value lines above it). */
  // `Tooltip` has a 400ms show delay by default, so fake timers drive the
  // reveal instead of a real wait.
  const triggerEl = () =>
    screen.getByRole("button", { name: /what is the device id for/i }).parentElement!;

  const hoverHelp = () => {
    fireEvent.mouseEnter(triggerEl());
    act(() => { vi.advanceTimersByTime(500); });
  };

  const unhoverHelp = () => {
    fireEvent.mouseLeave(triggerEl());
    act(() => { vi.advanceTimersByTime(500); });
  };

  /** `Tooltip` renders through `createPortal` to `document.body`, so it is
   *  not inside the settings card. `.fixed` is the portal wrapper; the tip
   *  itself is the child. */
  const tip = (needle: RegExp) => screen.getByText(needle);

  it("explains the pairing address when an id exists", async () => {
    mockRelayStatus(GW_ID);
    render(<GatewayTab />);
    openRelaySection();
    await waitFor(() => expect(deviceIdRow()).toBeTruthy());

    const rowBefore = deviceIdRow().innerHTML;
    hoverHelp();

    const text = tip(/Remote-access device id/).textContent ?? "";
    expect(text).toContain(`https://${GW_ID}.<relay-domain>`);
    // No "how to create" instructions — there is nothing to create.
    expect(text).not.toContain("relay_identity.json");
    // The row's own content is untouched — help never reflows the card.
    expect(deviceIdRow().innerHTML).toBe(rowBefore);
  });

  it("explains how to get an id when the Gateway has none", async () => {
    mockRelayStatus(null);
    render(<GatewayTab />);
    openRelaySection();
    await waitFor(() => expect(deviceIdRow().textContent).toContain("—"));

    hoverHelp();

    const text = tip(/No device id yet/).textContent ?? "";
    // The hint points at the config-file way of enabling the tunnel —
    // that is the path the README documents, and the one an operator
    // can actually take without hand-crafting a request body.
    expect(text).toContain("gateway.toml");
    expect(text).toContain("[relay]");
    expect(text).toContain("relay_identity.json");
  });

  it("is a hover hint, not a click popup", async () => {
    mockRelayStatus(GW_ID);
    render(<GatewayTab />);
    openRelaySection();
    await waitFor(() => expect(deviceIdRow()).toBeTruthy());

    // A click alone must NOT open it — this is what separates the stock
    // tooltip from the hand-rolled click popup it replaced.
    fireEvent.click(screen.getByRole("button", { name: /what is the device id for/i }));
    expect(screen.queryByText(/Remote-access device id/)).toBeNull();

    hoverHelp();
    expect(tip(/Remote-access device id/)).toBeTruthy();
  });

  it("hides again on mouse leave", async () => {
    mockRelayStatus(GW_ID);
    render(<GatewayTab />);
    openRelaySection();
    await waitFor(() => expect(deviceIdRow()).toBeTruthy());

    hoverHelp();
    expect(tip(/Remote-access device id/)).toBeTruthy();

    unhoverHelp();
    expect(screen.queryByText(/Remote-access device id/)).toBeNull();
  });
});
