/**
 * Agent Hub catalog logic (PRD docs/prd/zh/prd-agent-hub-install-dialog.md).
 *
 * Pinned rules:
 *   - REQ-010 search: case-insensitive, multi-word AND, and it reaches the
 *     capability keys — the published schema has no `capability_keys`.
 *   - REQ-007 risk: the Hub's own `high_risk_permissions` wins; the local set
 *     is only the fallback for entries that omit it.
 *   - The avatar path must not be able to point off the Hub origin.
 *   - The session cache must coalesce concurrent callers into ONE request and
 *     must not serve a cached catalog to a caller that asked to force-refresh.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import { invoke } from "@tauri-apps/api/core";
import {
  countInstancesOnNode,
  fetchHubIndex,
  formatSizeBytes,
  highRiskItemsOf,
  hubAssetUrl,
  hubAvatarInitialOf,
  hubChipsOf,
  hubDeprecationOf,
  resetHubIndexCache,
  searchHubAgents,
  type HubAgent,
} from "./agentHub";

function agent(overrides: Partial<HubAgent> = {}): HubAgent {
  return {
    agent_id: "com.acowork.test",
    version: "1.0.0",
    name: "test-agent",
    display_name: "Test Agent",
    role: "QA",
    description: "Verifies things",
    labels: ["quality"],
    capabilities: [{ key: "case-design", description: "", skills: [] }],
    skills: ["regression"],
    avatar: null,
    permissions: ["Read"],
    high_risk_permissions: [],
    package: { file: "downloads/com.acowork.test.agent", size_bytes: 2048, sha256: "ab" },
    signature: { status: "official-channel" },
    deprecated: null,
    ...overrides,
  };
}

describe("searchHubAgents", () => {
  const catalog = [
    agent({ agent_id: "a", display_name: "Software Architect", description: "designs systems" }),
    agent({ agent_id: "b", display_name: "Translator", role: "i18n", description: "converts text" }),
  ];

  it("is case-insensitive", () => {
    expect(searchHubAgents(catalog, "SOFTWARE")).toHaveLength(1);
    expect(searchHubAgents(catalog, "software architect")).toHaveLength(1);
  });

  it("treats multiple words as AND, in any order", () => {
    expect(searchHubAgents(catalog, "architect designs")).toHaveLength(1);
    expect(searchHubAgents(catalog, "designs architect")).toHaveLength(1);
    expect(searchHubAgents(catalog, "architect translator")).toHaveLength(0);
  });

  it("matches role, capability key and skills, not just names", () => {
    expect(searchHubAgents(catalog, "i18n")).toHaveLength(1);
    const withCapabilities = [
      agent({ capabilities: [{ key: "risk-analysis", description: "", skills: [] }], skills: ["audit"] }),
    ];
    expect(searchHubAgents(withCapabilities, "risk-analysis")).toHaveLength(1);
    expect(searchHubAgents(withCapabilities, "audit")).toHaveLength(1);
  });

  it("returns the whole list for a blank query", () => {
    expect(searchHubAgents(catalog, "   ")).toHaveLength(2);
  });
});

describe("hubAssetUrl", () => {
  it("joins a site-relative path onto the catalog's own root", () => {
    expect(hubAssetUrl("https://hub.acowork.ai", "avatars/a.png")).toBe(
      "https://hub.acowork.ai/avatars/a.png",
    );
    expect(hubAssetUrl("https://hub.acowork.ai/", "/avatars/a.png")).toBe(
      "https://hub.acowork.ai/avatars/a.png",
    );
  });

  it("refuses traversal and protocol-relative hosts", () => {
    expect(hubAssetUrl("https://hub.acowork.ai", "../evil.png")).toBeNull();
    expect(hubAssetUrl("https://hub.acowork.ai", "//evil.example/a.png")).toBeNull();
    expect(hubAssetUrl("https://hub.acowork.ai", "avatars\\..\\a.png")).toBeNull();
    // Only http(s) may be passed through absolute — never file:/data:.
    expect(hubAssetUrl("https://hub.acowork.ai", "file:///etc/passwd")).toBeNull();
    expect(hubAssetUrl("https://hub.acowork.ai", "https://cdn.example/a.png")).toBe(
      "https://cdn.example/a.png",
    );
    expect(hubAssetUrl("https://hub.acowork.ai", "")).toBeNull();
    expect(hubAssetUrl("https://hub.acowork.ai", null)).toBeNull();
  });
});

describe("high risk permissions", () => {
  it("prefers the Hub's own classification", () => {
    const a = agent({ permissions: ["Shell", "Read"], high_risk_permissions: ["Network"] });
    expect(highRiskItemsOf(a)).toEqual(["Network"]);
  });

  it("falls back to the known-risky set when the Hub omits it", () => {
    const a = agent({ permissions: ["Shell", "FilesystemWrite", "Read"], high_risk_permissions: [] });
    expect(highRiskItemsOf(a).sort()).toEqual(["FilesystemWrite", "Shell"]);
  });
});

describe("card rendering rules", () => {
  it("caps chips at 4 and prefers labels over capability keys", () => {
    const a = agent({
      labels: ["alpha", "beta"],
      capabilities: [
        { key: "cap1", description: "", skills: [] },
        { key: "cap2", description: "", skills: [] },
        { key: "cap3", description: "", skills: [] },
      ],
      skills: ["skill-1"],
    });
    expect(hubChipsOf(a)).toEqual(["alpha", "beta", "cap1", "cap2"]);
  });

  it("de-duplicates chips case-insensitively", () => {
    expect(
      hubChipsOf(agent({ labels: ["Ops"], capabilities: [], skills: ["ops", "extra"] })),
    ).toEqual(["Ops", "extra"]);
  });

  it("takes a unicode-safe first letter for the avatar fallback", () => {
    expect(hubAvatarInitialOf(agent({ display_name: "  架构师" }))).toBe("架");
    expect(hubAvatarInitialOf(agent({ display_name: "", name: "", agent_id: "com.x.y" }))).toBe("C");
  });

  it("reads the deprecation marker in either shape", () => {
    expect(hubDeprecationOf(agent({ deprecated: null })).flagged).toBe(false);
    expect(hubDeprecationOf(agent({ deprecated: true })).flagged).toBe(true);
    expect(hubDeprecationOf(agent({ deprecated: { reason: "superseded" } })).reason).toBe("superseded");
    expect(hubDeprecationOf(agent({ deprecated: "moved to x" })).reason).toBe("moved to x");
  });
});

describe("formatSizeBytes", () => {
  it("scales and rejects nonsense", () => {
    expect(formatSizeBytes(512)).toBe("512 B");
    expect(formatSizeBytes(2048)).toBe("2.0 KB");
    expect(formatSizeBytes(5 * 1024 * 1024)).toBe("5.0 MB");
    expect(formatSizeBytes(-1)).toBe("—");
    expect(formatSizeBytes(Number.NaN)).toBe("—");
  });
});

describe("countInstancesOnNode", () => {
  const entries = [
    { agent_id: "a", node_id: "n1" },
    { agent_id: "a", node_id: "n1" },
    { agent_id: "a", node_id: "n2" },
  ];

  it("counts per package AND per node", () => {
    expect(countInstancesOnNode(entries, "a", "n1")).toBe(2);
    expect(countInstancesOnNode(entries, "a", "n2")).toBe(1);
    expect(countInstancesOnNode(entries, "b", "n1")).toBe(0);
  });

  it("says nothing when no target node is resolved", () => {
    expect(countInstancesOnNode(entries, "a", null)).toBe(0);
  });
});

describe("fetchHubIndex caching", () => {
  const INDEX = { schema_version: 2, base_url: "https://hub.acowork.ai", agents: [agent()] };

  beforeEach(() => {
    resetHubIndexCache();
    vi.mocked(invoke).mockReset();
  });

  it("serves a cached catalog without a second request", async () => {
    vi.mocked(invoke).mockResolvedValue(INDEX);
    await fetchHubIndex();
    await fetchHubIndex();
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it("coalesces concurrent callers into one request", async () => {
    vi.mocked(invoke).mockResolvedValue(INDEX);
    await Promise.all([fetchHubIndex(), fetchHubIndex(), fetchHubIndex()]);
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it("re-requests on a forced refresh (the retry button)", async () => {
    vi.mocked(invoke).mockResolvedValue(INDEX);
    await fetchHubIndex();
    await fetchHubIndex({ force: true });
    expect(invoke).toHaveBeenCalledTimes(2);
  });

  it("surfaces a failed fetch instead of an empty catalog", async () => {
    vi.mocked(invoke).mockRejectedValue(new Error("content-type text/html"));
    await expect(fetchHubIndex()).rejects.toThrow("content-type");
    // A failure is not cached: the next attempt goes out again.
    vi.mocked(invoke).mockResolvedValue(INDEX);
    await expect(fetchHubIndex()).resolves.toBeTruthy();
    expect(invoke).toHaveBeenCalledTimes(2);
  });
});
