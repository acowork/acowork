// ADR-104 follow-up: read a platform-hosted MCP server's tool list *before*
// an agent exists, so the Create wizard can offer per-tool opt-in.
//
// ## Why probe instead of a static table
//
// The tool list is the server's fact, not the client's: `pm` ships
// `PM_TOOL_MANIFEST`, `doc` ships `DOC_TOOL_MANIFEST`, `memory` ships
// `MEMORY_TOOL_MANIFEST`, and each one answers `tools/list` straight from
// that manifest. A second copy in the Desktop would drift (the same reason
// `mcpPresets.ts` stopped owning the preset table). So the wizard asks the
// server itself, over the Gateway reverse-proxy paths the Runtime already
// uses to connect:
//
//   pm     -> POST /api/pm/mcp
//   doc    -> POST /api/doc/mcp
//   memory -> POST /api/memory/mcp
//
// `tools/list` needs no `X-MCP-Actor` on any of the three (the header only
// scopes `tools/call`), and the app-wide auth interceptor adds the Desktop's
// own bearer token, so this works under `AUTH_MODE=multi_user` too.
//
// ## Failure policy
//
// A probe failure is NOT an error state that blocks creation: the wizard
// falls back to "all tools enabled" (the empty whitelist = "no opinion"
// semantics of `[[mcp_servers]].tools`) and says so. The user can still fix
// the set later in the agent's Tools panel.

import { getGatewayUrl } from "./config";
import { log } from "./logger";

/** One advertised tool: the wizard renders name + description, whitelists by name. */
export interface HostedMcpTool {
  name: string;
  description?: string;
}

/** Gateway reverse-proxy MCP endpoint per platform-hosted server id.
 *  Mirrors `PLATFORM_HOSTED_MCP` in core (`manifest.rs`) — keys MUST stay
 *  in sync with that list, and the wizard only probes ids it finds here. */
const HOSTED_MCP_PATHS: Record<string, string> = {
  pm: "/api/pm/mcp",
  doc: "/api/doc/mcp",
  memory: "/api/memory/mcp",
};

/** True when this hosted id's tool list can be probed at all. */
export function isProbeableHostedMcp(id: string): boolean {
  return Object.prototype.hasOwnProperty.call(HOSTED_MCP_PATHS, id);
}

let cache: Map<string, HostedMcpTool[]> | null = null;
const inflight = new Map<string, Promise<HostedMcpTool[]>>();

/**
 * Fetch one hosted server's `tools/list` (cached per session — these are
 * static manifests server-side, and the wizard row re-expands on every
 * keystroke of the step-3 form).
 *
 * Concurrent calls for the same id share one request; a failed request is
 * not cached, so the next expand retries.
 */
export async function fetchHostedMcpTools(id: string): Promise<HostedMcpTool[]> {
  const path = HOSTED_MCP_PATHS[id];
  if (!path) throw new Error(`unknown platform-hosted MCP server: ${id}`);
  if (!cache) cache = new Map();
  const hit = cache.get(id);
  if (hit) return hit;
  const pending = inflight.get(id);
  if (pending) return pending;

  const req = (async (): Promise<HostedMcpTool[]> => {
    const resp = await fetch(`${getGatewayUrl()}${path}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }),
    });
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    const body = (await resp.json()) as {
      result?: { tools?: { name?: string; description?: string }[] };
      error?: { message?: string };
    };
    if (body.error) throw new Error(body.error.message ?? "tools/list failed");
    const tools = (body.result?.tools ?? [])
      .filter((t): t is { name: string; description?: string } => typeof t.name === "string")
      .map((t) => (t.description ? { name: t.name, description: t.description } : { name: t.name }));
    cache?.set(id, tools);
    return tools;
  })();

  inflight.set(id, req);
  try {
    return await req;
  } catch (e) {
    log.warn(`fetchHostedMcpTools(${id}) failed`, e);
    throw e;
  } finally {
    inflight.delete(id);
  }
}

/** Test hook: drop the session cache. */
export function clearHostedMcpToolsCache(): void {
  cache = null;
  inflight.clear();
}
