/**
 * User profile API client tests.
 *
 * Regression coverage for the backend response contract:
 * - `POST /api/users` answers with an `OperationAck` (ADR-059 §7.3)
 * - `PUT  /api/users/{user_id}` answers with `UserResponse { user, version }`
 * - `POST /api/users/{user_id}/activate` answers with `ActivateResponse`
 * - `GET  /api/nodes` answers snake_case `NodeResponse` (legacy camelCase normalized)
 * - LSP registry writes address the Node through the Gateway `lsp-admin`
 *   proxy, and a failed install reports its reason in `stderr` (ADR-091)
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
    createUser,
    updateUser,
    activateUser,
    fetchActiveUser,
    verifyAgentHealth,
    fetchNodes,
    lspAdminUrlFromRelayUrl,
    runLspInstall,
} from "./gateway-api";
import type { BackendUserProfile } from "./types";
import { usableNodesOf } from "../components/nodes/nodeAvailability";

// ── Fixtures ─────────────────────────────────────────────────────────────

const PROFILE: BackendUserProfile = {
    user_id: "e8397df5-0000-4000-8000-000000000001",
    display_name: "大鱼",
    language: "zh-CN",
    timezone: "Asia/Shanghai",
    city: "上海",
    created_at: "2026-08-01T00:00:00Z",
    updated_at: "2026-08-27T00:00:00Z",
    is_active: true,
};

const ENVELOPE = { user: PROFILE, version: 7 };

/** URLs + methods captured from the mocked global fetch. */
const calls: Array<{ url: string; init?: RequestInit }> = [];

function mockFetchOnce(body: unknown, status = 200) {
    vi.stubGlobal(
        "fetch",
        vi.fn((url: string | URL, init?: RequestInit) => {
            calls.push({ url: String(url), init });
            return Promise.resolve({
                ok: status >= 200 && status < 300,
                status,
                json: () => Promise.resolve(body),
            } as Response);
        }),
    );
}

// ── Tests ────────────────────────────────────────────────────────────────

beforeEach(() => {
    calls.length = 0;
});

afterEach(() => {
    vi.unstubAllGlobals();
});

describe("updateUser response unwrapping", () => {
    it("unwraps { user, version } so the profile is returned directly", async () => {
        mockFetchOnce(ENVELOPE);

        const result = await updateUser(PROFILE.user_id, { city: "上海" }, "http://gw");

        expect(result).toEqual(PROFILE);
        expect(result.user_id).toBe(PROFILE.user_id);
        // The envelope's version must NOT leak into the profile.
        expect((result as unknown as { version?: unknown }).version).toBeUndefined();
        expect(calls[0].url).toBe(`http://gw/api/users/${PROFILE.user_id}`);
    });

    it("throws on non-OK responses", async () => {
        mockFetchOnce({ error: "User not found" }, 404);

        await expect(updateUser("missing", { city: "x" }, "http://gw")).rejects.toThrow(
            /User not found/,
        );
    });
});

describe("chained update after a previous save (ProfileTab saveField pattern)", () => {
    it("uses the returned profile's user_id for the next PUT — never /users/undefined", async () => {
        // First save returns the envelope; the client must unwrap it so the
        // caller's cached profile keeps a valid user_id.
        mockFetchOnce(ENVELOPE);

        const saved = await updateUser(PROFILE.user_id, { city: "上海" }, "http://gw");

        // Second save: same flow as ProfileTab.saveField(user_id, ...).
        mockFetchOnce(ENVELOPE);
        await updateUser(saved.user_id, { language: "zh-CN" }, "http://gw");

        expect(calls).toHaveLength(2);
        // Regression: the old code stored the envelope, user_id became
        // undefined, and this PUT went to /api/users/undefined → 404.
        expect(calls[1].url).not.toContain("undefined");
        expect(calls[1].url).toBe(`http://gw/api/users/${PROFILE.user_id}`);
    });
});

describe("createUser response shape (ADR-059 §7.3)", () => {
    it("returns OperationAck as-is — not the unwrapped profile", async () => {
        const ack = {
            operation_id: "op-1234",
            state: "committed",
            resource_version: 42,
        };
        mockFetchOnce(ack);

        const result = await createUser(
            { display_name: "大鱼", language: "zh-CN", timezone: "Asia/Shanghai" },
            "http://gw",
        );

        // OperationAck must be returned verbatim, with no unwrapping.
        expect(result).toEqual(ack);
        expect(result.operation_id).toBe("op-1234");
        expect(result.state).toBe("committed");
        expect((result as unknown as { user?: unknown }).user).toBeUndefined();
        expect(calls[0].url).toBe("http://gw/api/users");
        expect(calls[0].init?.method).toBe("POST");
    });
});

describe("activateUser response shape", () => {
    it("returns ActivateResponse { active_user_id, version } as-is", async () => {
        mockFetchOnce({ active_user_id: PROFILE.user_id, version: 9 });

        const result = await activateUser(PROFILE.user_id, "http://gw");

        expect(result).toEqual({ active_user_id: PROFILE.user_id, version: 9 });
        expect(calls[0].url).toBe(`http://gw/api/users/${PROFILE.user_id}/activate`);
        expect(calls[0].init?.method).toBe("POST");
    });
});

describe("fetchActiveUser", () => {
    it("picks the active user from the list response", async () => {
        mockFetchOnce({
            version: 3,
            users: [
                { ...PROFILE, user_id: "other", is_active: false },
                PROFILE,
            ],
        });

        const active = await fetchActiveUser("http://gw");

        expect(active?.user_id).toBe(PROFILE.user_id);
    });
});

// ── verifyAgentHealth (distributed liveness double-check) ────────────────
//
// Regression: 2026-09-02 09:12 incident. MQTT disconnection during
// system sleep marked agents as offline and the desktop rendered them
// as sleeping (zzz). The fix probes the Runtime's `/health` endpoint
// through the Gateway reverse-proxy on every MQTT `online=false` event;
// a 2xx answer overrides back to online.

describe("verifyAgentHealth (HTTP double-check on MQTT disconnect)", () => {
    it("returns true on a 2xx response and targets the Gateway health proxy", async () => {
        mockFetchOnce({ status: "ok" }, 200);
        const alive = await verifyAgentHealth("com.acowork.architect", 3000, "http://gw");
        expect(alive).toBe(true);
        expect(calls).toHaveLength(1);
        expect(calls[0].url).toBe("http://gw/api/agents/com.acowork.architect/health");
        expect(calls[0].init?.method).toBeUndefined(); // GET is the fetch default
    });

    it("returns false on a 503 response (Runtime not registered)", async () => {
        mockFetchOnce({ error: "agent_not_running" }, 503);
        const alive = await verifyAgentHealth("com.acowork.never-registered", 3000, "http://gw");
        expect(alive).toBe(false);
    });

    it("returns false on a 502 BAD_GATEWAY (Runtime died after registry populated)", async () => {
        mockFetchOnce({ error: "upstream unreachable" }, 502);
        const alive = await verifyAgentHealth("com.acowork.dead", 3000, "http://gw");
        expect(alive).toBe(false);
    });

    it("returns false on a 404 (route not configured — would be a Gateway bug)", async () => {
        mockFetchOnce({ error: "not found" }, 404);
        const alive = await verifyAgentHealth("com.acowork.architect", 3000, "http://gw");
        expect(alive).toBe(false);
    });

    it("returns false when fetch itself throws (network unreachable, DNS, etc.)", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(() => Promise.reject(new Error("ECONNREFUSED"))),
        );
        const alive = await verifyAgentHealth("com.acowork.architect", 3000, "http://gw");
        expect(alive).toBe(false);
    });

    it("returns false when the request is aborted by the timeout (Runtime slow)", async () => {
        // fetch returns a promise that never resolves before the AbortController
        // fires — the function must catch that and return false, never hang.
        vi.stubGlobal(
            "fetch",
            vi.fn((_url: string | URL, init?: RequestInit) => {
                return new Promise((_resolve, reject) => {
                    const signal = init?.signal as AbortSignal | undefined;
                    signal?.addEventListener("abort", () => {
                        reject(new DOMException("Aborted", "AbortError"));
                    });
                });
            }),
        );
        // Use a short timeout so the test doesn't actually wait 3s.
        const alive = await verifyAgentHealth("com.acowork.architect", 50, "http://gw");
        expect(alive).toBe(false);
    });

    it("URL-encodes the agent_id so reverse-domain dots don't confuse the path", async () => {
        mockFetchOnce({ status: "ok" }, 200);
        await verifyAgentHealth("com.acowork.senior-engineer", 3000, "http://gw");
        expect(calls[0].url).toBe(
            "http://gw/api/agents/com.acowork.senior-engineer/health",
        );
    });
});

// ── fetchNodes field normalization (remote-mode node grouping) ───────────
//
// Regression: Gateway builds from ADR-055 Phases 1–3 serialized
// `NodeResponse` in camelCase while the desktop consumed snake_case
// (`NodeInfo`). Every `node_id` parsed as `undefined`, the node never
// matched its agent bucket, and the remote sidebar collapsed into one
// gray "unknown" group — while the node was actually online. The
// normalize layer accepts both field-name shapes so a desktop build
// works against old and new Gateways alike.

describe("fetchNodes field normalization", () => {
    it("reads the snake_case contract (new Gateways) verbatim", async () => {
        mockFetchOnce([
            {
                node_id: "nicholas-pc",
                online: true,
                hostname: "NICHOLAS-PC",
                os: "windows",
                arch: "x86_64",
                node_version: "0.1.0",
                agent_count: 2,
                max_agents: 16,
                capabilities: ["control_plane"],
                http_endpoint: "http://127.0.0.1:19900",
            },
        ]);

        const nodes = await fetchNodes("http://gw");

        expect(nodes).toHaveLength(1);
        expect(nodes[0].node_id).toBe("nicholas-pc");
        expect(nodes[0].online).toBe(true);
        expect(nodes[0].agent_count).toBe(2);
        expect(nodes[0].http_endpoint).toBe("http://127.0.0.1:19900");
        expect(calls[0].url).toBe("http://gw/api/nodes");
    });

    it("normalizes legacy camelCase payloads (ADR-055 Phase 1–3 Gateways)", async () => {
        mockFetchOnce([
            {
                nodeId: "nicholas-pc",
                online: true,
                hostname: "NICHOLAS-PC",
                os: "windows",
                arch: "x86_64",
                nodeVersion: "0.1.0",
                agentCount: 2,
                maxAgents: 16,
                capabilities: ["control_plane"],
                httpEndpoint: "http://127.0.0.1:19900",
            },
        ]);

        const nodes = await fetchNodes("http://gw");

        // The regression: node_id was undefined → partitionAgentsByNode
        // could never match this node → gray "unknown" group in the sidebar.
        expect(nodes[0].node_id).toBe("nicholas-pc");
        expect(nodes[0].online).toBe(true);
        expect(nodes[0].node_version).toBe("0.1.0");
        expect(nodes[0].agent_count).toBe(2);
        expect(nodes[0].max_agents).toBe(16);
        expect(nodes[0].http_endpoint).toBe("http://127.0.0.1:19900");
    });

    it("carries lsp_endpoint so the harness panel can offer a node picker", async () => {
        // ADR-055 §6.7: the panel needs every node's relay address, not just
        // the selected agent's, or the node dropdown has nothing to list.
        mockFetchOnce([
            {
                node_id: "gpu-1",
                online: true,
                node_name: "gpu-box",
                lsp_endpoint: "http://10.0.0.2:19878",
            },
            {
                node_id: "laptop",
                online: true,
                node_name: "laptop",
                // Relay down / not yet published — the picker must be able to
                // tell "no relay" apart from "not a manager" by omission.
            },
        ]);

        const nodes = await fetchNodes("http://gw");

        expect(nodes[0].lsp_endpoint).toBe("http://10.0.0.2:19878");
        expect(nodes[1].lsp_endpoint).toBeUndefined();
    });

    it("accepts the legacy camelCase lspEndpoint spelling", async () => {
        mockFetchOnce([
            { nodeId: "gpu-1", online: true, lspEndpoint: "http://10.0.0.2:19878" },
        ]);

        const nodes = await fetchNodes("http://gw");

        expect(nodes[0].lsp_endpoint).toBe("http://10.0.0.2:19878");
    });

    // ADR-113 D6: an older Gateway has no `control_ready` field. Absence
    // must stay `undefined` (not `false`), or `usableNodesOf()` filters out
    // every node and the UI is stuck on "no available node" forever.
    it("keeps control_ready undefined for pre-ADR-113 Gateways", async () => {
        mockFetchOnce([{ node_id: "legacy-1", online: true }]);
        const nodes = await fetchNodes("http://gw");

        expect(nodes[0].control_ready).toBeUndefined();
        expect(usableNodesOf(nodes).map((n) => n.node_id)).toEqual(["legacy-1"]);
    });

    it("maps an explicit control_ready through to the usable-node predicate", async () => {
        mockFetchOnce([
            { node_id: "ready", online: true, control_ready: true },
            { node_id: "pending", online: true, control_ready: false },
        ]);
        const nodes = await fetchNodes("http://gw");

        expect(nodes[0].control_ready).toBe(true);
        expect(nodes[1].control_ready).toBe(false);
        expect(usableNodesOf(nodes).map((n) => n.node_id)).toEqual(["ready"]);
    });
});

describe("runLspInstall failure reporting", () => {
    it("surfaces the script stderr on the Node's 500 (no `error` key)", async () => {
        // The Node's non-zero-exit answer carries no `error` key, so reading
        // only `data.error` degraded to "Install failed: 500" and hid the real
        // reason (missing JDK / no package manager).
        mockFetchOnce(
            {
                name: "java",
                success: false,
                installState: "failed",
                willRestart: false,
                exitCode: 1,
                stdout: "",
                stderr: "ERROR: jdtls requires Java 21+, but found Java 11.",
            },
            500,
        );

        await expect(
            runLspInstall("http://gw/api/nodes/n-1/lsp-admin", "java"),
        ).rejects.toThrow("ERROR: jdtls requires Java 21+, but found Java 11.");
        expect(calls[0].url).toBe("http://gw/api/nodes/n-1/lsp-admin/servers/java/install");
        expect(calls[0].init?.method).toBe("POST");
    });

    it("still prefers an explicit `error` field when present (404 path)", async () => {
        mockFetchOnce({ error: "Install script file 'java.ps1' not found", code: 404 }, 404);

        await expect(
            runLspInstall("http://gw/api/nodes/n-1/lsp-admin", "java"),
        ).rejects.toThrow("Install script file 'java.ps1' not found");
    });
});

describe("lspAdminUrlFromRelayUrl", () => {
    it("derives the registry base from the relay proxy URL", () => {
        // The panel only ever knows the relay address; a second endpoint
        // lookup would fail exactly when the relay is down — which is when
        // the registry is most needed (ADR-091 §5.5).
        expect(
            lspAdminUrlFromRelayUrl("http://localhost:19876/api/nodes/n-1/lsp"),
        ).toBe("http://localhost:19876/api/nodes/n-1/lsp-admin");
    });

    it("refuses a raw loopback relay address", () => {
        // Not a Gateway proxy path, so there is no caller identity attached
        // to the request and no permission gate in front of it.
        expect(lspAdminUrlFromRelayUrl("http://127.0.0.1:19878")).toBeNull();
    });
});
