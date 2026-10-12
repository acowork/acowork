/**
 * TitleBar view-toggle buttons.
 *
 * Why this file exists:
 *   The two buttons added left of the window controls (global search, right
 *   panel toggle) are pure wiring over two stores — there is no layout math
 *   to verify, but plenty of ways to wire them wrong:
 *     1. search button calls `closeDialog` (or toggles), so the first click
 *        never opens the dialog;
 *     2. the panel button reads a *local* copy of `rightPanelCollapsed`
 *        instead of the store, so it shows the wrong icon and the RightNavBar
 *        rows disagree with the title bar;
 *     3. it sets a constant instead of flipping, so the second click is a no-op.
 *
 * What this pins: both buttons drive the same store the rest of the app
 * reads (`searchStore`, `layoutStore.rightPanelCollapsed`), and the panel
 * button round-trips across both states.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { act, render, screen, fireEvent } from "@testing-library/react";
import { TitleBar, RightPanelIcon } from "./TitleBar";
import { useSearchStore } from "../../stores/searchStore";
import { useLayoutStore } from "../../stores/layoutStore";
import { useGatewayStore } from "../../stores/gatewayStore";

// TitleBar calls getCurrentWindow() during render; jsdom has no Tauri IPC.
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    minimize: vi.fn(async () => {}),
    toggleMaximize: vi.fn(async () => {}),
    close: vi.fn(async () => {}),
  }),
}));

// The gateway menu probes URL history via fetch + Tauri invoke when it
// opens; stub both so the mounting tests stay hermetic.
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async () => ({})),
}));

const searchBtn = () => screen.getByRole("button", { name: /Ctrl\+Shift\+F/ });
const panelBtn = () =>
  screen.getByRole("button", { name: /Right Panel/i });

describe("TitleBar view toggles", () => {
  beforeEach(() => {
    useSearchStore.setState({ open: false });
    useLayoutStore.setState({ rightPanelCollapsed: true });
    // ADR-052: the outage chip is steady-state only; start from healthy
    // so the toggle tests below aren't coupled to a gateway outage.
    useGatewayStore.setState({ status: "connected", candidates: [] });
  });

  it("search button opens the global search dialog", () => {
    render(<TitleBar />);
    expect(useSearchStore.getState().open).toBe(false);
    fireEvent.click(searchBtn());
    expect(useSearchStore.getState().open).toBe(true);
  });

  it("search button never closes an open dialog (it is not a toggle)", () => {
    useSearchStore.setState({ open: true });
    render(<TitleBar />);
    fireEvent.click(searchBtn());
    expect(useSearchStore.getState().open).toBe(true);
  });

  it("panel button round-trips both states off the store", () => {
    render(<TitleBar />);
    const set = useLayoutStore.getState().setRightPanelCollapsed;
    // Collapsed on mount -> first click opens.
    fireEvent.click(panelBtn());
    expect(useLayoutStore.getState().rightPanelCollapsed).toBe(false);
    // Open -> second click closes again (guards against a set-constant bug).
    act(() => set(false));
    fireEvent.click(panelBtn());
    expect(useLayoutStore.getState().rightPanelCollapsed).toBe(true);
  });

  it("panel glyph shows fill=open, hollow=closed (no arrow)", () => {
    // State is carried by the right pane's fill, not by swapping in an arrow
    // icon — the arrow reading was rejected as too ambiguous at 14px.
    const { container, rerender } = render(<RightPanelIcon open />);
    // rect[0] = outer frame, rect[1] = right pane (the stateful part)
    const pane = (container.querySelectorAll("rect")[1] as SVGElement);
    expect(pane.getAttribute("fill")).toBe("var(--color-accent)");
    rerender(<RightPanelIcon open={false} />);
    const hollow = (container.querySelectorAll("rect")[1] as SVGElement);
    // No fill attribute at all — it inherits `fill="none"` from the <svg>
    // root, which is what makes the pane hollow. Asserting the *absence*
    // rather than a literal "none" keeps the test honest about the markup.
    expect(hollow.getAttribute("fill")).toBeNull();
  });

  it("panel button tracks the store, not a local snapshot", () => {
    // RightNavBar / RightPanel can collapse the panel on their own; the
    // title-bar icon must follow without the user having re-rendered it.
    act(() => useLayoutStore.setState({ rightPanelCollapsed: false }));
    const { rerender } = render(<TitleBar />);
    expect(panelBtn().getAttribute("aria-pressed")).toBe("true");
    act(() => useLayoutStore.setState({ rightPanelCollapsed: true }));
    rerender(<TitleBar />);
    expect(panelBtn().getAttribute("aria-pressed")).toBe("false");
  });
});

/**
 * The Gateway outage signal is mounted by TitleBar. Worth pinning here
 * because the wiring is invisible in either file alone: TitleBar only
 * renders it because it imports GatewaySwitcherMenu, and it only appears
 * because AppLayout gates the whole post-boot tree on `gatewayReady`. A
 * dropped import would silently leave the app with no outage indicator
 * at all.
 *
 * The trigger moved from a second button next to the wordmark into the
 * gateway switcher's own trigger (one control, two shapes) — so these
 * assertions are now scoped by `aria-label`, which is the one thing the
 * two shapes do not share.
 */
/**
 * The title bar's shape, not its behaviour.
 *
 * This regressed for real: the gateway merge briefly split the app
 * toggles and the window controls into two sibling groups so the toggles
 * could render on macOS, and `justify-between` over three children put
 * those three toggles dead centre of the window. It reads as correct in
 * a diff and catastrophic on screen, and no button-level assertion can
 * see it — every button was still there, still wired, just in the wrong
 * place. Hence a structural assertion on the child count.
 */
describe("TitleBar layout shape", () => {
  beforeEach(() => {
    useSearchStore.setState({ open: false });
    useLayoutStore.setState({ rightPanelCollapsed: true });
    useGatewayStore.setState({ status: "connected", gatewayUnreachable: false, candidates: [] });
  });

  it("has exactly two children: title left, one right-aligned cluster", () => {
    const { container } = render(<TitleBar />);
    const bar = container.firstElementChild as HTMLElement;
    // `justify-between` means child N sits in slot N: 1 = far left,
    // 2 = far right, 3 = dead centre. The bar may only ever have two.
    expect(bar.className).toContain("justify-between");
    expect(bar.children).toHaveLength(2);
    // Slot 1 is the wordmark, slot 2 is everything clickable.
    expect(bar.children[0].textContent).toContain("ACowork");
    expect(bar.children[1].querySelectorAll("button").length).toBeGreaterThan(0);
  });

  it("keeps app toggles and OS chrome in ONE cluster, toggle-side first", () => {
    const { container } = render(<TitleBar />);
    const right = container.firstElementChild!.children[1];
    const buttons = Array.from(right.querySelectorAll("button"));
    const labels = buttons.map((b) => b.getAttribute("aria-label") ?? "");
    // Order matters: which Gateway am I on is more fundamental than the
    // search index that belongs to it, and OS window chrome reads last.
    const idx = (re: RegExp) => labels.findIndex((l) => re.test(l));
    const gw = idx(/Gateway|网关|閘道|ゲートウェイ|게이트웨이/);
    const search = idx(/Ctrl\+Shift\+F|全局搜索|全域搜尋|グローバル検索|전역 검색/);
    const panel = idx(/Right Panel|右侧面板|右側面板|右パネル|오른쪽/);
    const chrome = idx(/^$/); // window buttons carry no aria-label
    expect(gw).toBeGreaterThanOrEqual(0);
    expect(gw).toBeLessThan(search);
    expect(search).toBeLessThan(panel);
    expect(panel).toBeLessThan(chrome);
  });
});

describe("TitleBar Gateway outage pill", () => {
  beforeEach(() => {
    useSearchStore.setState({ open: false });
    useLayoutStore.setState({ rightPanelCollapsed: true });
    useGatewayStore.setState({ status: "disconnected", gatewayUnreachable: true, candidates: [] });
    vi.stubGlobal("fetch", vi.fn(() => Promise.reject(new Error("down"))));
  });

  it("mounts the pill on a steady-state gateway drop", () => {
    render(<TitleBar />);
    expect(screen.getByRole("button", { expanded: false, name: /Gateway Status/ })).toBeTruthy();
  });

  it("renders no pill while the gateway is healthy", () => {
    act(() => useGatewayStore.setState({ status: "connected", gatewayUnreachable: false }));
    const { container } = render(<TitleBar />);
    // Scoped to the pill by name, not to `aria-expanded` in general: the
    // title bar also hosts the search / right-panel toggles.
    expect(
      container.querySelector('[aria-label="Gateway Status"]'),
    ).toBeNull();
  });

  it("stays on the title bar until recovery clears the signal, never auto-hides", () => {
    const { rerender } = render(<TitleBar />);
    expect(screen.queryByRole("button", { expanded: false, name: /Gateway Status/ })).toBeTruthy();
    // Recovery clears it — the connectivity module drops the
    // `gatewayUnreachable` signal on the MQTT rise; nothing else does.
    act(() => useGatewayStore.setState({ gatewayUnreachable: false }));
    rerender(<TitleBar />);
    expect(screen.queryByRole("button", { expanded: false, name: /Gateway Status/ })).toBeNull();
  });
});