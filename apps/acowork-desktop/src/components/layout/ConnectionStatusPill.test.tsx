/**
 * The connection pill is now the ONLY place the MQTT/agent liveness is shown
 * as an alarm (it used to be a strip inside the composer). What this pins:
 *   - healthy → nothing rendered (a permanent pill would out-shout the title
 *     bar's real controls);
 *   - `idle` and `disconnected` are terminal states with DIFFERENT fixes —
 *     `idle` has no Rust client to rebuild, so it must call `connect_mqtt`,
 *     while `disconnected` calls `force_reconnect_mqtt`. Swapping the two
 *     leaves a dead button (the original P0-7 bug).
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { ConnectionStatusPill } from "./ConnectionStatusPill";
import { useChatStore } from "../../stores/chatStore";

const invoke = vi.fn(async () => ({}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));

describe("ConnectionStatusPill", () => {
  beforeEach(() => {
    invoke.mockClear();
    useChatStore.setState({ effectiveConnection: "connected", staleSince: null });
  });

  it("renders nothing while the connection is healthy", () => {
    const { container } = render(<ConnectionStatusPill />);
    expect(container.firstChild).toBeNull();
  });

  it("bootstraps the Rust client on `idle` via connect_mqtt", async () => {
    useChatStore.setState({ effectiveConnection: "idle" });
    render(<ConnectionStatusPill />);
    fireEvent.click(screen.getByTestId("chat-connection-banner-connect"));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("connect_mqtt"));
  });

  it("rebuilds the client on `disconnected` via force_reconnect_mqtt", async () => {
    useChatStore.setState({ effectiveConnection: "disconnected" });
    render(<ConnectionStatusPill />);
    fireEvent.click(screen.getByTestId("chat-connection-banner-reconnect"));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("force_reconnect_mqtt"));
  });

  it("counts down to the watchdog's forced reconnect instead of offering a button", () => {
    useChatStore.setState({
      effectiveConnection: "stale",
      staleSince: Date.now(),
    });
    render(<ConnectionStatusPill />);
    expect(screen.getByTestId("chat-connection-banner-countdown")).toBeTruthy();
    expect(screen.queryByTestId("chat-connection-banner-connect")).toBeNull();
  });
});