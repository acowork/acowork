/**
 * GatewaySwitcherMenu — the title-bar "minimized Settings ▸ Gateway", and
 * (since it absorbed the old GatewayStatusChip) the app's only Gateway
 * outage affordance.
 *
 * ADR-115 changes what this pins. The old contract was "the mode listbox is
 * a FILTER, not a config editor"; the control itself is gone, because a
 * user-visible local/remote mode was the thing that made a wrong address
 * unrecoverable. What is load-bearing now:
 *
 *   1. There is no mode control at all — `localhost` is an ordinary row.
 *   2. Picking a row goes through `switchGateway` (session drop + full boot
 *      restart), never a bare URL write.
 *   3. The list is `gatewayAddressOptions`: localhost, then last-good, then
 *      the current address, then connected history.
 *   4. A `localhost` row is clickable even when its probe failed — that is
 *      the escape hatch that spawns a local Gateway.
 *   5. Probing is per-row and local; the panel must not write into
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

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => ({})) }));

// Probe is the network; stub it and record what it was asked to check.
const probeMock = vi.hoisted(() =>
  vi.fn(async () => [] as { url: string; ok: boolean; latencyMs: number }[]),
);
vi.mock("../../lib/gateway-probe", () => ({ probeGateways: probeMock }));

const LOOPBACK = "http://127.0.0.1:19876";
const CURRENT = "http://192.168.1.10:19876";
const OTHER = "http://192.168.1.20:19876";
const THIRD = "http://192.168.1.30:19876";

const openBtn = () => screen.getByRole("button", { name: /Gateway|网关|閘道|ゲートウェイ|게이트웨이/ });

const open = () => {
  render(<GatewaySwitcherMenu />);
  fireEvent.click(openBtn());
};

const row = (fragment: RegExp) => screen.getByRole("button", { name: fragment });

function settleAll(ok: (url: string) => boolean) {
  probeMock.mockImplementation(async (urls: string[], opts?: { onSettled?: (r: never) => void }) => {
    urls.forEach((u) => opts?.onSettled?.({ url: u, ok: ok(u), latencyMs: 3 } as never));
    return [];
  });
}

describe("GatewaySwitcherMenu", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.clearAllMocks();
    probeMock.mockImplementation(async () => []);
    useSettingsStore.setState({
      gatewayUrl: CURRENT,
      gatewayUrlHistory: [CURRENT, OTHER],
      lastGoodUrl: null,
      relayActive: false,
    });
    useGatewayStore.setState({
      status: "connected",
      localState: "running",
      gatewayUnreachable: false,
      candidates: [],
    });
  });

  it("offers localhost first, then the current address and connected history", async () => {
    settleAll(() => true);
    open();
    await waitFor(() => expect(probeMock).toHaveBeenCalled());
    const urls = screen.getAllByTitle(/19876/).map((el) => el.getAttribute("title"));
    expect(urls).toEqual([LOOPBACK, CURRENT, OTHER]);
  });

  it("has no mode control (the address is the only connection setting)", () => {
    open();
    expect(screen.queryByRole("combobox")).toBeNull();
  });

  it("switching a row goes through switchGateway, not a bare URL write", async () => {
    settleAll(() => true);
    const switchSpy = vi
      .spyOn(useSettingsStore.getState(), "switchGateway")
      .mockResolvedValue();
    open();
    await waitFor(() => expect((row(/192\.168\.1\.20/) as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(row(/192\.168\.1\.20/));
    expect(switchSpy).toHaveBeenCalledWith(OTHER);
    switchSpy.mockRestore();
  });

  it("disables only the probe-rejected remote address — a reachable one stays clickable", async () => {
    useSettingsStore.setState({ gatewayUrlHistory: [CURRENT, OTHER, THIRD] });
    settleAll((u) => !u.includes(".30"));
    open();
    await waitFor(() => expect((row(/192\.168\.1\.30/) as HTMLButtonElement).disabled).toBe(true));
    expect((row(/192\.168\.1\.20/) as HTMLButtonElement).disabled).toBe(false);
  });

  it("a probe-rejected address cannot fire switchGateway", async () => {
    settleAll(() => false);
    const switchSpy = vi
      .spyOn(useSettingsStore.getState(), "switchGateway")
      .mockResolvedValue();
    open();
    await waitFor(() => expect((row(/192\.168\.1\.20/) as HTMLButtonElement).disabled).toBe(true));
    fireEvent.click(row(/192\.168\.1\.20/));
    expect(switchSpy).not.toHaveBeenCalled();
    switchSpy.mockRestore();
  });

  it("keeps localhost clickable even when no local Gateway answers (it will be spawned)", async () => {
    settleAll(() => false);
    open();
    await waitFor(() => expect((row(/192\.168\.1\.20/) as HTMLButtonElement).disabled).toBe(true));
    expect((row(/127\.0\.0\.1/) as HTMLButtonElement).disabled).toBe(false);
  });

  it("disables a still-probing remote address until its probe lands", () => {
    probeMock.mockImplementation(async () => []);
    open();
    expect((row(/192\.168\.1\.20/) as HTMLButtonElement).disabled).toBe(true);
  });

  it("enables a row the moment its probe answers ok", async () => {
    probeMock.mockImplementation(async (urls: string[], opts?: { onSettled?: (r: never) => void }) => {
      const target = urls.find((u) => u.includes(".20")) ?? urls[0];
      opts?.onSettled?.({ url: target, ok: true, latencyMs: 3 } as never);
      return [];
    });
    open();
    await waitFor(() => expect((row(/192\.168\.1\.20/) as HTMLButtonElement).disabled).toBe(false));
  });

  it("probes the whole address list on open and does NOT touch gatewayStore.candidates", async () => {
    settleAll(() => true);
    open();
    await waitFor(() => expect(probeMock).toHaveBeenCalled());
    expect(probeMock.mock.calls[0][0]).toEqual([LOOPBACK, CURRENT, OTHER]);
    // The boot-time chooser owns this slice — borrowing it would leak this
    // menu's rows into the failover banner.
    expect(useGatewayStore.getState().candidates).toEqual([]);
  });

  it("still probes when the current address is loopback", async () => {
    useSettingsStore.setState({ gatewayUrl: LOOPBACK, gatewayUrlHistory: [LOOPBACK] });
    settleAll(() => true);
    open();
    await waitFor(() => expect(probeMock).toHaveBeenCalled());
  });

  it("closes on Escape", () => {
    open();
    expect(row(/192\.168\.1\.20/)).toBeTruthy();
    act(() => {
      fireEvent.keyDown(document, { key: "Escape" });
    });
    expect(screen.queryByRole("button", { name: /192\.168\.1\.20/ })).toBeNull();
  });

  it("the Details button fires onOpenGatewaySettings exactly once", () => {
    const onOpen = vi.fn();
    render(<GatewaySwitcherMenu onOpenGatewaySettings={onOpen} />);
    fireEvent.click(openBtn());
    fireEvent.click(screen.getByRole("button", { name: /详情|Details|詳細|상세/ }));
    expect(onOpen).toHaveBeenCalledTimes(1);
  });

  it("hides the Details button when no callback is provided", () => {
    open();
    expect(screen.queryByRole("button", { name: /详情|Details|詳細|상세/ })).toBeNull();
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
      gatewayUrl: CURRENT,
      gatewayUrlHistory: [CURRENT, OTHER],
      lastGoodUrl: null,
      relayActive: false,
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
    expect(btn.className).not.toMatch(/animate-\[.*fade/);
    expect(btn.className).toContain("bg-amber-500");
    expect(btn.querySelector(".animate-pulse")).toBeTruthy();
  });

  it("follows gatewayUnreachable, NOT the death classifier's status", () => {
    useGatewayStore.setState({ status: "error", gatewayUnreachable: false });
    const { container, rerender } = render(<GatewaySwitcherMenu />);
    expect(container.querySelector('[aria-label="Gateway Status"]')).toBeNull();
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
    expect(openBtn()).toBeTruthy();
  });

  it("the pill opens the SAME popover the healthy glyph opens", async () => {
    settleAll((u) => u.includes(".20"));
    render(<GatewaySwitcherMenu />);
    fireEvent.click(pill());
    const reachable = await screen.findByRole("button", { name: /192\.168\.1\.20/ });
    expect((reachable as HTMLButtonElement).disabled).toBe(false);
    expect(screen.getAllByText(/unreachable|不可达|到達できません|연결할 수 없음/)).toHaveLength(2);
    fireEvent.click(screen.getByRole("button", { name: /Retry|重试|再試行|재시도/ }));
    expect(probeMock.mock.calls.length).toBeGreaterThan(1);
  });

  it("start button appears for a down local gateway, retry alone otherwise", () => {
    render(<GatewaySwitcherMenu />);
    fireEvent.click(pill());
    expect(screen.queryByRole("button", { name: /^(Start|启动|啟動|起動|시작)$/ })).toBeNull();
    expect(screen.getByRole("button", { name: /Retry|重试|再試行|재시도/ })).toBeTruthy();
  });

  it("offers Start (not just Retry) when the local gateway is the one down", () => {
    // `isLocal` derives from the address now — no mode flag to set.
    useSettingsStore.setState({ gatewayUrl: LOOPBACK, gatewayUrlHistory: [LOOPBACK] });
    render(<GatewaySwitcherMenu />);
    fireEvent.click(pill());
    expect(screen.getByRole("button", { name: /^(Start|启动|啟動|起動|시작)$/ })).toBeTruthy();
    act(() => useGatewayStore.setState({ localState: "starting" }));
    fireEvent.click(screen.getByRole("button", { name: /Retry|重试|再試行|재시도/ }));
    expect(screen.queryByRole("button", { name: /^(Start|启动|啟動|起動|시작)$/ })).toBeNull();
  });
});
