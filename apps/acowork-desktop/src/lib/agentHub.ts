/**
 * Agent Hub catalog contract and the pure logic behind the Agent 库 tab.
 *
 * PRD: docs/prd/zh/prd-agent-hub-install-dialog.md.
 *
 * The catalog itself is fetched on the Rust side (`fetch_hub_index`) — never
 * with a webview `fetch` — because the Hub is served by Cloudflare Pages,
 * which answers an unknown path with `200 + HTML`. Rust checks the
 * content-type before parsing; the frontend only ever sees a parsed catalog
 * or an error (PRD §5 D6 / §9 F1).
 *
 * Everything in this file is pure so the search / chip / risk / size rules
 * are unit-testable without a DOM or a mocked `invoke`.
 */
import { invoke } from "@tauri-apps/api/core";

/** `GET {hub}/data/index.json` — field names mirror the published JSON. */
export interface HubIndex {
  schema_version: number;
  agents: HubAgent[];
  /**
   * Site root the catalog was read from, injected by Rust (not a JSON field).
   * The display side never hard-codes the Hub origin, so `ACOWORK_HUB_URL`
   * pointing at staging cannot leave avatars on the production host.
   */
  base_url: string;
}

export interface HubAgent {
  /** Package identity. NOT an instance key (ADR-073). */
  agent_id: string;
  version: string;
  name: string;
  display_name: string;
  role: string;
  description: string;
  labels: string[];
  capabilities: HubCapability[];
  skills: string[];
  avatar: string | null;
  permissions: string[];
  /** The Hub's own high-risk subset of `permissions`; drives REQ-007. */
  high_risk_permissions: string[];
  /** Absent when the entry carries no installable package. */
  package: HubPackage | null;
  signature: HubSignature | null;
  /** Shape not pinned by the published data — passed through verbatim. */
  deprecated: unknown;
}

export interface HubCapability {
  key: string;
  description: string;
  skills: string[];
}

export interface HubPackage {
  /** Site-relative download path. */
  file: string;
  size_bytes: number;
  sha256: string;
}

export interface HubSignature {
  status: string;
}

/** Result of `install_from_hub` — the instance the operation resolved to. */
export interface HubInstallResult {
  agent_id: string;
  instance_id: string;
  already_present: boolean;
}

/** Summary of a local `.agent` file (`inspect_local_package`). */
export interface LocalPackageInfo {
  file_name: string;
  size_bytes: number;
  agent_id: string;
}

/**
 * One row of the Local tab's file library (`list_local_agent_packages`).
 *
 * Mirror of `commands::agent_library::LocalAgentRow`. Identity comes from the
 * package's own `manifest.toml`, never from the filename (REQ-024) — which is
 * why a row whose file cannot be parsed still exists, with every manifest field
 * `null` and `error` filled (PRD §14.4). Rendering `""` there would make the UI
 * responsible for knowing what "empty" means.
 */
export interface LocalAgentRow {
  file_name: string;
  /** Absolute path — handed back verbatim to `ensureLocalAgent`. */
  path: string;
  agent_id: string | null;
  name: string | null;
  version: string | null;
  description: string | null;
  size_bytes: number | null;
  /** Package role / job title — the line the Hub card shows next to the name. */
  role: string | null;
  /** Discovery labels (`labels` in the manifest). */
  labels: string[];
  /** Capability keys (`[capabilities.*]` table names), sorted. */
  capabilities: string[];
  /** `skills/<name>/SKILL.md` entries of the archive — the manifest has no list. */
  skills: string[];
  /** The package's own avatar inlined as a `data:` URL, or `null` when absent. */
  avatar: string | null;
  /** Declared permissions — only counted, never confirmed (REQ-007 boundary). */
  permissions: string[];
  error: string | null;
}

/**
 * Envelope of the Local list. `warning` is the library-level reason the list is
 * empty or incomplete (seeding failed / directory unreadable) in the IO's own
 * words — REQ-028 forbids a guessed message, and an empty list without a reason
 * is indistinguishable from a broken one.
 */
export interface LocalAgentList {
  rows: LocalAgentRow[];
  warning: string | null;
}

/** Result of `ensure_local_agent` — mirrors the Hub path's shape (D11). */
export interface LocalAgentEnsureResult {
  agent_id: string;
  instance_id: string;
  already_present: boolean;
}

/**
 * Permissions the user must explicitly accept before anything is submitted
 * (REQ-007). The Hub's own `high_risk_permissions` list wins when it is
 * published; this set is the fallback for entries that omit it, so a package
 * that can run a shell is never installed on an unchecked click.
 */
export const HIGH_RISK_PERMISSIONS = new Set(["Shell", "FilesystemWrite"]);

export function isHighRiskPermission(permission: string): boolean {
  return HIGH_RISK_PERMISSIONS.has(permission);
}

/** High-risk items to confirm, preferring the Hub's classification. */
export function highRiskItemsOf(agent: HubAgent): string[] {
  const declared = agent.high_risk_permissions ?? [];
  if (declared.length > 0) return declared;
  return (agent.permissions ?? []).filter(isHighRiskPermission);
}

/**
 * Search haystack (REQ-010): the fields a user would name an agent by.
 * Capability keys are included because the published schema carries no
 * separate `capability_keys` field.
 */
export function searchHubAgents(agents: HubAgent[], query: string): HubAgent[] {
  const tokens = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (tokens.length === 0) return agents;
  return agents.filter((agent) => {
    const haystack = haystackOf(agent);
    // Multi-word AND: every token must appear somewhere on the entry.
    return tokens.every((token) => haystack.includes(token));
  });
}

function haystackOf(agent: HubAgent): string {
  return [
    agent.display_name,
    agent.name,
    agent.role,
    agent.description,
    ...(agent.labels ?? []),
    ...(agent.capabilities ?? []).map((c) => c.key),
    ...(agent.skills ?? []),
  ]
    .filter(Boolean)
    .join("\n")
    .toLowerCase();
}

/**
 * Card chips (REQ-004): at most 4, labels first because they are the
 * human-curated axis, capability keys as the filler.
 */
/**
 * The card chip row, in the one order both tabs use: labels first (the author's
 * discovery words), then capability keys, then skill names, de-duplicated
 * case-insensitively and capped.
 *
 * Shared by the Hub card and the Local row so the two tabs cannot drift into
 * showing different chip sets for the same package — a local `.agent` and its
 * published Hub entry describe the same artifact.
 */
export function chipsOf(sources: Array<Array<string | null | undefined> | undefined>, limit = 4): string[] {
  const chips: string[] = [];
  const seen = new Set<string>();
  for (const source of sources) {
    for (const value of source ?? []) {
      const v = (value ?? "").trim();
      if (!v || seen.has(v.toLowerCase())) continue;
      seen.add(v.toLowerCase());
      chips.push(v);
      if (chips.length === limit) return chips;
    }
  }
  return chips;
}

export function hubChipsOf(agent: HubAgent, limit = 4): string[] {
  return chipsOf(
    [agent.labels, (agent.capabilities ?? []).map((c) => c.key), agent.skills],
    limit,
  );
}

/**
 * The same chip row for a library file. Same sources, same order, same cap —
 * `labels` / `[capabilities.*]` keys / `skills/*` directories all come out of
 * the archive, so a local row can look like its Hub card without inventing
 * fields.
 */
export function localChipsOf(row: LocalAgentRow, limit = 4): string[] {
  return chipsOf([row.labels, row.capabilities, row.skills], limit);
}

export function hubDisplayNameOf(agent: HubAgent): string {
  return agent.display_name || agent.name || agent.agent_id;
}

/**
 * Resolve a site-relative Hub asset path (the avatar) for an `<img src>`.
 *
 * Downloads never go through here — Rust re-validates every package path
 * (`hub_client::join_asset_url`). This is the display-side mirror: `..`
 * traversal and protocol-relative `//host/…` are refused, and only an
 * already-absolute `http(s)` URL is passed through, so a catalog entry can
 * never point the webview at `file:` or a sibling origin's asset tree.
 * A rejected / empty path renders as the initial-letter fallback.
 */
export function hubAssetUrl(base: string, rel: string | null | undefined): string | null {
  const raw = (rel ?? "").trim();
  if (!raw || !base) return null;
  if (raw.startsWith("//") || raw.includes("\\")) return null;
  if (raw.split("/").includes("..")) return null;
  if (/^https:\/\//i.test(raw) || /^http:\/\//i.test(raw)) return raw;
  if (/^[a-z][a-z0-9+.-]*:/i.test(raw)) return null;
  return `${base.replace(/\/+$/, "")}/${raw.replace(/^\/+/, "")}`;
}

/**
 * Initial-letter avatar fallback (REQ-016). Unicode-aware on purpose: the
 * catalog is multilingual and `slice(0, 1)` on a surrogate pair renders "?".
 */
export function hubAvatarInitialOf(agent: HubAgent): string {
  const name = hubDisplayNameOf(agent);
  const first = Array.from(name.trim()).find((ch) => /\S/.test(ch)) ?? "";
  return first.toLocaleUpperCase();
}

/**
 * Byte size for the card meta line. Kept here so the Library and Local tabs
 * render the same number the same way.
 */
export function formatSizeBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return `${bytes} B`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${kb.toFixed(1)} KB`;
  return `${(kb / 1024).toFixed(1)} MB`;
}

/** `deprecated` marker + reason (REQ-017), tolerant of either shape. */
export function hubDeprecationOf(agent: HubAgent): { flagged: boolean; reason?: string } {
  const value = agent.deprecated;
  if (value === null || value === undefined || value === false) {
    return { flagged: false };
  }
  if (value === true) return { flagged: true };
  if (typeof value === "object") {
    const obj = value as Record<string, unknown>;
    const reason = [obj.reason, obj.message, obj.note, obj.detail].find(
      (v): v is string => typeof v === "string" && v.trim().length > 0,
    );
    return { flagged: true, reason };
  }
  if (typeof value === "string" && value.trim()) {
    return { flagged: true, reason: value.trim() };
  }
  return { flagged: false };
}

/**
 * How many instances of this package already live on the target node
 * (REQ-018). Drives the card's primary-button routing: zero → idempotent
 * ensure; one or more → the click lands another instance (ADR-073
 * multi-instance is legal, and an "already present, nothing happened"
 * primary button reads as broken). The version stays per instance, so
 * this count never implies an "upgradable" verdict.
 */
export function countInstancesOnNode(
  entries: ReadonlyArray<{ agent_id: string; node_id?: string | null }>,
  agentId: string,
  nodeId: string | null,
): number {
  if (!nodeId) return 0;
  return entries.filter((e) => e.agent_id === agentId && e.node_id === nodeId).length;
}

// ── Catalog access (session-scoped cache) ─────────────────────────────────
//
// One successful fetch is reused for the rest of the session (PRD §7 "本地
// 缓存命中"), and concurrent callers share one request. The shared promise is
// deliberately NOT bound to any caller's lifetime — a caller that unmounts
// must not poison the cache entry for everyone else.

let cachedIndex: HubIndex | null = null;
let inflight: Promise<HubIndex> | null = null;
/** Guards against a superseded (forced) request landing after a newer one. */
let requestSeq = 0;

export async function fetchHubIndex(options: { force?: boolean } = {}): Promise<HubIndex> {
  const force = options.force === true;
  if (!force && cachedIndex) return cachedIndex;
  if (!force && inflight) return inflight;

  const seq = ++requestSeq;
  const promise = invoke<HubIndex>("fetch_hub_index").then((index) => {
    // Only the newest request may populate the cache.
    if (seq === requestSeq) cachedIndex = index;
    return index;
  });
  inflight = promise;
  try {
    return await promise;
  } finally {
    if (inflight === promise) inflight = null;
  }
}

/** Test seam: drop the session cache. */
export function resetHubIndexCache(): void {
  cachedIndex = null;
  inflight = null;
  requestSeq = 0;
}

/** Download + checksum verify + submit. See `commands/hub.rs`. */
export function installFromHub(input: {
  agentId: string;
  packageFile: string;
  sha256: string;
  nodeId: string | null;
  ensure: boolean;
}): Promise<HubInstallResult> {
  return invoke<HubInstallResult>("install_from_hub", {
    agentId: input.agentId,
    packageFile: input.packageFile,
    sha256: input.sha256,
    nodeId: input.nodeId,
    ensure: input.ensure,
  });
}

/** Local `.agent` file summary (extension + manifest id + size). */
export function inspectLocalPackage(packagePath: string): Promise<LocalPackageInfo> {
  return invoke<LocalPackageInfo>("inspect_local_package", { packagePath });
}

/**
 * Every `.agent` in the user's library, manifests already parsed.
 *
 * ONE call for N files (F13): the Rust side loops the directory and parses each
 * `manifest.toml`, so the webview never does N `invoke`s. Seeding happens
 * inside the same call, which is why the first open of the app is enough to
 * make the shipped packages appear.
 */
export function listLocalAgentPackages(): Promise<LocalAgentList> {
  return invoke<LocalAgentList>("list_local_agent_packages");
}

/**
 * Install a library file on `nodeId` idempotently (`/api/agents/ensure`).
 *
 * The row's primary button. `already_present` is the Gateway's answer, so a
 * second click converges instead of creating a second instance (REQ-026) — the
 * non-idempotent `/install` path stays behind the row menu (REQ-027).
 */
export function ensureLocalAgent(input: {
  packagePath: string;
  nodeId: string | null;
}): Promise<LocalAgentEnsureResult> {
  return invoke<LocalAgentEnsureResult>("ensure_local_agent", {
    packagePath: input.packagePath,
    nodeId: input.nodeId,
  });
}

/**
 * Outcome of the publish wizard's library write-back (REQ-029 / D12).
 *
 * Two legal outcomes, and the second one is **not** a failure:
 *
 *  - `saved`          — a single-machine deployment: the artifact the node
 *                       built was copied into the library, `path` is its new
 *                       home. A same-named file is replaced (one version is one
 *                       row); a different version lands beside it.
 *  - `remote_skipped` — the Gateway (and therefore the node that built the
 *                       package) is on another machine, so the artifact is not
 *                       readable here. D12 scopes the write-back to local
 *                       mode and PRD §14.6 makes remote transfer a non-goal, so
 *                       the wizard names the node and copies nothing.
 */
export type SaveToLibraryStatus = "saved" | "remote_skipped";

export interface SaveToLibraryResult {
  status: SaveToLibraryStatus;
  /** Absolute path inside the library — `null` for `remote_skipped`. */
  path: string | null;
  /** `{agent_id}-{version}.agent`, known in both outcomes. */
  file_name: string;
}

/**
 * Copy the package the publish build just produced into the local library.
 *
 * `packagePath` is the node-local `output_path` the Gateway returned. The
 * command re-validates it (it must be a direct `.agent` file inside the node's
 * `packages_dir`) and refuses `remote`/`relay` deployments without copying —
 * the frontend never decides that policy itself.
 */
export function savePublishToLibrary(packagePath: string): Promise<SaveToLibraryResult> {
  return invoke<SaveToLibraryResult>("save_publish_to_library", { packagePath });
}
