// ADR-104: the Create wizard's data model — mapping and gating.
//
// These tests exist because the wizard's *only* job is to produce one correct
// `CreateAgentDraft`. Everything visible in the dialog (step order, spinners,
// copy) can be wrong and be noticed; a mapping bug is silent — it produces a
// manifest that looks fine in the preview and is missing a table, or contains
// a `0` where the author meant "no opinion".
//
// So the assertions are on the wire shape:
//
//   - absent ≠ empty (`[config]` and `[[mcp_servers]]` are omitted, not blank)
//   - "" ≠ 0 (a cleared temperature must not become `temperature = 0`)
//   - integer fields never carry a fraction (serde `u64` would 400)
//   - a half-typed blank capability row is dropped, not sent
//
// Gating is tested as data too: `stepIssues` is the predicate that decides
// whether a broken draft can reach Rust at all.

import { describe, it, expect } from "vitest";
import {
  EMPTY_CONFIG_INPUT,
  EMPTY_WIZARD_STATE,
  PLATFORM_HOSTED_MCP,
  configFromInput,
  mcpServersFromPicks,
  stepIssues,
  toCreateAgentDraft,
  undeclaredToolDeps,
  type CapabilityInput,
  type McpPick,
  type WizardState,
} from "./createWizardDraft";

function state(over: Partial<WizardState> = {}): WizardState {
  return {
    ...EMPTY_WIZARD_STATE,
    agentId: "com.example.agent",
    name: "Agent",
    ...over,
  };
}

function cap(over: Partial<CapabilityInput> = {}): CapabilityInput {
  return {
    uid: "c1",
    action: "requirements_analysis",
    description: "Analyze requirements",
    skills: [],
    inputSchema: "",
    outputSchema: "",
    ...over,
  };
}

function pick(over: Partial<McpPick> = {}): McpPick {
  return {
    kind: "preset",
    id: "brave-search",
    why: "",
    required: false,
    requiredSecrets: [],
    label: "Brave Search",
    ...over,
  };
}

describe("configFromInput", () => {
  it("omits the table when every field is empty", () => {
    expect(configFromInput(EMPTY_CONFIG_INPUT)).toBeUndefined();
  });

  it("keeps 0 as a value for temperature but not for integer fields", () => {
    const out = configFromInput({ ...EMPTY_CONFIG_INPUT, temperature: "0" });
    // `temperature = 0.0` is legal; `context_window = 0` is rejected by core
    // (ADR-074 §6 — 0 is not a "no limit" sentinel there).
    expect(out).toEqual({ temperature: 0 });
  });

  it("drops an out-of-range value instead of writing it", () => {
    // The wizard refuses to advance past the config step on a range error, so
    // this only happens for a half-typed value in a render-time preview call.
    const out = configFromInput({ ...EMPTY_CONFIG_INPUT, temperature: "9" });
    expect(out).toBeUndefined();
  });

  it("never sends a fractional integer", () => {
    const out = configFromInput({
      ...EMPTY_CONFIG_INPUT,
      context_window: "1.5",
      max_output_tokens: "1000",
    });
    expect(out).toEqual({ max_output_tokens: 1000 });
  });

  it("carries every field when all are filled", () => {
    const out = configFromInput({
      temperature: "0.5",
      context_window: "128000",
      max_output_tokens: "4096",
      max_iterations: "40",
      compression_ratio_threshold: "0.9",
    });
    expect(out).toEqual({
      temperature: 0.5,
      context_window: 128000,
      max_output_tokens: 4096,
      max_iterations: 40,
      compression_ratio_threshold: 0.9,
    });
  });
});

describe("mcpServersFromPicks", () => {
  it("emits preset and hosted forms without mixing them", () => {
    const decls = mcpServersFromPicks([
      pick(),
      pick({ kind: "hosted", id: "pm", label: "PM" }),
    ]);
    expect(decls[0]).toEqual({
      preset: "brave-search",
      required_secrets: [],
      tools: [],
      required: false,
    });
    expect(decls[1]).toEqual({ hosted: "pm", required_secrets: [], tools: [], required: false });
    // Mutual exclusion: a declaration may carry exactly one identity form.
    expect(decls[0].hosted).toBeUndefined();
    expect(decls[1].preset).toBeUndefined();
  });

  it("carries required_secrets from the registry, not from the user", () => {
    const decls = mcpServersFromPicks([pick({ requiredSecrets: ["BRAVE_API_KEY"] })]);
    expect(decls[0].required_secrets).toEqual(["BRAVE_API_KEY"]);
  });

  it("omits an empty why instead of writing why = \"\"", () => {
    const decls = mcpServersFromPicks([pick({ why: "   " })]);
    expect("why" in decls[0]).toBe(false);
    expect(mcpServersFromPicks([pick({ why: "web search" })])[0].why).toBe("web search");
  });

  it("always leaves tools empty — the server is not connected during authoring", () => {
    expect(mcpServersFromPicks([pick()])[0].tools).toEqual([]);
  });

  it("yields no declarations for no picks", () => {
    expect(mcpServersFromPicks([])).toEqual([]);
  });
});

describe("toCreateAgentDraft", () => {
  it("omits blank optional scalars rather than sending empty strings", () => {
    const draft = toCreateAgentDraft(state({ version: "0.1.0" }));
    expect(draft.display_name).toBeUndefined();
    expect(draft.role).toBeUndefined();
    expect(draft.author).toBeUndefined();
    expect(draft.description).toBeUndefined();
    expect(draft.version).toBe("0.1.0");
  });

  it("trims identity fields", () => {
    const draft = toCreateAgentDraft(state({ agentId: "  com.example.a  ", name: " X " }));
    expect(draft.agent_id).toBe("com.example.a");
    expect(draft.name).toBe("X");
  });

  it("drops a capability row whose action is still blank", () => {
    const draft = toCreateAgentDraft(
      state({ capabilities: [cap(), cap({ uid: "c2", action: "  " })] }),
    );
    expect(draft.capabilities).toHaveLength(1);
    expect(draft.capabilities[0].action).toBe("requirements_analysis");
  });

  it("parses the advanced JSON cells and omits empty ones", () => {
    const draft = toCreateAgentDraft(
      state({
        capabilities: [
          cap({ inputSchema: '{"type":"object"}', outputSchema: "" }),
          cap({ uid: "c3", action: "b", inputSchema: "not json" }),
        ],
      }),
    );
    expect(draft.capabilities[0].input_schema).toEqual({ type: "object" });
    expect("output_schema" in draft.capabilities[0]).toBe(false);
    // Unparsable is dropped here; step gating is what refuses it in the UI.
    expect("input_schema" in draft.capabilities[1]).toBe(false);
  });

  it("forwards skill zip bytes positionally, matching the manifest build order", () => {
    const draft = toCreateAgentDraft(
      state({
        skills: [
          { name: "a", description: "", toolDeps: [], bytes: [1, 2] },
          { name: "b", description: "", toolDeps: [], bytes: [3] },
        ],
      }),
    );
    expect(draft.skill_files).toEqual([[1, 2], [3]]);
  });

  it("produces a draft with no config / no mcp when the user declared none", () => {
    const draft = toCreateAgentDraft(state());
    expect(draft.config).toBeUndefined();
    expect(draft.mcp_servers).toEqual([]);
    expect(draft.labels).toEqual([]);
  });
});

describe("stepIssues — basic", () => {
  it("accepts a reverse-domain id", () => {
    expect(stepIssues("basic", state())).toEqual([]);
  });

  it("rejects a bare name (no dot)", () => {
    expect(stepIssues("basic", state({ agentId: "myagent" }))).toContain("agentIdInvalid");
  });

  it("rejects a segment starting with a digit or containing an underscore", () => {
    expect(stepIssues("basic", state({ agentId: "com.1example.a" }))).toContain("agentIdInvalid");
    expect(stepIssues("basic", state({ agentId: "com.ex_ample.a" }))).toContain("agentIdInvalid");
  });

  it("requires a display name", () => {
    expect(stepIssues("basic", state({ name: "   " }))).toContain("nameMissing");
  });

  it("refuses more labels than the manifest allows", () => {
    const labels = Array.from({ length: 13 }, (_, i) => `label-${i}`);
    expect(stepIssues("basic", state({ labels }))).toContain("tooManyLabels");
    expect(stepIssues("basic", state({ labels: labels.slice(0, 12) }))).toEqual([]);
  });
});

describe("stepIssues — config", () => {
  it("accepts all-empty (no opinion)", () => {
    expect(stepIssues("config", state())).toEqual([]);
  });

  it("rejects out-of-range values", () => {
    expect(stepIssues("config", state({ config: { ...EMPTY_CONFIG_INPUT, temperature: "2.5" } })))
      .toContain("configRange");
    expect(
      stepIssues("config", state({ config: { ...EMPTY_CONFIG_INPUT, compression_ratio_threshold: "1" } })),
    ).toContain("configRange");
    expect(stepIssues("config", state({ config: { ...EMPTY_CONFIG_INPUT, context_window: "0" } })))
      .toContain("configRange");
  });

  it("rejects a fractional integer field", () => {
    expect(
      stepIssues("config", state({ config: { ...EMPTY_CONFIG_INPUT, max_iterations: "2.5" } })),
    ).toContain("configInteger");
  });
});

describe("stepIssues — tools", () => {
  it("is always satisfiable (every choice there is optional)", () => {
    expect(stepIssues("tools", state())).toEqual([]);
  });
});

describe("stepIssues — skills and capabilities", () => {
  it("accepts an empty capability list", () => {
    expect(stepIssues("skills", state())).toEqual([]);
  });

  it("rejects an action outside [a-z0-9_]", () => {
    expect(stepIssues("skills", state({ capabilities: [cap({ action: "Bad-Action" })] })))
      .toContain("capabilityActionInvalid");
  });

  it("rejects two rows with the same action", () => {
    expect(
      stepIssues("skills", state({ capabilities: [cap(), cap({ uid: "c2" })] })),
    ).toContain("capabilityActionDuplicate");
  });

  it("rejects unparsable JSON in either schema cell", () => {
    expect(
      stepIssues("skills", state({ capabilities: [cap({ outputSchema: "{oops" })] })),
    ).toContain("capabilitySchemaInvalid");
    expect(
      stepIssues("skills", state({ capabilities: [cap({ inputSchema: "{oops" })] })),
    ).toContain("capabilitySchemaInvalid");
  });

  it("ignores the schema cells when they are left empty", () => {
    expect(stepIssues("skills", state({ capabilities: [cap()] }))).toEqual([]);
  });
});

describe("undeclaredToolDeps — advisory reachability", () => {
  it("flags a dependency nothing declares", () => {
    const out = undeclaredToolDeps(
      state({ skills: [{ name: "risk", description: "", toolDeps: ["brave_web_search"], bytes: [] }] }),
    );
    expect(out).toEqual([{ skill: "risk", tool: "brave_web_search" }]);
  });

  it("does not flag a builtin tool declared in step 3", () => {
    const out = undeclaredToolDeps(
      state({
        tools: ["file_read"],
        skills: [{ name: "s", description: "", toolDeps: ["file_read"], bytes: [] }],
      }),
    );
    expect(out).toEqual([]);
  });

  it("treats a platform-hosted MCP tool as reachable without a declaration", () => {
    for (const hosted of PLATFORM_HOSTED_MCP) {
      const out = undeclaredToolDeps(
        state({
          skills: [
            { name: "s", description: "", toolDeps: [`mcp_${hosted}__do_thing`], bytes: [] },
          ],
        }),
      );
      expect(out, `${hosted} should be reachable`).toEqual([]);
    }
  });

  it("treats a recommended server's tools as reachable", () => {
    const out = undeclaredToolDeps(
      state({
        mcp: [pick()],
        skills: [{ name: "s", description: "", toolDeps: ["mcp_brave-search__search"], bytes: [] }],
      }),
    );
    expect(out).toEqual([]);
  });

  it("still flags a tool of a server nobody declared", () => {
    const out = undeclaredToolDeps(
      state({ skills: [{ name: "s", description: "", toolDeps: ["mcp_other__search"], bytes: [] }] }),
    );
    expect(out).toEqual([{ skill: "s", tool: "mcp_other__search" }]);
  });
});