/**
 * `applyAgentIdentity` — the store's write path for a backend-confirmed
 * instance identity (name / custom avatar / builtin icon).
 *
 * It exists because `PUT /api/agents/{id}/avatar-config` is answered by the
 * Runtime (the owner of `agent_config.json`), while the surfaces that paint an
 * avatar read `meta` from `GET /api/agents` — which the Gateway builds from an
 * ASYNCHRONOUS projection of that same file (Node inventory → MQTT →
 * `inventoryVersion` → `fetchAgents`). Writing the confirmation straight into
 * the row closes that window; the inventory doorbell's refetch then converges
 * on a value the row already holds.
 *
 * The second half is the "already holds" part: a converged write must not
 * produce a new object identity, or the sidebar and every transcript repaint
 * for nothing (AGENTS.md — identity-preserving store updates). Both halves are
 * pinned here, plus the field semantics non-avatar callers depend on.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => []) }));
vi.mock("../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
  setLevel: () => {},
  getLevel: () => "off" as const,
}));
vi.mock("../lib/profileStore", () => ({
  loadAllProfiles: () => ({}),
  loadProfile: () => null,
  saveProfile: () => {},
  DEFAULT_PROFILE: {
    language: "zh-CN",
    timezone: "Asia/Shanghai",
    model_override: null,
    reasoning_effort: null,
    auto_approve_tools: [],
    yolo_mode: false,
  },
}));

import { useAgentStore } from "./agentStore";
import type { AgentInfo } from "../lib/types";

const INSTANCE_ID = "bd68e6a7-7dd4-4ca7-82a2-e65745c4d198";
const PACKAGE_ID = "com.acowork.senior-engineer";

function seed(over: Partial<AgentInfo> = {}) {
  useAgentStore.setState({
    agents: {
      [INSTANCE_ID]: {
        meta: {
          instance_id: INSTANCE_ID,
          agent_id: PACKAGE_ID,
          name: "SSE.Kevin",
          version: "1.0.0",
          alive: true,
          avatar: "assets/avatar-02.png",
          ...over,
        } as AgentInfo,
        profile: {} as never,
        sessions: [],
      },
    } as never,
    selectedAgentId: INSTANCE_ID,
  });
  return useAgentStore.getState().agents[INSTANCE_ID];
}

function metaOf() {
  return useAgentStore.getState().agents[INSTANCE_ID].meta;
}

beforeEach(() => {
  useAgentStore.setState({ agents: {}, selectedAgentId: null, loading: false, error: null });
});

describe("applyAgentIdentity — confirmed identity lands on the row", () => {
  it("replaces the custom avatar with the picked builtin icon", () => {
    seed();

    useAgentStore.getState().applyAgentIdentity(INSTANCE_ID, {
      name: "SSE.Kevin",
      avatar: undefined,
      builtin_avatar: "icon-09",
    });

    expect(metaOf().builtin_avatar).toBe("icon-09");
    // The slots are mutually exclusive upstream. Keeping the old custom path
    // would make it win the render priority (custom > builtin) again.
    expect(metaOf().avatar).toBeUndefined();
  });

  it("carries a rename without touching either avatar slot", () => {
    seed({ builtin_avatar: "icon-09" });

    useAgentStore.getState().applyAgentIdentity(INSTANCE_ID, { name: "Kevin" });

    expect(metaOf().name).toBe("Kevin");
    expect(metaOf().builtin_avatar).toBe("icon-09");
    expect(metaOf().avatar).toBe("assets/avatar-02.png");
  });

  it("resolves a package id to its instance row (ADR-073)", () => {
    seed();

    useAgentStore.getState().applyAgentIdentity(PACKAGE_ID, { builtin_avatar: "icon-03" });

    expect(metaOf().builtin_avatar).toBe("icon-03");
  });

  it("ignores an unknown agent instead of inventing a row", () => {
    seed();

    useAgentStore.getState().applyAgentIdentity("no-such-instance", { builtin_avatar: "icon-03" });

    expect(Object.keys(useAgentStore.getState().agents)).toEqual([INSTANCE_ID]);
    expect(metaOf().builtin_avatar).toBeUndefined();
  });
});

describe("applyAgentIdentity — identity preservation", () => {
  it("returns the previous state when the confirmed identity already matches", () => {
    seed({ avatar: undefined, builtin_avatar: "icon-09" });
    const agentsBefore = useAgentStore.getState().agents;
    const rowBefore = useAgentStore.getState().agents[INSTANCE_ID];

    useAgentStore.getState().applyAgentIdentity(INSTANCE_ID, {
      name: "SSE.Kevin",
      avatar: undefined,
      builtin_avatar: "icon-09",
    });

    // Same references: no subscriber repaints, no transcript row re-renders.
    expect(useAgentStore.getState().agents).toBe(agentsBefore);
    expect(useAgentStore.getState().agents[INSTANCE_ID]).toBe(rowBefore);
  });

  it("keeps every other row's identity when one row changes", () => {
    seed();
    useAgentStore.setState((state) => ({
      agents: {
        ...state.agents,
        "other-instance": {
          ...state.agents[INSTANCE_ID],
          meta: { ...state.agents[INSTANCE_ID].meta, instance_id: "other-instance" },
        },
      },
    }));
    const otherBefore = useAgentStore.getState().agents["other-instance"];

    useAgentStore.getState().applyAgentIdentity(INSTANCE_ID, { builtin_avatar: "icon-09" });

    expect(useAgentStore.getState().agents["other-instance"]).toBe(otherBefore);
  });
});
