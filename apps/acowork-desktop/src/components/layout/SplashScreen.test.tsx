/**
 * SplashScreen timeout recovery (ADR-115).
 *
 * When the persisted address is unreachable, the recovery view must be a
 * real way out. The old view offered a local/remote/relay dropdown plus a
 * URL field that was hidden in local mode — which is how a user ended up
 * stuck: the mode said `local`, the address was remote, and the only
 * control they needed was behind the mode they could not see.
 *
 * Now the timeout view renders the SAME `GatewayAddressPicker` as the login
 * screen: localhost is an ordinary row, an address commit goes through
 * `switchGateway` (session drop + full boot restart), and `Retry` re-runs
 * the pipeline for the current address without editing anything.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, act } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { SplashScreen } from "./SplashScreen";
import { useSettingsStore } from "../../stores/settingsStore";
import { useGatewayStore } from "../../stores/gatewayStore";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async () => ({})),
}));

const OLD_URL = "http://192.168.1.10:19876";
const NEW_URL = "http://192.168.1.11:19876";
const LOCAL_URL = "http://127.0.0.1:19876";
// Must match REMOTE_MAX_WAIT_MS in SplashScreen.tsx (a non-loopback address
// gets the short reachability budget).
const MAX_WAIT_MS = 12_000;

/** Flush chained microtasks (bootGateway awaits several resolved mocks). */
async function flushMicrotasks() {
  for (let i = 0; i < 10; i++) {
    await act(async () => {});
  }
}

async function driveToTimeout() {
  render(<SplashScreen onReady={vi.fn()} />);
  await flushMicrotasks();
  await act(async () => {
    vi.advanceTimersByTime(MAX_WAIT_MS + 1_000);
  });
  await flushMicrotasks();
}

const addressInput = () =>
  screen.getByRole("textbox", { name: /Server address|服务器地址/i }) as HTMLInputElement;

const connectButton = () =>
  screen.getByRole("button", { name: /^(Connect|连接)$/ }) as HTMLButtonElement;

/** The known-address list now lives ONLY inside the combobox dropdown —
 *  the flat duplicate list below the field is gone. Open it before looking
 *  for an option row. */
const openHistoryDropdown = () =>
  fireEvent.click(screen.getByRole("button", { name: "Show history" }));

function configPushCount(): number {
  return vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === "set_gateway_config").length;
}

describe("SplashScreen timeout recovery", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal("requestAnimationFrame", () => 0);
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.reject(new Error("unreachable"))),
    );
    useSettingsStore.setState({
      gatewayUrl: OLD_URL,
      gatewayMode: "remote",
      relayActive: false,
      gatewayUrlHistory: [],
      lastGoodUrl: null,
    });
    useGatewayStore.setState({
      status: "disconnected",
      health: null,
      localState: "idle",
      localOwnership: "none",
      candidates: [],
    });
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("shows an address picker with localhost as an ordinary row — and no mode control", async () => {
    await driveToTimeout();

    expect(screen.getByText("Retry Connection")).toBeTruthy();
    expect(addressInput().value).toBe(OLD_URL);
    // localhost is an option row in the dropdown (not hidden behind a
    // "local mode"), alongside the current remote address.
    openHistoryDropdown();
    expect(screen.getByTitle(LOCAL_URL)).toBeTruthy();
    expect(screen.getByTitle(OLD_URL)).toBeTruthy();
    // The mode dropdown is gone for good.
    expect(screen.queryByRole("combobox")).toBeNull();
  });

  it("committing a new address switches the connection instead of only editing text", async () => {
    const switchSpy = vi
      .spyOn(useSettingsStore.getState(), "switchGateway")
      .mockResolvedValue();
    await driveToTimeout();

    fireEvent.change(addressInput(), { target: { value: NEW_URL } });
    fireEvent.click(connectButton());
    await flushMicrotasks();

    expect(switchSpy).toHaveBeenCalledWith(NEW_URL);
  });

  it("clicking a localhost option is a real escape hatch, not a dead address", async () => {
    const switchSpy = vi
      .spyOn(useSettingsStore.getState(), "switchGateway")
      .mockResolvedValue();
    await driveToTimeout();

    openHistoryDropdown();
    fireEvent.click(screen.getByTitle(LOCAL_URL));
    await flushMicrotasks();

    // Localhost never answered /health here, yet the row is still usable:
    // `switchGateway` restarts the boot pipeline, which spawns a local
    // Gateway when the address resolves to this machine.
    expect(switchSpy).toHaveBeenCalledWith(LOCAL_URL);
  });

  it("Retry re-runs the boot pipeline for the current address and stays on the recovery view", async () => {
    await driveToTimeout();
    const before = configPushCount();

    fireEvent.click(screen.getByText("Retry Connection"));
    await flushMicrotasks();

    expect(configPushCount()).toBeGreaterThan(before);
    expect(useSettingsStore.getState().gatewayUrl).toBe(OLD_URL);
    // Still unreachable → the editable recovery view must be back, not an
    // endless "Retrying..." spinner.
    expect(screen.getByText("Retry Connection")).toBeTruthy();
  });
});
