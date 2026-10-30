// ADR-104: MCP presets and package `[[mcp_servers]]` declarations.
//
// ## Why the table is not here anymore
//
// This module used to own a static `MCP_PRESETS` array. A preset id is now a
// **package-visible identifier** (`[[mcp_servers]] preset = "brave-search"`),
// so its meaning cannot live in one client — the Gateway validates it, other
// clients render it, and both have to agree. The registry therefore lives in
// `acowork-gateway/src/http/mcp_presets.rs` and this file is only the client:
// `GET /api/mcp-presets` to read it, `POST /api/mcp-catalog/from-declaration`
// to materialize one (alone, or because a package recommends it).
//
// The Desktop never builds a catalog entry out of a preset itself: the spawn
// recipe and the `origin` string are both Gateway-owned facts, and a second
// copy here would drift.

import { getGatewayUrl } from "./config";
import { with503Retry } from "./httpRetry";
import { log } from "./logger";
import type {
  McpInstallSpec,
  McpPresetCategory,
  McpTransportDef,
  OperationAck,
} from "./types";

/**
 * One curated MCP server as served by `GET /api/mcp-presets`.
 *
 * Field names are snake_case because they mirror the Rust struct verbatim —
 * one wire shape, no mapping layer to drift.
 */
export interface McpPreset {
  id: string;
  name: string;
  description: string;
  category: McpPresetCategory;
  transport: McpTransportDef;
  /** stdio spawn command */
  command: string;
  args: string[];
  /** Env vars the user must supply; mirrored into `required_secrets`. */
  required_env: string[];
  install_hint: string;
  /** lucide-react icon name */
  icon: string;
  /** Declarative install spec (ADR-072); `state` is always `unknown`. */
  install: McpInstallSpec;
}

/*
 * There is deliberately no `optionalEnv` here: the Gateway registry models a
 * preset as "spawn recipe + required secrets + install spec", and every
 * curated preset's optional env map was empty. When one is needed, add it to
 * the Rust struct rather than resurrecting a client-side copy.
 */

let presetsCache: McpPreset[] | null = null;
let presetsInflight: Promise<McpPreset[]> | null = null;

/**
 * Load the preset registry (cached for the session — it is static Gateway
 * data, and the picker re-renders on every keystroke of the search box).
 *
 * Concurrent callers share one request: the Harness MCP tab and the Create
 * wizard can both mount before the first response lands.
 */
export async function fetchMcpPresets(force = false): Promise<McpPreset[]> {
  if (!force && presetsCache) return presetsCache;
  if (presetsInflight) return presetsInflight;

  presetsInflight = (async () => {
    try {
      const resp = await with503Retry(
        () => fetch(`${getGatewayUrl()}/api/mcp-presets`),
        { tag: "fetchMcpPresets", logger: log },
      );
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as { presets?: McpPreset[] };
      const presets = data.presets ?? [];
      presetsCache = presets;
      return presets;
    } catch (e) {
      log.warn("fetchMcpPresets failed", e);
      // Do not cache a failure — the next mount retries.
      return [];
    } finally {
      presetsInflight = null;
    }
  })();

  return presetsInflight;
}

/** Test hook: drop the session cache. */
export function clearMcpPresetsCache(): void {
  presetsCache = null;
  presetsInflight = null;
}

/** What a `[[mcp_servers]]` declaration can express, in wire form. */
export interface McpDeclarationBody {
  /** Catalog key — equals the preset id for preset-form declarations. */
  key: string;
  preset_id?: string;
  inline?: {
    transport: McpTransportDef;
    url?: string;
    headers?: Record<string, string>;
    required_secrets?: string[];
  };
  /** Package that recommends this server; selects the catalog `origin`. */
  agent_id?: string;
  /** Secret typed before the entry exists; ignored once it does. */
  env?: Record<string, string>;
}

/**
 * Materialize a declaration (or a user-picked preset) into the Gateway
 * catalog.
 *
 * Idempotent: `200` means "already there, untouched" and `201` means
 * "created". Both are success. The caller should reload the catalog either
 * way, because the entry may have been created by another surface moments
 * ago.
 */
export async function materializeDeclaration(
  body: McpDeclarationBody,
): Promise<{ created: boolean }> {
  const resp = await fetch(`${getGatewayUrl()}/api/mcp-catalog/from-declaration`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
    throw new Error(err.error || `HTTP ${resp.status}`);
  }
  // Typed so the ADR-059 ACK contract is compile-checked even though the
  // value is not surfaced.
  await resp.json().catch(() => undefined) as OperationAck | undefined;
  return { created: resp.status === 201 };
}

/** Groups presets by category, preserving registry order. */
export function groupPresetsByCategory(
  presets: McpPreset[],
): Array<[string, McpPreset[]]> {
  const groups = new Map<string, McpPreset[]>();
  for (const preset of presets) {
    const bucket = groups.get(preset.category);
    if (bucket) bucket.push(preset);
    else groups.set(preset.category, [preset]);
  }
  return Array.from(groups.entries());
}