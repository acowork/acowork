/**
 * GatewaySwitcherMenu — the title-bar "minimized Settings ▸ Gateway", and
 * (since it absorbed the old GatewayStatusChip) the app's only Gateway
 * outage affordance.
 *
 * What this pins (each was a real way to get it wrong):
 *   1. The mode listbox is a FILTER, not a config editor. Selecting
 *      "local" must NOT call `setGatewayMode` — that pushes to Rust and
 *      rebuilds MQTT, which would switch the connection out from under
 *      the user with no address chosen.
 *   2. Picking a row switches through `applyGatewayUrl` + `checkHealth`
 *      (the old chip's entry point), NOT `setGatewayUrl`. `setGatewayUrl`
 *      alone skips the relay→remote correction and the auth re-validation.
 *   3. The list is `gatewayUrlHistory` (URLs that actually connected) with
 *      the current URL forced to the front — a first-run install has a URL
 *      but no history, and the panel must not be empty then.
 *   4. Probing is per-row and local; the panel must not write into
 *      `gatewayStore.candidates`, which the boot-time chooser owns.
 *
 * The outage block (formerly the chip) pins the parts that were easy to
 * break while merging: the trigger changing shape without unmounting,
 * visibility following `gatewayUnreachable` rather than `status ===
 * "error"` (the classifier stays silent on a black-holed path), and the
 * pill carrying no auto-dismiss.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { act, render, screen, fireEvent, waitFor } from "@testing-library/react";
import { GatewaySwitcherMenu } from "./GatewaySwitcherMenu";
import { useSettingsStore } from "../../stores/settingsStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { probeGateways } from "../../lib/gateway-probe";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => ({})) }));

// Probe is the network; stub it and record what it was asked to check.
const probeMock = vi.hoisted(() => vi.fn(async () => [] as { url: string; ok: boolean; latencyMs: number }[]));
vi.mock("../../lib/gateway-probe", () => ({ probeGateways: probeMock }));

const openBtn = () => screen.getByRole("button", { name: /Gateway|网关|閘道|ゲートウェイ|게이트웨이/ });

const open = () => {
  render(<GatewaySwitcherMenu />);
  fireEvent.click(openBtn());
};

describe("GatewaySwitcherMenu", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.clearAllMocks();
    probeMock.mockImplementation(async () => []);
    useSettingsStore.setState({
      gatewayMode: "remote",
      gatewayUrl: "http://192.168.1.10:19876",
      gatewayUrlHistory: ["http://192.168.1.10:19876", "http://192.168.1.20:19876"],
    });
    useGatewayStore.setState({
      status: "connected",
      localState: "running",
      gatewayUnreachable: false,
      candidates: [],
    });
  });

  it("lists the current URL first, then recorded history", () => {
    open();
    const rows = screen.getAllByRole("button", { name: /19876/ });
    expect(rows[0].getAttribute("title")).toBe("http://192.168.1.10:19876");
    expect(screen.getByRole("button", { name: /192\.168\.1\.20/ })).toBeTruthy();
  });

  it("mode listbox filters the list WITHOUT changing the configured mode", () => {
    const setSpy = vi.spyOn(useSettingsStore.getState(), "setGatewayMode");
    open();
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "local" } });
    // Filtered: local shows only the loopback/current address, with the
    // lifecycle row instead of the browsable history.
    expect(screen.getByText("http://192.168.1.10:19876")).toBeTruthy();
    expect(screen.queryByText("http://192.168.1.20:19876")).toBeNull();
    // ...and the live config was NOT committed.
    expect(setSpy).not.toHaveBeenCalled();
    expect(useSettingsStore.getState().gatewayMode).toBe("remote");
    setSpy.mockRestore();
  });

  it("clicking another address switches via applyGatewayUrl + checkHealth", async () => {
    probeMock.mockImplementation(async (urls: string[], opts?: { onSettled?: (r: never) => void }) => {
      urls.forEach((u) => opts?.onSettled?.({ url: u, ok: true, latencyMs: 3 } as never));
      return [];
    });
    const applySpy = vi.spyOn(useSettingsStore.getState(), "applyGatewayUrl");
    const checkSpy = vi.spyOn(useGatewayStore.getState(), "checkHealth").mockResolvedValue();
    open();
    const row = screen.getByRole("button", { name: /192\.168\.1\.20/ });
    await waitFor(() => expect((row as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(row);
    expect(applySpy).toHaveBeenCalledWith("http://192.168.1.20:19876");
    expect(checkSpy).toHaveBeenCalled();
    applySpy.mockRestore();
    checkSpy.mockRestore();
  });

  it("disables only the probe-rejected address — a reachable one stays clickable", async () => {
    // Three rows so "disabled because unreachable" is distinguishable from
    // "disabled because it is the current address": `.10` is current,
    // `.20` probes ok, `.30` probes dead.
    useSettingsStore.setState({
      gatewayUrlHistory: [
        "http://192.168.1.10:19876",
        "http://192.168.1.20:19876",
        "http://192.168.1.30:19876",
      ],
    });
    probeMock.mockImplementation(async (urls: string[], opts?: { onSettled?: (r: never) => void }) => {
      urls.forEach((u) =>
        opts?.onSettled?.({ url: u, ok: !u.includes(".30"), latencyMs: 3 } as never),
      );
      return [];
    });
    open();
    await waitFor(() =>
      expect(
        (screen.getByRole("button", { name: /192\.168\.1\.30/ }) as HTMLButtonElement).disabled,
      ).toBe(true),
    );
    expect(
      (screen.getByRole("button", { name: /192\.168\.1\.20/ }) as HTMLButtonElement).disabled,
    ).toBe(false);
  });

  it("a probe-rejected address cannot fire applyGatewayUrl", async () => {
    probeMock.mockImplementation(async (urls: string[], opts?: { onSettled?: (r: never) => void }) => {
      urls.forEach((u) => opts?.onSettled?.({ url: u, ok: false, latencyMs: 0 } as never));
      return [];
    });
    const applySpy = vi.spyOn(useSettingsStore.getState(), "applyGatewayUrl");
    open();
    await waitFor(() =>
      expect(
        (screen.getByRole("button", { name: /192\.168\.1\.20/ }) as HTMLButtonElement).disabled,
      ).toBe(true),
    );
    fireEvent.click(screen.getByRole("button", { name: /192\.168\.1\.20/ }));
    expect(applySpy).not.toHaveBeenCalled();
    applySpy.mockRestore();
  });

  it("disables a still-probing address until its probe lands", () => {
    // The window between opening the menu and the first verdict arriving
    // is exactly the one an unreachable row is clicked through. Nothing
    // may be clickable in it — not even a reachable host, because nothing
    // has been proven yet.
    probeMock.mockImplementation(async () => []);
    open();
    expect(
      (screen.getByRole("button", { name: /192\.168\.1\.20/ }) as HTMLButtonElement).disabled,
    ).toBe(true);
  });

  it("enables a row the moment its probe answers ok", async () => {
    probeMock.mockImplementation(async (urls: string[], opts?: { onSettled?: (r: never) => void }) => {
      opts?.onSettled?.({ url: urls[1], ok: true, latencyMs: 3 } as never);
      return [];
    });
    open();
    await waitFor(() =>
      expect(
        (screen.getByRole("button", { name: /192\.168\.1\.20/ }) as HTMLButtonElement).disabled,
      ).toBe(false),
    );
  });

  it("probes the browsed list on open and does NOT touch gatewayStore.candidates", async () => {
    probeMock.mockImplementation(async (urls: string[], opts?: { onSettled?: (r: never) => void }) => {
      urls.forEach((u) => opts?.onSettled?.({ url: u, ok: u.endsWith(".20:19876"), latencyMs: 3 } as never));
      return [];
    });
    open();
    await waitFor(() => expect(probeMock).toHaveBeenCalled());
    expect(probeMock.mock.calls[0][0]).toEqual([
      "http://192.168.1.10:19876",
      "http://192.168.1.20:19876",
    ]);
    // The outage chip owns this slice — borrowing it would leak this
    // menu's rows into the failover banner.
    expect(useGatewayStore.getState().candidates).toEqual([]);
  });

  it("does not probe in local mode (single loopback address)", () => {
    useSettingsStore.setState({ gatewayMode: "local" });
    open();
    expect(probeMock).not.toHaveBeenCalled();
  });

  it("closes on Escape", () => {
    open();
    expect(screen.getByRole("button", { name: /192\.168\.1\.20/ })).toBeTruthy();
    act(() => {
      fireEvent.keyDown(document, { key: "Escape" });
    });
    expect(screen.queryByRole("button", { name: /192\.168\.1\.20/ })).toBeNull();
  });
});

/**
 * Outage half of the merged trigger — everything here used to be pinned by
 * GatewayStatusChip.test.tsx, which was deleted with the component. The
 * behaviours are load-bearing, not cosmetic:
 *   - the pill is the ONLY thing telling the user the app is cut off, so
 *     it must not depend on the death classifier, which never fires on a
 *     black-holed route;
 *   - and it must never auto-dismiss, because a sleep/wake drop is still
 *     broken until the user acts.
 */
describe("GatewaySwitcherMenu outage trigger", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.clearAllMocks();
    probeMock.mockImplementation(async () => []);
    useSettingsStore.setState({
      gatewayMode: "remote",
      gatewayUrl: "http://192.168.1.10:19876",
      gatewayUrlHistory: ["http://192.168.1.10:19876", "http://192.168.1.20:19876"],
    });
    useGatewayStore.setState({
      status: "disconnected",
      localState: "idle",
      gatewayUnreachable: true,
      candidates: [],
    });
  });

  const pill = () => screen.getByRole("button", { expanded: false });

  it("turns the trigger into a breathing amber pill on a drop", () => {
    render(<GatewaySwitcherMenu />);
    const btn = pill();
    // No auto-dismiss anywhere — a sleep/wake drop is still broken until
    // the user picks an address, so the pill must stay lit.
    expect(btn.className).not.toMatch(/animate-\[.*fade/);
    // Solid amber fill + pulsing icon: the fill carries the attention,
    // the pulse marks the outage as live and unattended.
    expect(btn.className).toContain("bg-amber-500");
    expect(btn.querySelector(".animate-pulse")).toBeTruthy();
  });

  it("follows gatewayUnreachable, NOT the death classifier's status", () => {
    // A classifier verdict alone must NOT raise the pill — the 5s budget
    // lives in the connectivity module.
    useGatewayStore.setState({ status: "error", gatewayUnreachable: false });
    const { container, rerender } = render(<GatewaySwitcherMenu />);
    expect(container.querySelector('[aria-label="Gateway Status"]')).toBeNull();
    // ...and the inverse: unreachable raises it even though the
    // classifier never wrote `error` — a black-holed path, exactly the
    // outage that used to stay hidden for minutes.
    act(() => useGatewayStore.setState({ status: "connecting", gatewayUnreachable: true }));
    rerender(<GatewaySwitcherMenu />);
    expect(screen.getByRole("button", { expanded: false })).toBeTruthy();
  });

  it("drops back to the plain glyph once recovery clears the signal", () => {
    const { rerender } = render(<GatewaySwitcherMenu />);
    expect(pill()).toBeTruthy();
    act(() => useGatewayStore.setState({ gatewayUnreachable: false }));
    rerender(<GatewaySwitcherMenu />);
    expect(screen.queryByRole("button", { expanded: false, name: /Gateway Status/ })).toBeNull();
    // Back to the ordinary switcher, still functional.
    expect(openBtn()).toBeTruthy();
  });

  it("the pill opens the SAME popover the healthy glyph opens", async () => {
    probeMock.mockImplementation(async (urls: string[], opts?: { onSettled?: (r: never) => void }) => {
      for (const url of urls) opts?.onSettled?.({ url, ok: url.endsWith(".20:19876"), latencyMs: 2 } as never);
      return [];
    });
    render(<GatewaySwitcherMenu />);
    fireEvent.click(pill());
    // One list, not two: the old chip rendered its own candidate block
    // above the menu's address list, offering the same URLs twice in two
    // differently-decorated lists. The address list IS the candidate list.
    const reachable = await screen.findByRole("button", { name: /192\.168\.1\.20/ });
    expect((reachable as HTMLButtonElement).disabled).toBe(false);
    // The outage reason is the one thing the list can't offer — it shows
    // twice on purpose (pill + popover heading), matching the old chip's
    // own "pill says it, popover repeats it" arrangement.
    expect(screen.getAllByText(/unreachable|不可达|到達できません|연결할 수 없음/)).toHaveLength(2);
    // Retry is available and is NOT the mode listbox.
    fireEvent.click(screen.getByRole("button", { name: /Retry|重试|再試行|재시도/ }));
    expect(probeMock.mock.calls.length).toBeGreaterThan(1);
  });

  it("start button appears for a down local gateway, retry alone otherwise", () => {
    render(<GatewaySwitcherMenu />);
    fireEvent.click(pill());
    // Remote outage: nothing to start, only retry.
    expect(screen.queryByRole("button", { name: /Start Gateway|启动网关|啟動閘道/ })).toBeNull();
    expect(screen.getByRole("button", { name: /Retry|重试|再試行|재시도/ })).toBeTruthy();
  });

  it("offers Start (not just Retry) when the local gateway is the one down", () => {
    useSettingsStore.setState({ gatewayMode: "local" });
    render(<GatewaySwitcherMenu />);
    fireEvent.click(pill());
    expect(screen.getByRole("button", { name: /Start Gateway|启动网关|啟動閘道/ })).toBeTruthy();
    // A gateway already booting must not offer to boot again.
    act(() => useGatewayStore.setState({ localState: "starting" }));
    fireEvent.click(screen.getByRole("button", { name: /Retry|重试|再試行|재시도/ }));
    expect(screen.queryByRole("button", { name: /Start Gateway|启动网关|啟動閘道/ })).toBeNull();
  });
});