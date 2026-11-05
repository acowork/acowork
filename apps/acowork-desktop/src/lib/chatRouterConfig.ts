/**
 * Chat Session Router config — the read/write client for the one agent
 * field the Chat surface's usage popover edits.
 *
 * **Why this is not `agentStore`'s profile**: the Agent Setup panel and the
 * Chat surface must edit the SAME value. The Setup panel keeps its copy in
 * the store (so its number input can render without a fetch); the Chat
 * surface has no store slot for it. Both therefore talk to the same wire
 * field — `chat_router_history_max_tokens` on
 * `PUT /api/agents/{id}/config` — and this module is the single place
 * that knows the wire shape, the clamping rule and the default.
 *
 * The clamping range mirrors `ChatRouterConfig::history_max_tokens`
 * (runtime, `core/acowork-runtime/src/agent_config.rs`): 1,000–64,000
 * tokens, default 8,000. The client clamps to the same range so a value
 * the runtime would silently clamp is never *displayed* as if it were in
 * effect — the number the user sees is the number the router uses.
 */
import { getGatewayUrl } from "./config";

/** Runtime `ChatRouterConfig::history_max_tokens` lower bound. */
export const ROUTER_WINDOW_FLOOR = 1_000;
/** Runtime `ChatRouterConfig::history_max_tokens` upper bound. */
export const ROUTER_WINDOW_CEILING = 64_000;
/** Runtime `ChatRouterConfig::DEFAULT_HISTORY_MAX_TOKENS`. */
export const ROUTER_WINDOW_DEFAULT = 8_000;

/**
 * Preset budgets offered by the chat-side editor, in tokens.
 *
 * The set is deliberately coarse: the router's window is a *reading*
 * budget (how much history the router may read), not a hard context
 * limit, so the useful choices are "small / medium / large" rather than
 * the exec surface's 200K–500K ladder.
 */
export const ROUTER_WINDOW_PRESETS = [2_000, 4_000, 8_000, 16_000, 32_000] as const;

/** Wire field name on `PUT /api/agents/{id}/config` (flat, ADR-099). */
const WIRE_FIELD = "chat_router_history_max_tokens";

/**
 * Clamp a raw history-budget value into the range the runtime accepts.
 * `null`/`undefined` = "no override" (the runtime default applies).
 */
export function clampRouterWindow(value: number | null | undefined): number | null {
  if (typeof value !== "number" || !Number.isFinite(value)) return null;
  return Math.min(ROUTER_WINDOW_CEILING, Math.max(ROUTER_WINDOW_FLOOR, Math.floor(value)));
}

/**
 * The router's effective history budget for one agent.
 *
 * Reads `chat_router.history_max_tokens` off the agent's config envelope —
 * the same `GET /api/agents/{id}/config` response the Agent Setup panel
 * consumes. `null` = no override on disk → the runtime default (8,000).
 *
 * The read is best-effort by contract: a failed fetch resolves to `null`
 * (the caller renders the default), never throws — a usage popover must
 * not fail to open because a config read raced a Runtime restart.
 */
export async function fetchRouterWindow(agentId: string): Promise<number | null> {
  try {
    const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/config`);
    if (!resp.ok) return null;
    const data = (await resp.json()) as ServerConfigLike;
    const raw = data?.config?.chat_router?.history_max_tokens;
    if (typeof raw !== "number") return null;
    return clampRouterWindow(raw);
  } catch {
    return null;
  }
}

/**
 * Persist the router's history budget for one agent.
 *
 * Writes the same wire field the Agent Setup panel writes, so the two
 * surfaces can never drift: whichever wrote last wins, and both read back
 * the same value on the next fetch.
 *
 * `null` clears the override (the runtime default applies again).
 * Returns `true` on success; a failed write returns `false` and the
 * caller keeps the draft open so the user can retry.
 */
export async function saveRouterWindow(
  agentId: string,
  value: number | null,
): Promise<boolean> {
  const payload = value === null ? null : clampRouterWindow(value);
  try {
    const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/config`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      // `null` = explicit clear (`FieldPatch::Clear`) — the runtime drops
      // the on-disk key, so the design default applies again.
      body: JSON.stringify({ [WIRE_FIELD]: payload }),
    });
    return resp.ok;
  } catch {
    return false;
  }
}

/**
 * Minimal shape of `GET /api/agents/{id}/config` this module reads.
 * Kept structural (not the full envelope type) because the wire is
 * partial — `AgentConfig` skips every `None` field on serialization.
 */
interface ServerConfigLike {
  config?: {
    chat_router?: {
      history_max_tokens?: number | null;
    } | null;
  } | null;
}
