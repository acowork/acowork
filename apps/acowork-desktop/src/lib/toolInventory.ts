/**
 * Agent-layer tool inventory + per-session tool narrowing (desktop side).
 *
 * # Two layers, one direction of travel
 *
 * The **agent layer** is what the right-panel settings cards edit and what
 * `agent-tools.json` / `agent-mcp.json` persist: which builtin tools the agent
 * exposes and which MCP servers / tools it has enabled. It is fetched here
 * from the same Runtime endpoints the panel uses (`GET .../tools` and
 * `GET .../mcp-tools`).
 *
 * The **session layer** is a *narrowing* over it: a session may switch OFF
 * something the agent exposes, never switch ON something the agent does not.
 * That asymmetry is the backend's invariant
 * (`core/acowork-runtime/src/agent/session/session_tool_layer.rs`) and it is
 * mirrored here so the UI cannot express an impossible state — see
 * [`deriveToolStates`] and [`SessionToolPolicy`].
 *
 * Everything in this module except [`fetchAgentToolInventory`] is pure, so the
 * switch semantics are unit-tested without a network or a React tree. The
 * session-layer transport lives in [session-control](lib/session-control.ts)
 * with the rest of the control plane — this module only normalises what it
 * returns.
 */
import { getGatewayUrl } from "./config";
import { with503Retry } from "./httpRetry";
import { log } from "./logger";
import { getSessionToolPolicy, putSessionToolPolicy } from "./session-control";
import type { AgentMcpToolsConfig, McpServerView } from "./types";

/**
 * Wire mirror of `acowork_core::session_meta::SessionToolPolicy`.
 *
 * All three groups are **deny** lists, and every group is optional: an absent
 * or empty group means "follow the agent layer for this group". A session that
 * never had a tool toggled therefore stores no policy at all (`null` on the
 * wire), which is exactly what makes "inherit by default, write only on
 * change" true rather than merely conventional.
 */
export interface SessionToolPolicy {
  disabled_builtin_tools: string[];
  disabled_mcp_servers: string[];
  disabled_mcp_tools: McpToolRef[];
}

/**
 * The **wire** shape of `acowork_core::session_meta::SessionToolPolicy`, which
 * is not the shape the UI wants.
 *
 * Rust persists the per-tool denies as `BTreeMap<String, BTreeSet<String>>` —
 * `{"server": ["tool", …]}` — because the two levels are hierarchical (a
 * disabled server makes its tool rows moot). The UI iterates identities, so it
 * keeps `McpToolRef[]`. Keeping both types and converting in one place
 * ([`fromWirePolicy`] / [`toWirePolicy`]) is what makes the difference explicit
 * instead of a latent mismatch; the Rust side pins the same literal in
 * `session_meta::tool_policy_tests::mcp_tool_denies_serialise_as_a_map`.
 */
export interface WireSessionToolPolicy {
  disabled_builtin_tools?: string[];
  disabled_mcp_servers?: string[];
  disabled_mcp_tools?: Record<string, string[]>;
}

/** One `server` + `tool` pair inside a session's deny list. */
export interface McpToolRef {
  server: string;
  tool: string;
}

/** An empty policy — the "follow the agent layer" value. */
export function emptyToolPolicy(): SessionToolPolicy {
  return {
    disabled_builtin_tools: [],
    disabled_mcp_servers: [],
    disabled_mcp_tools: [],
  };
}

/**
/** Normalise the wire shape into the UI's total value. */
export function fromWirePolicy(
  raw: WireSessionToolPolicy | null | undefined,
): SessionToolPolicy {
  const mcpTools = raw?.disabled_mcp_tools ?? {};
  return {
    disabled_builtin_tools: [...(raw?.disabled_builtin_tools ?? [])],
    disabled_mcp_servers: [...(raw?.disabled_mcp_servers ?? [])],
    disabled_mcp_tools: Object.entries(mcpTools).flatMap(([server, tools]) =>
      (tools ?? []).map((tool) => ({ server, tool })),
    ),
  };
}

/** The inverse of [`fromWirePolicy`]: identities → `{ server: [tool] }`. */
export function toWirePolicy(
  policy: SessionToolPolicy,
): WireSessionToolPolicy {
  const mcpTools: Record<string, string[]> = {};
  for (const { server, tool } of policy.disabled_mcp_tools) {
    (mcpTools[server] ??= []).push(tool);
  }
  return {
    disabled_builtin_tools: [...policy.disabled_builtin_tools],
    disabled_mcp_servers: [...policy.disabled_mcp_servers],
    disabled_mcp_tools: mcpTools,
  };
}

/** True when the policy denies nothing — i.e. it is identical to inheriting. */
export function isToolPolicyEmpty(policy: SessionToolPolicy): boolean {
  return (
    policy.disabled_builtin_tools.length === 0 &&
    policy.disabled_mcp_servers.length === 0 &&
    policy.disabled_mcp_tools.length === 0
  );
}

/**
 * `null` for an all-empty policy, so "the user re-enabled everything" writes
 * the same value as "never touched" and the meta column stays empty.
 */
export function policyOrNull(
  policy: SessionToolPolicy,
): SessionToolPolicy | null {
  return isToolPolicyEmpty(policy) ? null : policy;
}

// ── Inventory ─────────────────────────────────────────────────────────

/** One builtin tool as the agent layer exposes it. */
export interface InventoryBuiltinTool {
  name: string;
  /** `enabled` in `agent-tools.json` — the agent-layer switch. */
  agentEnabled: boolean;
}

/** One MCP tool under a server, with the agent-layer effective state. */
export interface InventoryMcpTool {
  server: string;
  tool: string;
  /** `server__tool`, the form the Runtime uses for the LLM-facing tool name
   *  and the form a session deny entry is matched against. */
  prefixedName: string;
  description?: string | null;
  /** `server.active && tool.enabled` — a tool under a switched-off server is
   *  not connected at all, so it cannot be enabled at either layer. */
  agentEnabled: boolean;
}

/** One MCP server, with its tool list (empty when not connected). */
export interface InventoryMcpServer {
  name: string;
  /** Friendly alias from the Gateway catalog, when the parent has one. */
  displayName?: string;
  /** `active` in `agent-mcp.json` — the agent-layer switch. */
  agentEnabled: boolean;
  tools: InventoryMcpTool[];
}

/**
 * The agent layer, flattened into the two groups the menu shows.
 *
 * Built from `GET .../tools` (`tools` + `mcp_servers_defs`) and
 * `GET .../mcp-tools` (per-server tool lists) — the same pair of endpoints the
 * settings panel reads, so the two surfaces cannot disagree about what the
 * agent exposes.
 */
export interface AgentToolInventory {
  agentId: string;
  builtin: InventoryBuiltinTool[];
  mcpServers: InventoryMcpServer[];
}

/** Shape of `GET /api/agents/{id}/tools` (only the fields used here). */
interface AgentToolsResponse {
  tools?: { name: string; enabled?: boolean }[];
  mcp_servers_defs?: McpServerView[];
}

/** Shape of `GET /api/agents/{id}/mcp-tools`.
 *
 * The endpoint answers with the `{ agent_id, servers }` envelope
 * (`core/acowork-runtime/src/http/server.rs::get_agent_mcp_tools`); the
 * per-server map the inventory needs is the `servers` field inside it. The
 * envelope is declared once, in [types](types.ts) (`AgentMcpToolsConfig`),
 * next to the settings panel's copy of the same shape.
 *
 * Do NOT re-declare the inner map as this endpoint's response type here: an
 * `as` to such an alias is unchecked, so indexing the envelope by server name
 * type-checks and yields `undefined` for every server. That is how the `+`
 * menu ended up with no tool drill-down at all while its pure fold function
 * stayed green — the drift lived one layer out, at the fetch boundary.
 */
type McpToolsServers = AgentMcpToolsConfig["servers"];

/** The Runtime-side prefix separator for an MCP tool's LLM-facing name. */
export const MCP_TOOL_SEPARATOR = "__";

/** `server__tool` — the name an MCP tool is known by at the tool layer. */
export function prefixedMcpToolName(server: string, tool: string): string {
  return `${server}${MCP_TOOL_SEPARATOR}${tool}`;
}

/**
 * Fold the two endpoint payloads into one inventory.
 *
 * Kept pure (payloads in, inventory out) so the shape handling is testable
 * without a gateway — the field names on the wire are the only thing that can
 * silently drift, and a unit test pins them.
 */
export function buildToolInventory(
  agentId: string,
  tools: AgentToolsResponse | null | undefined,
  mcpTools: McpToolsServers | null | undefined,
): AgentToolInventory {
  const builtin: InventoryBuiltinTool[] = (tools?.tools ?? []).map((t) => ({
    name: t.name,
    agentEnabled: t.enabled !== false,
  }));

  const mcpServers: InventoryMcpServer[] = (tools?.mcp_servers_defs ?? []).map(
    (server) => {
      const agentEnabled = server.active === true;
      const rows = mcpTools?.[server.name] ?? [];
      return {
        name: server.name,
        agentEnabled,
        tools: rows.map((row) => ({
          server: server.name,
          tool: row.name,
          prefixedName: prefixedMcpToolName(server.name, row.name),
          description: row.description,
          agentEnabled: agentEnabled && row.enabled !== false,
        })),
      };
    },
  );

  return { agentId, builtin, mcpServers };
}

// ── Policy application ────────────────────────────────────────────────

/**
 * The three switch states the UI needs for one row.
 *
 * `effective` is what the session actually runs with; `agentEnabled` is what
 * the agent exposes; `denied` is this session's own narrowing. A row can only
 * ever move from `effective = agentEnabled` down, which is why the switch is
 * disabled when `agentEnabled` is false.
 */
export interface ToolRowState {
  agentEnabled: boolean;
  denied: boolean;
  effective: boolean;
}

function rowState(agentEnabled: boolean, denied: boolean): ToolRowState {
  return { agentEnabled, denied, effective: agentEnabled && !denied };
}

/** Effective switch state of an inventory row under one session policy. */
export function deriveBuiltinState(
  tool: InventoryBuiltinTool,
  policy: SessionToolPolicy,
): ToolRowState {
  return rowState(
    tool.agentEnabled,
    policy.disabled_builtin_tools.includes(tool.name),
  );
}

export function deriveMcpServerState(
  server: InventoryMcpServer,
  policy: SessionToolPolicy,
): ToolRowState {
  return rowState(
    server.agentEnabled,
    policy.disabled_mcp_servers.includes(server.name),
  );
}

/**
 * A tool is denied when either it is named explicitly or its whole server is
 * switched off for this session — the backend prunes at both levels, and the
 * menu must show the same answer (a tool row under a session-disabled server
 * reads as off, and its own toggle is moot).
 */
export function deriveMcpToolState(
  tool: InventoryMcpTool,
  policy: SessionToolPolicy,
): ToolRowState {
  const denied =
    policy.disabled_mcp_servers.includes(tool.server) ||
    policy.disabled_mcp_tools.some(
      (r) => r.server === tool.server && r.tool === tool.tool,
    );
  return rowState(tool.agentEnabled, denied);
}

// ── Policy edits (pure, immutable) ────────────────────────────────────

/**
 * Turn a builtin tool on/off for this session.
 *
 * `on` removes the deny entry (falling back to the agent layer); `off` adds it.
 * Asking to turn ON a tool the agent does not expose is a no-op — the switch is
 * disabled in that case, and silently ignoring the impossible direction keeps
 * this function total instead of throwing.
 */
export function withBuiltinToolToggled(
  policy: SessionToolPolicy,
  tool: InventoryBuiltinTool,
  on: boolean,
): SessionToolPolicy {
  if (on && !tool.agentEnabled) return policy;
  const set = new Set(policy.disabled_builtin_tools);
  if (on) set.delete(tool.name);
  else set.add(tool.name);
  return { ...policy, disabled_builtin_tools: [...set].sort() };
}

/**
 * Turn an MCP server on/off for this session.
 *
 * Switching a server off leaves any per-tool deny entries under it in place:
 * re-enabling the server must not silently resurrect tools the user had
 * already trimmed (and they are inert while the server is off).
 */
export function withMcpServerToggled(
  policy: SessionToolPolicy,
  server: InventoryMcpServer,
  on: boolean,
): SessionToolPolicy {
  if (on && !server.agentEnabled) return policy;
  const set = new Set(policy.disabled_mcp_servers);
  if (on) set.delete(server.name);
  else set.add(server.name);
  return { ...policy, disabled_mcp_servers: [...set].sort() };
}

/** Turn one MCP tool on/off for this session. */
export function withMcpToolToggled(
  policy: SessionToolPolicy,
  tool: InventoryMcpTool,
  on: boolean,
): SessionToolPolicy {
  if (on && !tool.agentEnabled) return policy;
  // A server-level deny already covers every tool under it, so an ON request
  // for one tool has to lift that server deny — otherwise the switch would
  // spring back on the next render.
  const servers = on
    ? policy.disabled_mcp_servers.filter((s) => s !== tool.server)
    : policy.disabled_mcp_servers;
  const refs = policy.disabled_mcp_tools.filter(
    (r) => !(r.server === tool.server && r.tool === tool.tool),
  );
  if (!on) refs.push({ server: tool.server, tool: tool.tool });
  return {
    ...policy,
    disabled_mcp_servers: [...servers].sort(),
    disabled_mcp_tools: refs.sort((a, b) =>
      a.server === b.server
        ? a.tool.localeCompare(b.tool)
        : a.server.localeCompare(b.server),
    ),
  };
}

/** The three deny groups, as addressable keys for the per-group reset. */
export type ToolPolicyGroup = "builtin" | "mcpServers" | "mcpTools";

/**
 * Drop one group's denies — the "follow the agent default" reset for that
 * group. The other groups are untouched: resetting MCP servers must not
 * re-enable builtin tools the user switched off.
 */
export function withGroupReset(
  policy: SessionToolPolicy,
  group: ToolPolicyGroup,
): SessionToolPolicy {
  switch (group) {
    case "builtin":
      return { ...policy, disabled_builtin_tools: [] };
    case "mcpServers":
      return { ...policy, disabled_mcp_servers: [] };
    case "mcpTools":
      return { ...policy, disabled_mcp_tools: [] };
  }
}

// ── Transport ─────────────────────────────────────────────────────────

/**
 * Fetch the agent layer for `agentId`.
 *
 * A failed `/tools` read is fatal (no inventory to show); a failed
 * `/mcp-tools` read degrades to "servers without their tool lists", because a
 * server that has never connected legitimately has none and the menu can still
 * offer the server-level switch.
 */
export async function fetchAgentToolInventory(
  agentId: string,
): Promise<AgentToolInventory> {
  const [toolsResp, mcpToolsResp] = await Promise.all([
    with503Retry(
      () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/tools`),
      { tag: `toolInventory.tools(${agentId})`, logger: log },
    ),
    with503Retry(
      () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/mcp-tools`),
      { tag: `toolInventory.mcpTools(${agentId})`, logger: log },
    ).catch(() => null),
  ]);

  if (!toolsResp.ok) {
    throw new Error(`GET /agents/${agentId}/tools -> ${toolsResp.status}`);
  }
  const tools = (await toolsResp.json()) as AgentToolsResponse;
  // The endpoint answers with the `{ agent_id, servers }` envelope, so the
  // per-server map has to be unwrapped before it reaches the fold function.
  // Reading the envelope itself as that map is what left every server with
  // zero tools — and hence no drill-down in the `+` menu.
  let mcpServers: McpToolsServers | null = null;
  if (mcpToolsResp?.ok) {
    mcpServers = ((await mcpToolsResp.json()) as AgentMcpToolsConfig).servers ?? null;
  }

  return buildToolInventory(agentId, tools, mcpServers);
}

/**
 * Read the session layer. `null` means "inherit the agent layer" — the state
 * of every session the user has not trimmed.
 *
 * The transport is [session-control](lib/session-control.ts) (`getSessionToolPolicy`:
 * control plane, with the 503 retry and the 403/404 → `SessionAccessError`
 * mapping); this wrapper only normalises the wire shape into a total policy.
 */
export async function fetchSessionToolPolicy(
  agentId: string,
  sessionId: string,
): Promise<SessionToolPolicy | null> {
  return normalizeOrNull(await getSessionToolPolicy(agentId, sessionId));
}

/**
 * Replace the session layer. `null` clears it (back to inheriting), and the
 * Runtime's echo is normalised the same way the read is.
 */
export async function saveSessionToolPolicy(
  agentId: string,
  sessionId: string,
  policy: SessionToolPolicy | null,
): Promise<SessionToolPolicy | null> {
  // The transport takes the wire shape: the UI's identity list has to become
  // `{ server: [tool] }` before it leaves this module.
  return normalizeOrNull(
    await putSessionToolPolicy(
      agentId,
      sessionId,
      policy ? toWirePolicy(policy) : null,
    ),
  );
}

/** A policy that denies nothing is indistinguishable from inheriting. */
function normalizeOrNull(
  raw: WireSessionToolPolicy | null,
): SessionToolPolicy | null {
  if (!raw) return null;
  const normalized = fromWirePolicy(raw);
  return isToolPolicyEmpty(normalized) ? null : normalized;
}
