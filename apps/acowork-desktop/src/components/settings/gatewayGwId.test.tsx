/**
 * The Gateway's relay device id must be visible in EVERY connection mode.
 *
 * Why this file exists:
 *   The gw-id is what a user needs BEFORE they can fill in a relay address
 *   — the address IS `https://<gw-id>.<relay-domain>`. It used to render
 *   only inside `RelayTunnelPanel`, which was gated behind
 *   `gatewayMode === "relay"`, and that panel polls the configured Gateway
 *   URL. So the only place the id appeared was unreachable at the exact
 *   moment it was needed: relay mode rejects a non-https address, and the
 *   https address does not resolve until you have the id. Local mode
 *   (the mode where the id is actually readable — the Gateway is right
 *   there) showed nothing at all.
 *
 * What this pins:
 *   1. The gw-id row renders in all three modes.
 *   2. It reports the id the Gateway returned, and an em-dash when the
 *      Gateway has none (tunnel never enabled).
 *   3. The help trigger shows the stock `Tooltip` on hover — not an inline
 *      expander, not a click popup, not a toast — with content that depends
 *      on whether the Gateway has an id, and it never reflows the row.
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

/** The row is labelled by the i18n string "Device ID" (en.json
 *  `settings.relayGwId`), so tests match the rendered label rather than a
 *  testid the production code has no reason to carry. */
const deviceIdRow = () => screen.getByText(/^Device ID:/);

/** Same idea for the "?" beside a non-loopback address in local mode. */
const urlWarn = () => screen.getByRole("button", { name: /probes this address first/i });

beforeEach(() => {
  useSettingsStore.setState({ gatewayMode: "local", gatewayUrl: "http://127.0.0.1:19876" });
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
    // Everything else the tab pulls (agents, nodes, health) — empty.
    return new Response("[]", {
      status: 200,
      headers: { "Content-Type": "application/json" } });
  });
}

describe("gw-id visibility", () => {
  it.each(["local", "remote", "relay"] as const)(
    "shows the row in %s mode",
    async (mode) => {
      mockRelayStatus(GW_ID);
      useSettingsStore.setState({ gatewayMode: mode });
      render(<GatewayTab />);

      // The regression: in local/remote this row did not exist at all.
      await waitFor(() => expect(deviceIdRow()).toBeTruthy());
      expect(deviceIdRow().textContent).toContain(GW_ID);
    },
  );

  it("shows an em-dash when the Gateway has no device id", async () => {
    mockRelayStatus(null);
    render(<GatewayTab />);

    await waitFor(() => expect(deviceIdRow().textContent).toContain("—"));
    expect(deviceIdRow().textContent).not.toContain(GW_ID);
  });

  it("keeps the row when the Gateway is unreachable", async () => {
    vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("ECONNREFUSED"));
    render(<GatewayTab />);

    // The row is part of the mode card's always-on chrome, not of the
    // connection status — it must not disappear because a fetch failed.
    await waitFor(() => expect(deviceIdRow()).toBeTruthy());
  });
});

describe("relay plain-http hint", () => {
  beforeEach(() => { vi.useFakeTimers({ shouldAdvanceTime: true }); });
  afterEach(() => { vi.useRealTimers(); });

  const HINT = /must be the https:\/\/ device domain|is not https:\/\//;

  const renderRelay = (url: string) => {
    mockRelayStatus(GW_ID);
    useSettingsStore.setState({ gatewayMode: "relay", gatewayUrl: url });
    return render(<GatewayTab />);
  };

  it("puts the plain-http error behind a ? on the field row", () => {
    renderRelay("http://gw.example.com:19876");
    // Was two red paragraphs under the input, which reflowed the card.
    expect(screen.queryByText(HINT)).toBeNull();
    expect(screen.getByRole("button", { name: HINT })).toBeTruthy();

    fireEvent.mouseEnter(screen.getByRole("button", { name: HINT }).parentElement!);
    act(() => { vi.advanceTimersByTime(500); });
    expect(screen.getByText(HINT)).toBeTruthy();
  });

  it("shows no ? for an https relay address, or in remote mode", () => {
    const { unmount } = renderRelay("https://abc.relay.example.com");
    expect(screen.queryByRole("button", { name: HINT })).toBeNull();
    unmount();

    useSettingsStore.setState({ gatewayMode: "remote" });
    render(<GatewayTab />);
    expect(screen.queryByRole("button", { name: HINT })).toBeNull();
  });

  it("still disables Apply and the test button for a plain-http draft", () => {
    renderRelay("https://abc.relay.example.com");
    fireEvent.change(screen.getByLabelText(/gateway url/i), { target: { value: "http://x" } });
    expect((screen.getByRole("button", { name: /^apply$/i }) as HTMLButtonElement).disabled).toBe(true);
  });
});

describe("local-mode URL hint", () => {
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

  it("keeps the non-loopback warning out of the layout, behind a ? hint", () => {
    mockRelayStatus(GW_ID);
    useSettingsStore.setState({ gatewayUrl: "http://10.0.0.5:19876" });
    render(<GatewayTab />);

    // No inline prose next to the address: it used to be an amber span
    // sitting after the URL, pushing both label/value lines into a wrap.
    expect(screen.queryByText(/probes this address first/i)).toBeNull();

    fireEvent.mouseEnter(urlWarn().parentElement!);
    act(() => { vi.advanceTimersByTime(500); });
    expect(screen.getByText(/probes this address first/i)).toBeTruthy();
  });

  it("shows no ? for a loopback address, and none outside local mode", () => {
    const warn = /probes this address first/i;

    const { unmount } = render(<GatewayTab />);
    expect(screen.queryByRole("button", { name: warn })).toBeNull();
    unmount();

    useSettingsStore.setState({ gatewayMode: "remote" });
    render(<GatewayTab />);
    expect(screen.queryByRole("button", { name: warn })).toBeNull();
  });

  /** `Tooltip` renders through `createPortal` to `document.body`, so it is
   *  not inside the settings card. `.fixed` is the portal wrapper; the tip
   *  itself is the child. */
  const tip = (needle: RegExp) => screen.getByText(needle);

  it("explains the pairing address when an id exists", async () => {
    mockRelayStatus(GW_ID);
    render(<GatewayTab />);
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
    await waitFor(() => expect(deviceIdRow()).toBeTruthy());

    hoverHelp();
    expect(tip(/Remote-access device id/)).toBeTruthy();

    unhoverHelp();
    expect(screen.queryByText(/Remote-access device id/)).toBeNull();
  });
});
