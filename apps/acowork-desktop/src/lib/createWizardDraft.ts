// ADR-104: the Create Agent wizard's whole data model, as pure functions.
//
// ## Why this is not in the component
//
// The wizard edits eleven fields across five steps and then hands them to
// `create_agent` as ONE draft. Two things about that are worth testing on
// their own, without a DOM:
//
//   1. **Step gating** — "may the user leave this step" is the rule that
//      decides whether a broken draft ever reaches Rust. It is a pure
//      predicate over the state, so it belongs in a function.
//   2. **The wire mapping** — raw text inputs ("", "0.5") become typed
//      optional numbers (`null`, `0.5`), empty groups become *absent* tables
//      rather than empty ones. Getting "absent ≠ empty" wrong changes the
//      meaning of the manifest (an absent `[config]` is "no opinion"; an
//      empty one is silently the same, but an absent `[[mcp_servers]]` must
//      not become `[[]]`).
//
// The rules mirrored here are deliberately **stricter or equal** to
// `acowork-core`: `ManifestDraft::build` re-validates everything and is the
// authority, so a duplicate client rule can only ever reject an input Rust
// would also reject. The one exception is `undeclaredToolDeps`, which is an
// *advisory* mirror of `AgentManifest::validate_graph`'s warning half —
// documented there, and the preview step still shows Rust's own list.

import {
  CONFIG_LIMITS,
  configFieldError,
  isValidLabel,
  MAX_LABELS,
} from "./manifestRules";

/* -------------------------------------------------------------------------- */
/* Wire types — field names mirror the Rust structs verbatim                  */
/* -------------------------------------------------------------------------- */

/**
 * `core/acowork-core/src/manifest.rs::ManifestConfigDefaults`.
 *
 * Every field is optional because "the author has no opinion" is a real
 * state and must serialize to nothing at all.
 */
export interface ManifestConfigDefaultsWire {
  temperature?: number;
  context_window?: number;
  max_output_tokens?: number;
  max_iterations?: number;
  compression_ratio_threshold?: number;
}

/** `core/acowork-core/src/manifest.rs::McpServerDecl` (wizard subset). */
export interface McpServerDeclWire {
  /** Present only when it differs from the preset / hosted id. */
  name?: string;
  preset?: string;
  hosted?: string;
  required_secrets: string[];
  /** Always empty from the wizard: the server is not connected yet. */
  tools: string[];
  required: boolean;
  why?: string;
}

/** `core/acowork-core/src/manifest.rs::DraftCapability`. */
export interface DraftCapabilityWire {
  action: string;
  description: string;
  skills: string[];
  input_schema?: unknown;
  output_schema?: unknown;
}

/**
 * `apps/acowork-desktop/src-tauri/src/commands/create.rs::CreateAgentDraft`.
 *
 * `skill_files` sits at the top level next to the flattened manifest fields —
 * that is why `#[serde(flatten)]` is on the Rust side, and why this interface
 * is flat rather than `{ manifest: {...}, skill_files }`.
 */
export interface CreateAgentDraftWire {
  agent_id: string;
  name: string;
  display_name?: string;
  role?: string;
  version?: string;
  author?: string;
  description?: string;
  labels: string[];
  config?: ManifestConfigDefaultsWire;
  tools: string[];
  mcp_servers: McpServerDeclWire[];
  capabilities: DraftCapabilityWire[];
  skill_files: number[][];
}

/* -------------------------------------------------------------------------- */
/* Wizard state                                                               */
/* -------------------------------------------------------------------------- */

/** Raw text of the `[config]` step — kept as strings so "" ≠ 0. */
export interface ConfigInput {
  temperature: string;
  context_window: string;
  max_output_tokens: string;
  max_iterations: string;
  compression_ratio_threshold: string;
}

export const EMPTY_CONFIG_INPUT: ConfigInput = {
  temperature: "",
  context_window: "",
  max_output_tokens: "",
  max_iterations: "",
  compression_ratio_threshold: "",
};

/** One `[[mcp_servers]]` row the user ticked in step 3. */
export interface McpPick {
  /** `preset:<id>` or `hosted:<id>` — the radio axis is the declaration form. */
  kind: "preset" | "hosted";
  id: string;
  /** Author's reason, shown verbatim in the Tools panel later. */
  why: string;
  /** Soft requirement. Defaults OFF: a fresh package must not start with alerts. */
  required: boolean;
  /**
   * Secrets the server needs. Copied from the preset registry at pick time so
   * the manifest states it without the client having to re-resolve the preset
   * at read time.
   */
  requiredSecrets: string[];
  /** Display name from the registry, for the wizard's own summary list. */
  label: string;
}

/** One `[capabilities.<action>]` row. */
export interface CapabilityInput {
  /** Stable React key; the manifest key is `action`. */
  uid: string;
  action: string;
  description: string;
  /** Skill names this action is implemented by (ADR-104). */
  skills: string[];
  /** Raw JSON text from the advanced disclosure; "" means "not declared". */
  inputSchema: string;
  outputSchema: string;
}

/** One uploaded skill ZIP, after `parse_skill_zip_preview` answered. */
export interface SkillEntry {
  /** Parsed `SKILL.md` name — the directory name and the graph node. */
  name: string;
  description: string;
  /** Parsed `SKILL.md` `tool_deps` (ADR-104 reachability input). */
  toolDeps: string[];
  /** Raw ZIP bytes, forwarded to `create_agent`. */
  bytes: number[];
}

export interface WizardState {
  agentId: string;
  name: string;
  displayName: string;
  role: string;
  version: string;
  author: string;
  description: string;
  labels: string[];
  config: ConfigInput;
  tools: string[];
  mcp: McpPick[];
  capabilities: CapabilityInput[];
  skills: SkillEntry[];
}

export const EMPTY_WIZARD_STATE: WizardState = {
  agentId: "",
  name: "",
  displayName: "",
  role: "",
  version: "0.1.0",
  author: "",
  description: "",
  labels: [],
  config: { ...EMPTY_CONFIG_INPUT },
  tools: [],
  mcp: [],
  capabilities: [],
  skills: [],
};

/* -------------------------------------------------------------------------- */
/* Mapping: wizard state → wire draft                                         */
/* -------------------------------------------------------------------------- */

/** Trim, and turn "" into `undefined` so serde sees "absent", not `""`. */
function opt(value: string): string | undefined {
  const trimmed = value.trim();
  return trimmed === "" ? undefined : trimmed;
}

/** Integer-only `[config]` fields (serde `u64` / `u32` rejects `0.5`). */
const INTEGER_CONFIG_FIELDS = [
  "context_window",
  "max_output_tokens",
  "max_iterations",
] as const;

/**
 * Project the `[config]` step into `ManifestConfigDefaultsWire`.
 *
 * Returns `undefined` when every field is empty — the caller then omits the
 * table entirely. `configFieldError` guards each field, but this function is
 * also called during render (for the preview payload), so it must not throw
 * on a half-typed value; a value the user is still typing is simply skipped.
 */
export function configFromInput(input: ConfigInput): ManifestConfigDefaultsWire | undefined {
  const out: ManifestConfigDefaultsWire = {};

  if (configFieldError("temperature", input.temperature) === null) {
    const t = input.temperature.trim();
    if (t !== "") out.temperature = Number(t);
  }
  for (const field of INTEGER_CONFIG_FIELDS) {
    if (configFieldError(field, input[field]) === null) {
      const raw = input[field].trim();
      if (raw !== "") out[field] = Number(raw);
    }
  }
  if (configFieldError("compression_ratio_threshold", input.compression_ratio_threshold) === null) {
    const raw = input.compression_ratio_threshold.trim();
    if (raw !== "") out.compression_ratio_threshold = Number(raw);
  }

  return Object.keys(out).length === 0 ? undefined : out;
}

/**
 * Project the step-3 MCP picks into `[[mcp_servers]]` declarations.
 *
 * One declaration per pick, in registry order, and `tools` is always `[]`:
 * the server is not connected during authoring, so any tool name here would be
 * a guess the user could not verify. The whitelist is filled in later, from
 * the Tools panel, where `tools/list` is actually available.
 */
export function mcpServersFromPicks(picks: McpPick[]): McpServerDeclWire[] {
  return picks.map((pick) => {
    const decl: McpServerDeclWire = {
      required_secrets: [...pick.requiredSecrets],
      tools: [],
      required: pick.required,
    };
    if (pick.kind === "preset") decl.preset = pick.id;
    else decl.hosted = pick.id;
    const why = pick.why.trim();
    if (why) decl.why = why;
    return decl;
  });
}

/** Parse one advanced JSON cell; `undefined` for "" and for unparsable text. */
function parseSchemaCell(raw: string): unknown {
  const trimmed = raw.trim();
  if (!trimmed) return undefined;
  try {
    return JSON.parse(trimmed);
  } catch {
    return undefined;
  }
}

/**
 * Project capability rows into `[capabilities.*]`.
 *
 * Rows with an empty action are dropped rather than sent: the wizard keeps a
 * blank row on screen for the next entry, and sending it would make Rust
 * reject the whole draft for a row the user never filled.
 */
export function capabilitiesFromInput(caps: CapabilityInput[]): DraftCapabilityWire[] {
  return caps
    .filter((cap) => cap.action.trim() !== "")
    .map((cap) => {
      const out: DraftCapabilityWire = {
        action: cap.action.trim(),
        description: cap.description.trim(),
        skills: [...cap.skills],
      };
      const inputSchema = parseSchemaCell(cap.inputSchema);
      if (inputSchema !== undefined) out.input_schema = inputSchema;
      const outputSchema = parseSchemaCell(cap.outputSchema);
      if (outputSchema !== undefined) out.output_schema = outputSchema;
      return out;
    });
}

/** Project the whole wizard state into the single `create_agent` argument. */
export function toCreateAgentDraft(state: WizardState): CreateAgentDraftWire {
  const draft: CreateAgentDraftWire = {
    agent_id: state.agentId.trim(),
    name: state.name.trim(),
    labels: [...state.labels],
    tools: [...state.tools],
    mcp_servers: mcpServersFromPicks(state.mcp),
    capabilities: capabilitiesFromInput(state.capabilities),
    skill_files: state.skills.map((s) => s.bytes),
  };
  const displayName = opt(state.displayName);
  if (displayName) draft.display_name = displayName;
  const role = opt(state.role);
  if (role) draft.role = role;
  const version = opt(state.version);
  if (version) draft.version = version;
  const author = opt(state.author);
  if (author) draft.author = author;
  const description = opt(state.description);
  if (description) draft.description = description;
  const config = configFromInput(state.config);
  if (config) draft.config = config;
  return draft;
}

/* -------------------------------------------------------------------------- */
/* Step gating                                                                */
/* -------------------------------------------------------------------------- */

export type WizardStepKey = "basic" | "config" | "tools" | "skills" | "preview";

/**
 * Why a step cannot be left. Codes (not sentences) so the component renders
 * translated text and this module stays i18n-free.
 */
export type StepIssue =
  | "agentIdInvalid"
  | "nameMissing"
  | "tooManyLabels"
  | "configRange"
  | "configInteger"
  | "capabilityActionInvalid"
  | "capabilityActionDuplicate"
  | "capabilitySchemaInvalid"
  | "skillsUnparsed";

/**
 * Facts about the current step that are not part of `WizardState`.
 *
 * The skill rows are UI state — a ZIP still being parsed, a ZIP that
 * failed — not declarations, so they do not belong in `WizardState`.
 * They still gate the step, so the gate function needs them; it takes
 * them from the caller rather than keeping a second source of truth.
 */
export interface StepContext {
  /** Skill rows that are not ready: still parsing, or failed to parse. */
  unparsedSkills?: number;
}

/** Action name charset: `[a-z0-9_]`, and it cannot start with a digit. */
export const CAPABILITY_ACTION_RE = /^[a-z][a-z0-9_]*$/;

/** True when two rows would collide on the same `[capabilities.<action>]` key. */
function hasDuplicateAction(caps: CapabilityInput[]): boolean {
  const seen = new Set<string>();
  for (const cap of caps) {
    const action = cap.action.trim();
    if (!action) continue;
    if (seen.has(action)) return true;
    seen.add(action);
  }
  return false;
}

/** Report every reason the current step is not finished. Empty = may advance. */
export function stepIssues(
  step: WizardStepKey,
  state: WizardState,
  ctx?: StepContext,
): StepIssue[] {
  const issues: StepIssue[] = [];
  switch (step) {
    case "basic": {
      // The client rule is stricter than core's (reverse-domain, segments start
      // with a letter) — see `manifestRules.isValidAgentId`.
      if (!isAgentIdShapeOk(state.agentId)) issues.push("agentIdInvalid");
      if (state.name.trim() === "") issues.push("nameMissing");
      if (state.labels.length > MAX_LABELS) issues.push("tooManyLabels");
      break;
    }
    case "config": {
      for (const field of Object.keys(CONFIG_LIMITS) as Array<keyof ConfigInput>) {
        // `temperature` also has a `max`; integer fields do not. `configFieldError`
        // owns the distinction so this loop cannot drift from the inline hints.
        const err = configFieldError(
          field as keyof typeof CONFIG_LIMITS,
          state.config[field],
        );
        if (err === "range") issues.push("configRange");
        else if (err === "integer") issues.push("configInteger");
      }
      break;
    }
    case "tools":
      // Every sub-choice is optional; the step exists to declare, not to demand.
      break;
    case "skills": {
      if (state.capabilities.some((c) => c.action.trim() !== "" && !CAPABILITY_ACTION_RE.test(c.action.trim()))) {
        issues.push("capabilityActionInvalid");
      }
      if (hasDuplicateAction(state.capabilities)) issues.push("capabilityActionDuplicate");
      if (state.capabilities.some((c) => parseSchemaCell(c.inputSchema) === undefined && c.inputSchema.trim() !== "")) {
        issues.push("capabilitySchemaInvalid");
      }
      if (state.capabilities.some((c) => parseSchemaCell(c.outputSchema) === undefined && c.outputSchema.trim() !== "")) {
        issues.push("capabilitySchemaInvalid");
      }
      // A ZIP that is still parsing, or that failed to parse, must not be
      // carried into the manifest — so it blocks the step like any other
      // capability problem, and it is reported through the same channel.
      if ((ctx?.unparsedSkills ?? 0) > 0) {
        issues.push("skillsUnparsed");
      }
      break;
    }
    case "preview":
      break;
  }
  return issues;
}

/**
 * Mirror of `manifestRules.isValidAgentId`, kept in the same module as the
 * gating so a caller cannot apply one without the other.
 */
function isAgentIdShapeOk(value: string): boolean {
  const id = value.trim();
  if (id === "" || !id.includes(".")) return false;
  return id.split(".").every((seg) => /^[A-Za-z][A-Za-z0-9-]*$/.test(seg));
}

/* -------------------------------------------------------------------------- */
/* Advisory: skill tool_deps reachability                                     */
/* -------------------------------------------------------------------------- */

/**
 * Tools the manifest makes reachable, per `AgentManifest::validate_graph`:
 * `[[tools]]` ∪ every `[[mcp_servers]].tools` ∪ platform-hosted MCP tools.
 *
 * A declared MCP server's tool names are unknown at authoring time, so a
 * server the user declared contributes its *id* — a `mcp_<id>__<tool>` dep is
 * then treated as reachable, exactly like Rust does.
 */
function reachableTools(state: WizardState): Set<string> {
  const reachable = new Set<string>(state.tools);
  for (const pick of state.mcp) reachable.add(pick.id);
  return reachable;
}

/** Platform-hosted MCP ids (mirror of `PLATFORM_HOSTED_MCP` in core). */
export const PLATFORM_HOSTED_MCP = ["memory", "pm", "doc"] as const;

/**
 * Skill tool-dependencies nothing declares — the wizard's live version of
 * core's `validate_graph` *warning* half.
 *
 * Advisory by design: it is shown next to the skill row while the user can
 * still fix step 3, and it never gates `创建`. The preview step shows Rust's
 * own list, which is the authority if the two ever disagree.
 */
export function undeclaredToolDeps(state: WizardState): Array<{ skill: string; tool: string }> {
  const reachable = reachableTools(state);
  const hosted = new Set<string>(PLATFORM_HOSTED_MCP);
  const out: Array<{ skill: string; tool: string }> = [];

  for (const skill of state.skills) {
    for (const dep of skill.toolDeps) {
      if (reachable.has(dep)) continue;
      // `mcp_<server>__<tool>` where <server> is platform-hosted or a declared
      // server — the second half of core's reachability rule.
      const server = dep.startsWith("mcp_") ? dep.slice(4).split("__")[0] : undefined;
      if (server && (hosted.has(server) || reachable.has(server))) continue;
      out.push({ skill: skill.name, tool: dep });
    }
  }
  return out;
}

/** Labels that would be refused by `manifest::is_valid_label` (should be empty). */
export function invalidLabels(labels: string[]): string[] {
  return labels.filter((label) => !isValidLabel(label));
}