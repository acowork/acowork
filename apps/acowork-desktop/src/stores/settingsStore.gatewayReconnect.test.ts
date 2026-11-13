/**
 * Regression test for the stale-MQTT-broker bug (67 → 61 incident).
 *
 * When the user saves a new Gateway address — SplashScreen timeout view
 * or the Settings page — the frontend must push BOTH the HTTP config
 * (`set_gateway_config`) AND re-run `connect_mqtt`:
 *
 *  - `set_gateway_config` alone only re-points HTTP commands.
 *  - The MQTT client keeps publishing to the broker it was created for.
 *    `connect_mqtt` is what lets the Rust side compare the configured
 *    broker against the live client's endpoint and rebuild the
 *    connection on a mismatch.
 *
 * Before the fix, a remote address change left the MQTT client attached
 * to the OLD Gateway's broker: every chat message / create_session was
 * published into the void with no visible error until an app restart.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { useSettingsStore } from "./settingsStore";

vi.mock("@tauri-apps/api/core", () => ({
    invoke: vi.fn(async () => ({})),
}));

const NEW_URL = "http://192.168.3.61:19876";

/** Commands passed to `invoke`, in call order (after clearing history). */
function invokedCommands(): string[] {
    return vi.mocked(invoke).mock.calls.map(([cmd]) => String(cmd));
}

describe("settingsStore gateway config change", () => {
    beforeEach(() => {
        vi.clearAllMocks();
        // The mode is derived from the URL on every write, so seeding the
        // URL is enough — there is no independent mode knob to keep in sync.
        useSettingsStore.setState({ gatewayUrl: NEW_URL, gatewayMode: "remote" });
    });

    it("re-runs connect_mqtt after setGatewayUrl so Rust rebuilds the MQTT connection", async () => {
        useSettingsStore.getState().setGatewayUrl("http://192.168.3.67:19876");
        await vi.waitFor(() => {
            expect(invokedCommands()).toEqual(["set_gateway_config", "connect_mqtt"]);
        });
        expect(invoke).toHaveBeenCalledWith("set_gateway_config", {
            config: { mode: "remote", url: "http://192.168.3.67:19876" },
        });
    });

    it("does not fail when connect_mqtt rejects (Rust side not booted yet)", async () => {
        vi.mocked(invoke).mockImplementation(async (cmd: string) => {
            if (cmd === "connect_mqtt") throw new Error("not booted");
            return {};
        });
        useSettingsStore.getState().setGatewayUrl("http://192.168.3.99:19876");
        await vi.waitFor(() => {
            expect(invokedCommands()).toEqual(["set_gateway_config", "connect_mqtt"]);
        });
        // The rejection is swallowed — the address change itself must
        // never be blocked by a transient MQTT failure.
        expect(useSettingsStore.getState().gatewayUrl).toBe("http://192.168.3.99:19876");
    });
});
