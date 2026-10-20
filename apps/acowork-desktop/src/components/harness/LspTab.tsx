import { useState, useEffect, useCallback, useMemo, type ReactNode } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useAgentStore } from "../../stores/agentStore";
import {
  addLspServer,
  deleteLspServer,
  fetchLspInstallScript,
  fetchLspPresets,
  fetchLspRegistry,
  fetchLspStatus,
  fetchLspStatusForLanguage,
  fetchNodes,
  getLspRelayUrl,
  lspAdminUrlFromRelayUrl,
  runLspInstall,
  updateLspServer,
} from "../../lib/gateway-api";
import { getGatewayUrl } from "../../lib/config";
import { useLspInstallStore } from "../../stores/lspInstallStore";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { Dropdown } from "../common/Dropdown";
import { StyledInput, StyledTextarea } from "../common/StyledInput";
import type {
  LspHealthStatus,
  LspRegistryView,
  LspRuntimeEntry,
  LspServerEntry,
  LspServerStatusEntry,
  NodeInfo,
} from "../../lib/types";
import {
  CheckCircle2,
  XCircle,
  Loader2,
  Eye,
  Terminal,
  Code2,
  Pencil,
  Plus,
  RefreshCw,
  Trash2,
} from "lucide-react";
import { ErrorBox } from "../common/ErrorBox";
import { ExpandableRow, ListBox, ListRow } from "../common/list";
import { Tooltip } from "../common/Tooltip";
import { OverlayScrollbar } from "../common/OverlayScrollbar";

/**
 * Module-level cache of LSP install-status results, keyed by relay URL.
 *
 * Purpose: avoid re-fetching (and triggering the relay's full PROBE storm)
 * when the user switches Chat → Harness → Chat → Harness in quick
 * succession. Each Harness re-entry remounts `LspTab` (AppLayout
 * unmounts `HarnessPage` on view switch — see AppLayout.tsx), so the
 * `useState` inside the component is reset. The module-level cache
 * survives unmount, so the second mount can `loadAll` and skip both
 * the "checking" flash AND the network call for any language whose
 * status was probed within `MODULE_HEALTH_CACHE_TTL_MS`.
 *
 * The relay itself also caches results (Phase 1) — the module-level
 * cache is purely a UX optimization to skip the "checking" spinner
 * flicker on re-mount, when the relay's cache is also likely fresh.
 *
 * Mirrors `MODULE_HEALTH_CACHE_TTL_MS` to the relay's
 * `DEFAULT_STATUS_TTL_SECS` (30 min) — if the two go out of sync
 * the worst case is a "checking" flash with a fast cache-hit
 * underneath (still no fork on the relay side).
 */
type ModuleHealthCache = {
  status: Record<string, LspHealthStatus>;
  timestamp: Record<string, number>;
};
const moduleHealthCache = new Map<string, ModuleHealthCache>();
const MODULE_HEALTH_CACHE_TTL_MS = 30 * 60 * 1000;

function getModuleHealthCache(relayUrl: string): ModuleHealthCache | null {
  return moduleHealthCache.get(relayUrl) ?? null;
}

function seedModuleHealthCache(
  relayUrl: string,
  entries: LspServerStatusEntry[],
): void {
  const existing = moduleHealthCache.get(relayUrl) ?? {
    status: {},
    timestamp: {},
  };
  const now = Date.now();
  for (const entry of entries) {
    existing.status[entry.language] = entry.installed
      ? "installed"
      : "not_installed";
    existing.timestamp[entry.language] = now;
  }
  moduleHealthCache.set(relayUrl, existing);
}

/** Language display names for UI */
const LANGUAGE_LABELS: Record<string, string> = {
  rust: "Rust",
  python: "Python",
  typescript: "TypeScript / JavaScript",
  go: "Go",
  c: "C / C++",
  json: "JSON",
  yaml: "YAML",
  html: "HTML",
  css: "CSS / SCSS / Less",
  markdown: "Markdown",
  java: "Java",
};

/**
 * Badge implied by the registry's persisted `install_state` (§5.8).
 *
 * A probe result always wins while it is on screen — the persisted value is
 * a snapshot the Node converged at some earlier point and may be up to
 * `CONVERGE_TTL` old. This is what lets the list render with a meaningful
 * badge before (or instead of) a probe: with the relay down there is no
 * probe to wait for, and "no LSP installed" would otherwise be a lie for
 * every row.
 */
function deriveHealth(entry: LspRuntimeEntry): LspHealthStatus {
  switch (entry.install_state) {
    case "installed":
    case "detected":
      return "installed";
    case "failed":
      return "not_installed";
    default:
      return "unknown";
  }
}

/** Comma-separated list field → trimmed, non-empty entries. */
function splitList(raw: string): string[] {
  return raw
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}

/** Space-separated argument field → entries (args are never comma-joined). */
function splitArgs(raw: string): string[] {
  return raw.split(/\s+/).map((s) => s.trim()).filter(Boolean);
}

function listToText(values: string[] | undefined): string {
  return (values ?? []).join(", ");
}

export function LspTab() {
  const { t } = useTranslation();
  const status = useGatewayStore((s) => s.status);
  // ADR-055 §6.7 (Phase 4): the relay is a node-local sidecar, so the
  // endpoint is resolved per agent — use the currently selected agent.
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const [registry, setRegistry] = useState<LspRegistryView | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** Live probe results, keyed by server name. Absent → derive from the
   *  registry's persisted `install_state` (see `deriveHealth`). */
  const [healthStatus, setHealthStatus] = useState<Record<string, LspHealthStatus>>({});
  const [healthErrors, setHealthErrors] = useState<Record<string, string | null>>({});
  const [checkingLangs, setCheckingLangs] = useState<Set<string>>(new Set());
  // Install progress lives in a store, not here: `LspTab` unmounts on
  // every view switch, and an install outlives that. See lspInstallStore.
  const installing = useLspInstallStore((s) => s.installing);
  const installOutcomes = useLspInstallStore((s) => s.outcomes);
  const beginInstall = useLspInstallStore((s) => s.beginInstall);
  const endInstall = useLspInstallStore((s) => s.endInstall);
  const installingSet = useMemo(
    // Union of the two truths: this client's own in-flight requests and the
    // Node's memory-only `installing` set, which also covers an install
    // started from another window (or still running after a remount).
    () => new Set([...installing, ...(registry?.installing ?? [])]),
    [installing, registry],
  );
  const [scriptDialog, setScriptDialog] = useState<{ name: string; script: string; filename: string; platform: string } | null>(null);
  const [scriptLoading, setScriptLoading] = useState(false);
  /** LSP list — Tools-tab level-1 collapsible group, default open. */
  const [serversOpen, setServersOpen] = useState(true);
  /** Custom-server create / edit dialog. `null` = closed. */
  const [form, setForm] = useState<{ mode: "add" } | { mode: "edit"; name: string } | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);
  /** Shown after a write the Node answered with `willRestart` (§5.6). */
  const [restartNotice, setRestartNotice] = useState<string | null>(null);

  // Which node's relay the panel is showing. The default follows the
  // selected agent, but once the user picks a node explicitly their choice
  // wins — otherwise switching agents in the sidebar would yank the panel
  // back to a different machine mid-inspection.
  const [nodeOverride, setNodeOverride] = useState<string | null>(null);
  // Nodes the panel can address. An ONLINE node is enough: the registry API
  // is served by the Node itself and deliberately works while its relay is
  // down — a missing language server is what brought the user here
  // (ADR-091 §5.5). Only the live probe needs the relay, and it degrades to
  // persisted badges when the relay is not ready.
  const [lspNodes, setLspNodes] = useState<NodeInfo[]>([]);

  // Discover the LSP Relay endpoint when Gateway is connected.
  useEffect(() => {
    if (status !== "connected" || !selectedAgentId) {
      return;
    }
    let cancelled = false;
    getLspRelayUrl(selectedAgentId)
      .catch(() => null)
      .then((url) => {
        if (!cancelled) setAgentRelayUrl(url);
      });
    return () => { cancelled = true; };
  }, [status, selectedAgentId]);

  const [agentRelayUrl, setAgentRelayUrl] = useState<string | null>(null);

  // Enumerate nodes the panel can manage. `selectedAgentId` stays the source
  // of truth for the default selection, so this runs whenever the agent
  // changes too — it is what turns "which node" into a visible choice.
  useEffect(() => {
    if (status !== "connected") {
      setLspNodes([]);
      return;
    }
    let cancelled = false;
    fetchNodes()
      .then((nodes) => {
        if (cancelled) return;
        setLspNodes(nodes.filter((n) => n.online));
      })
      .catch(() => {
        if (!cancelled) setLspNodes([]);
      });
    return () => { cancelled = true; };
  }, [status, selectedAgentId]);

  // A node override is only valid while that node is still online; a node
  // going offline drops back to the agent-derived default instead of
  // stranding the panel on a dead URL.
  const effectiveNodeId = useMemo(() => {
    if (nodeOverride && lspNodes.some((n) => n.node_id === nodeOverride)) {
      return nodeOverride;
    }
    return null;
  }, [nodeOverride, lspNodes]);

  // ADR-089 D3: the relay binds loopback on its node, so the panel
  // addresses it exclusively through the Gateway reverse proxy
  // (`{gateway}/api/nodes/{id}/lsp`) — the raw `lsp_endpoint` from
  // `/api/nodes` is the node-local topic value, never a client URL.
  // Null when the relay is not ready and no node was picked explicitly:
  // the registry stays usable, only the probe is skipped.
  const effectiveRelayUrl = useMemo(() => {
    if (effectiveNodeId) {
      return `${getGatewayUrl().replace(/\/+$/, "")}/api/nodes/${encodeURIComponent(effectiveNodeId)}/lsp`;
    }
    return agentRelayUrl;
  }, [effectiveNodeId, agentRelayUrl]);

  /** Registry-admin base URL for the node behind `effectiveRelayUrl` (§5.5). */
  const adminUrl = useMemo(
    () => (effectiveRelayUrl ? lspAdminUrlFromRelayUrl(effectiveRelayUrl) : null),
    [effectiveRelayUrl],
  );

  // The node the panel is actually talking to, matched by node id. Null when
  // the endpoint is one the caller may not see (ADR-087 D5 prunes it for
  // non-managers) — the panel still functions, it just cannot name the
  // machine.
  const shownNode = useMemo(() => {
    const m = effectiveRelayUrl
      ? /\/api\/nodes\/([^/]+)\/lsp\/?$/.exec(effectiveRelayUrl)
      : null;
    if (!m) return null;
    return lspNodes.find((n) => n.node_id === decodeURIComponent(m[1])) ?? null;
  }, [lspNodes, effectiveRelayUrl]);

  // Probe results belong to one node; carrying them over to the next would
  // paint another machine's badges.
  useEffect(() => {
    setHealthStatus({});
    setHealthErrors({});
    setRestartNotice(null);
  }, [adminUrl]);

  const loadAll = useCallback(async (options: { force?: boolean } = {}) => {
    if (!adminUrl) return;
    setRefreshing(true);
    setError(null);

    try {
      // Phase 1: read the Node's merged registry. This is a memory read on
      // the Node — milliseconds, no probe — and it is the authoritative
      // list (presets ∪ customs) plus the persisted install state.
      const view = await fetchLspRegistry(adminUrl);
      setRegistry(view);

      if (!effectiveRelayUrl) {
        // Relay not reachable: badges stay derived from `install_state`.
        // Say so once per load instead of leaving the rows unexplained.
        setHealthErrors((prev) => ({
          ...prev,
          ...Object.fromEntries(Object.keys(view.servers).map((k) => [k, t("harnessLsp.relayUnavailable")])),
        }));
        return;
      }

      // Flip every row from the neutral "pending" badge to the amber
      // "checking" badge so the user can see that the slow phase has
      // started. React batches this setState with the `setRegistry` call
      // above, so the list and the "checking" badges appear together in a
      // single paint.
      //
      // Skip the "checking" flip when `force = false` AND we already have a
      // non-pending cached status for every language — this is the fast path
      // on Tab re-mount: the user shouldn't see a "checking" flash if the LSP
      // Relay has fresh cached data.
      const cachedModule = getModuleHealthCache(effectiveRelayUrl);
      const names = Object.keys(view.servers);
      const allCachedFresh =
        !options.force &&
        cachedModule != null &&
        names.every((name) => {
          const ts = cachedModule.timestamp[name];
          return typeof ts === "number" && Date.now() - ts < MODULE_HEALTH_CACHE_TTL_MS;
        });
      if (names.length > 0 && !allCachedFresh) {
        setHealthStatus((prev) => {
          const next: Record<string, LspHealthStatus> = { ...prev };
          for (const name of names) {
            next[name] = "checking";
          }
          return next;
        });
      }

      // Phase 2: probe PATH per language (cached on the relay). The list is
      // already on screen; only badges update when the response arrives.
      try {
        const entries = await fetchLspStatus(effectiveRelayUrl, { force: options.force });
        setHealthStatus((prev) => {
          const next = { ...prev };
          for (const entry of entries) {
            next[entry.language] = entry.installed ? "installed" : "not_installed";
          }
          return next;
        });
        // Mirror the result into the module-level cache so a remount
        // (e.g. switching Chat → Harness → back to LSP) can skip the
        // network call entirely.
        seedModuleHealthCache(effectiveRelayUrl, entries);
      } catch (statusErr) {
        // Phase 1 succeeded but the status probe failed — the list is fine,
        // so we don't surface a page-level error. Leave the badges at their
        // persisted values (better than "error" on every row when the relay
        // is merely restarting) and stash the message for the row's error
        // label. The user can still hit Refresh.
        const msg = statusErr instanceof Error ? statusErr.message : "Status check failed";
        setHealthErrors((prev) => {
          const next = { ...prev };
          for (const name of names) {
            next[name] = msg;
          }
          return next;
        });
      }
    } catch (e) {
      // The registry read failed — the node is unreachable, or this caller
      // may not manage it (ADR-091 §5.7). Surface it in the page-level
      // ErrorBox; there is no list to show without it.
      setError(e instanceof Error ? e.message : "Failed to load LSP servers");
    } finally {
      setRefreshing(false);
    }
  }, [adminUrl, effectiveRelayUrl, t]);

  useEffect(() => {
    if (status === "connected" && adminUrl) {
      // Two-phase load (see `loadAll`): the list arrives almost
      // immediately, then badges resolve incrementally as PATH probes
      // complete on the server.
      void loadAll();
    }
  }, [status, adminUrl, loadAll]);

  /** Check if an LSP server is available by querying the relay's PATH lookup */
  const handleCheck = useCallback(async (language: string) => {
    if (!effectiveRelayUrl) return;
    setCheckingLangs((prev) => new Set(prev).add(language));
    setHealthStatus((prev) => ({ ...prev, [language]: "checking" }));
    setHealthErrors((prev) => ({ ...prev, [language]: null }));

    try {
      // Single-language endpoint: probes (or cache-hits) only this row.
      // The relay canonicalizes the language (e.g. "js" → "typescript") so
      // the response key matches the row key in our healthStatus map. A
      // custom server whose name is not a language canonicalises to itself,
      // so the one round-trip still addresses the right row.
      const entry = await fetchLspStatusForLanguage(effectiveRelayUrl, language);
      setHealthStatus((prev) => ({
        ...prev,
        [entry.language]: entry.installed ? "installed" : "not_installed",
      }));
      // Mirror the single-language result into the module-level cache
      // so a future batch load can use it without re-fetching.
      seedModuleHealthCache(effectiveRelayUrl, [entry]);
    } catch (e) {
      setHealthStatus((prev) => ({ ...prev, [language]: "error" }));
      setHealthErrors((prev) => ({
        ...prev,
        [language]: e instanceof Error ? e.message : "Status check failed",
      }));
    } finally {
      setCheckingLangs((prev) => {
        const next = new Set(prev);
        next.delete(language);
        return next;
      });
    }
  }, [effectiveRelayUrl]);

  /** View install script for a server (also the pre-run confirmation, D14) */
  const handleViewScript = useCallback(async (name: string) => {
    if (!adminUrl) return;
    setScriptLoading(true);
    setError(null);
    try {
      const resp = await fetchLspInstallScript(adminUrl, name);
      setScriptDialog({
        name: resp.name,
        script: resp.script,
        filename: resp.filename,
        platform: resp.platform,
      });
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load install script");
    } finally {
      setScriptLoading(false);
    }
  }, [adminUrl]);

  /** Run install script for a server on the Node (ADR-091 D15) */
  const handleInstall = useCallback(async (name: string) => {
    if (!adminUrl) return;
    setScriptDialog(null);
    beginInstall(name);
    setError(null);
    try {
      const result = await runLspInstall(adminUrl, name);
      // The script's own verdict first; the Node's post-install forced probe
      // (§5.10) then converges the persisted state in the background, which
      // the next load picks up.
      setHealthStatus((prev) => ({
        ...prev,
        [name]: result.success ? "installed" : "not_installed",
      }));
      endInstall(name, {
        success: result.success,
        stdout: result.stdout,
        stderr: result.stderr || result.failure || "",
      });
    } catch (e) {
      endInstall(name, {
        success: false,
        stdout: "",
        stderr: e instanceof Error ? e.message : "Install failed",
      });
    }
    // Re-read the registry: the in-flight mark must clear even when the
    // request itself failed, and `install_state` may already have converged.
    try {
      setRegistry(await fetchLspRegistry(adminUrl));
    } catch {
      /* the next Refresh will show it */
    }
  }, [adminUrl, beginInstall, endInstall]);

  /** Apply a create / edit dialog submit (§5.6: writes may restart the relay). */
  const handleSubmitForm = useCallback(async (name: string, metadata: LspServerEntry, mode: "add" | "edit") => {
    if (!adminUrl) return;
    const resp = mode === "add"
      ? await addLspServer(adminUrl, name, metadata)
      : await updateLspServer(adminUrl, name, metadata);
    if (resp.willRestart) {
      setRestartNotice(t("harnessLsp.relayRestarting"));
    }
    setForm(null);
    setRegistry(await fetchLspRegistry(adminUrl));
  }, [adminUrl, t]);

  const handleDelete = useCallback(async (name: string) => {
    if (!adminUrl) return;
    setDeleteTarget(null);
    try {
      const resp = await deleteLspServer(adminUrl, name);
      if (resp.willRestart) {
        setRestartNotice(t("harnessLsp.relayRestarting"));
      }
      setRegistry(await fetchLspRegistry(adminUrl));
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to delete LSP server");
    }
  }, [adminUrl, t]);

  if (status !== "connected") {
    return (
      <div className="max-w-lg">
        <p className="text-xs text-text-tertiary">{t("harnessLsp.connectToGateway")}</p>
      </div>
    );
  }

  const servers = registry?.servers ?? {};
  const serverEntries = Object.entries(servers);
  const conflicts = registry?.conflicts ?? [];

  // Which machine's registry this panel is showing. It rides in the card
  // header next to Refresh: every row below is that node's install state,
  // so a free-floating control above the card read as a separate form and
  // left the list looking global when it is per-node.
  const nodePicker = lspNodes.length > 0 && (
    <Dropdown
      id="lsp-node-picker"
      size="small"
      aria-label={t("harnessLsp.node")}
      value={shownNode?.node_id ?? ""}
      onChange={(v) => setNodeOverride(v)}
      className="max-w-[190px]"
      options={[
        // The agent's own node may fall outside the caller's manage list
        // (ADR-087 D5), leaving the panel on an endpoint it cannot name.
        // Show the host rather than an empty select.
        ...(effectiveRelayUrl && !shownNode
          ? [{
              value: "",
              // Unnamed host (outside the manage list): show the node id
              // embedded in the proxy URL rather than the whole URL
              // (ADR-089 D3 — the URL is plumbing, not a machine name).
              label:
                /\/api\/nodes\/([^/]+)\/lsp\/?$/.exec(effectiveRelayUrl)?.[1] ??
                effectiveRelayUrl.replace(/^https?:\/\//, ""),
            }]
          : []),
        ...lspNodes.map((n) => ({
          value: n.node_id,
          // node_name is the renameable display slug (ADR-075 D2); the id
          // is the disambiguator when two nodes share a slug.
          label: n.node_name ? `${n.node_name} (${n.node_id.slice(0, 8)})` : n.node_id,
        })),
      ]}
    />
  );

  return (
    <div className="max-w-2xl space-y-4">
      {/* LSP Servers — Tools-tab level-1 collapsible card: chevron + title +
          count badge. Node picker, Add and Refresh share the header trailing
          slot (both wrapped in stopPropagation so neither toggles the
          fold). */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={serversOpen}
          onToggle={() => setServersOpen((v) => !v)}
          title={t("harnessLsp.lspServerManagement", { count: serverEntries.length })}
          ariaLabel={t("harnessLsp.lspServerManagement", { count: serverEntries.length })}
          trailing={
            <span className="flex items-center gap-1.5" onClick={(e) => e.stopPropagation()}>
              {nodePicker}
              <Tooltip
                content={adminUrl ? t("harnessLsp.addServer") : t("harnessLsp.connectToGateway")}
                variant="plain"
              >
                <button
                  aria-label={t("harnessLsp.addServer")}
                  onClick={() => setForm({ mode: "add" })}
                  disabled={!adminUrl}
                  className="inline-flex items-center justify-center rounded h-6 w-6 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 disabled:opacity-40 dark:hover:bg-zinc-700 dark:text-zinc-300 transition-colors"
                >
                  <Plus className="h-3.5 w-3.5" />
                </button>
              </Tooltip>
              <Tooltip
                content={refreshing ? t("harnessLsp.refreshing") : t("harnessLsp.refresh")}
                variant="plain"
              >
                <button
                  aria-label={refreshing ? t("harnessLsp.refreshing") : t("harnessLsp.refresh")}
                  onClick={() => void loadAll({ force: true })}
                  disabled={refreshing || !adminUrl}
                  className="inline-flex items-center justify-center rounded h-6 w-6 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 disabled:opacity-60 dark:hover:bg-zinc-700 dark:text-zinc-300 transition-colors"
                >
                  {refreshing ? (
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  ) : (
                    <RefreshCw className="h-3.5 w-3.5" />
                  )}
                </button>
              </Tooltip>
            </span>
          }
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
        >
          {/* No online node to talk to — the card stays so the picker remains
              reachable, and the reason replaces the row list. */}
          {!adminUrl && (
            <div className="px-3 py-3 text-xs text-text-tertiary">
              {lspNodes.length > 0
                ? t("harnessLsp.relayUnavailable")
                : t("harnessLsp.noNodes")}
            </div>
          )}

          {/* Error message */}
          {error && (
            <div className="px-3 pt-2">
              <ErrorBox message={error} onClose={() => setError(null)} />
            </div>
          )}

          {/* A write that costs a relay restart, said out loud (§5.6-2) */}
          {restartNotice && (
            <div className="mx-3 mt-2 rounded bg-amber-100 px-2 py-1.5 text-10 text-amber-700 dark:bg-amber-900/30 dark:text-amber-400">
              {restartNotice}
            </div>
          )}

          {/* Collisions with the shipped suite (§5.3): the user's entry won
              and the preset's version is being ignored — say so rather than
              silently hiding it. */}
          {conflicts.length > 0 && (
            <div className="mx-3 mt-2 rounded bg-zinc-100 px-2 py-1.5 text-10 text-text-secondary dark:bg-zinc-700">
              {t("harnessLsp.conflicts", { names: conflicts.join(", ") })}
            </div>
          )}

          {/* Loading state */}
          {adminUrl && refreshing && serverEntries.length === 0 && (
            <div className="px-3 py-3 text-xs text-text-tertiary">{t("harnessLsp.loadingServers")}</div>
          )}

          {/* Empty state */}
          {adminUrl && !refreshing && serverEntries.length === 0 && (
            <div className="px-3 py-3 text-xs text-text-tertiary">{t("harnessLsp.noLspServers")}</div>
          )}

          {/* Server list — unified ListRow rows (hairline separators, inset
              hover), one per registry entry. */}
          {adminUrl && serverEntries.length > 0 && (
            <ListBox variant="plain">
              {serverEntries.map(([name, entry]) => (
                <LspServerCard
                  key={name}
                  name={name}
                  entry={entry}
                  healthStatus={healthStatus[name] ?? deriveHealth(entry)}
                  healthError={healthErrors[name] ?? null}
                  isChecking={checkingLangs.has(name)}
                  isInstalling={installingSet.has(name)}
                  installResult={installOutcomes[name] ?? null}
                  onCheck={() => handleCheck(name)}
                  onViewScript={() => handleViewScript(name)}
                  onInstall={() => handleInstall(name)}
                  onEdit={() => setForm({ mode: "edit", name })}
                  onDelete={() => setDeleteTarget(name)}
                />
              ))}
            </ListBox>
          )}
        </ExpandableRow>
      </ListBox>

      {/* Create / edit dialog */}
      {form && adminUrl && (
        <LspServerFormDialog
          mode={form.mode}
          editName={form.mode === "edit" ? form.name : undefined}
          initial={form.mode === "edit" ? servers[form.name] : undefined}
          adminUrl={adminUrl}
          onCancel={() => setForm(null)}
          onSubmit={handleSubmitForm}
        />
      )}

      {/* Delete confirmation — names the two consequences that are not
          obvious: the binary stays on disk, and the relay restarts. */}
      <ConfirmDialog
        open={deleteTarget !== null}
        title={t("harnessLsp.deleteServer")}
        message={
          `${t("harnessLsp.confirmDeleteServer", { name: deleteTarget ?? "" })} ${t("harnessLsp.willRestartWarning")}`
        }
        confirmLabel={t("common.delete")}
        onConfirm={() => deleteTarget && void handleDelete(deleteTarget)}
        onCancel={() => setDeleteTarget(null)}
      />

      {/* Install script dialog */}
      {scriptDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay">
          <OverlayScrollbar
            maxHeight="85vh"
            className="p-6"
            containerClassName="w-[600px] overflow-hidden rounded-md bg-modal-surface shadow-xl"
          >
            <div className="flex items-center justify-between mb-3">
              <h3 className="text-sm font-semibold">
                {t("harnessLsp.scriptContent")} — {LANGUAGE_LABELS[scriptDialog.name] ?? scriptDialog.name}
              </h3>
              <span className="rounded bg-zinc-100 px-2 py-0.5 text-10 font-mono text-text-tertiary dark:bg-zinc-700">
                {scriptDialog.filename}
              </span>
            </div>
            <pre className="max-h-96 overflow-auto rounded-md bg-zinc-50 p-4 text-11 leading-relaxed dark:bg-zinc-900/50">
              <code>{scriptDialog.script}</code>
            </pre>
            {/* D14: showing the script is the user's gate — the run button
                lives here so the text on screen is the text being run. */}
            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => setScriptDialog(null)}
                className="inline-flex items-center gap-1 rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium"
              >
                {t("harnessLsp.close")}
              </button>
              <button
                onClick={() => void handleInstall(scriptDialog.name)}
                className="inline-flex items-center gap-1 rounded btn-accent px-3 py-[var(--ui-btn-py)] text-xs font-medium"
              >
                <Terminal className="h-3.5 w-3.5" />
                {t("harnessLsp.runInstall")}
              </button>
            </div>
          </OverlayScrollbar>
        </div>
      )}

      {/* Script loading overlay */}
      {scriptLoading && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay">
          <div className="rounded-md bg-modal-surface p-6 shadow-xl">
            <Loader2 className="mx-auto h-6 w-6 animate-spin text-text-tertiary" />
            <p className="mt-2 text-xs text-text-tertiary">{t("harnessLsp.loading")}</p>
          </div>
        </div>
      )}
    </div>
  );
}

/** Individual LSP server card */
function LspServerCard({
  name,
  entry,
  healthStatus,
  healthError,
  isChecking,
  isInstalling,
  installResult,
  onCheck,
  onViewScript,
  onInstall,
  onEdit,
  onDelete,
}: {
  name: string;
  entry: LspRuntimeEntry;
  healthStatus: LspHealthStatus;
  healthError: string | null;
  isChecking: boolean;
  isInstalling: boolean;
  installResult: { success: boolean; stdout: string; stderr: string } | null;
  onCheck: () => void;
  onViewScript: () => void;
  onInstall: () => void;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const { t } = useTranslation();
  const [showOutput, setShowOutput] = useState(false);
  const langLabel = LANGUAGE_LABELS[name] ?? name;
  const isCustom = entry.source === "custom";

  return (
    <ListRow
      surface="inset"
      testId={`lsp-row-${name}`}
      trailing={
        /* Action cluster — mirrors the MCP row grammar */
        <div className="flex shrink-0 items-center gap-1.5">
          {/* Check button */}
          <button
            onClick={onCheck}
            disabled={isChecking}
            className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
          >
            {isChecking ? (
              <Loader2 className="h-3 w-3 animate-spin" />
            ) : (
              <Code2 className="h-3 w-3" />
            )}
            {isChecking ? t("harnessLsp.checking") : t("harnessLsp.checkStatus")}
          </button>

          {/* Edit / remove — custom rows are fully user-owned; a preset can
              only be tuned through `candidate_args` (D1/D6), so it gets the
              edit affordance but never a delete that would lie. */}
          <button
            onClick={onEdit}
            aria-label={t("harnessLsp.editServer")}
            className="inline-flex items-center justify-center rounded p-1.5 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:text-zinc-300 transition-colors"
          >
            <Pencil className="h-3 w-3" />
          </button>
          {isCustom && (
            <button
              onClick={onDelete}
              aria-label={t("harnessLsp.deleteServer")}
              className="inline-flex items-center justify-center rounded p-1.5 text-text-tertiary hover:bg-red-100 hover:text-red-600 dark:hover:bg-red-900/30 dark:text-zinc-300 transition-colors"
            >
              <Trash2 className="h-3 w-3" />
            </button>
          )}

          {/* View Script button */}
          {entry.install_script && (
            <button
              onClick={onViewScript}
              className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium"
            >
              <Eye className="h-3 w-3" />
              {t("harnessLsp.viewScript")}
            </button>
          )}

          {/* Install button — hidden once we know the server is installed.
              Mirrors the MCP Tab pattern: instead of a no-op button we show
              a green "installed" indicator in the action area. The user can
              still re-run Check to confirm the server actually responds to
              LSP protocol messages. */}
          {entry.install_script && healthStatus !== "installed" && (
            <button
              onClick={onInstall}
              disabled={isInstalling}
              className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
            >
              {isInstalling ? (
                <Loader2 className="h-3 w-3 animate-spin" />
              ) : (
                <Terminal className="h-3 w-3" />
              )}
              {isInstalling ? t("harnessLsp.installing") : t("harnessLsp.install")}
            </button>
          )}
          {entry.install_script && healthStatus === "installed" && (
            <span
              data-testid="lsp-installed-indicator"
              className="inline-flex items-center gap-1 rounded bg-green-100 px-2 py-1 text-11 font-medium text-green-700 dark:bg-green-900/30 dark:text-green-400"
            >
              <CheckCircle2 className="h-3 w-3" />
              {t("harnessLsp.installed")}
            </span>
          )}
        </div>
      }
    >
      {/* Header line — label + source + health indicator. Order matters:
          - "unknown": neither a probe nor a convergence has resolved this
            row. Renders a neutral pending badge so the row is never empty.
          - "checking": a probe is in flight (either the auto probe
            triggered by loadAll / Refresh, or a manual per-row Check).
          - "installed" / "not_installed": live probe result when one is on
            screen, otherwise the persisted `install_state` (§5.8).
          - "error": error message is rendered below. */}
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-xs font-semibold">{langLabel}</span>
        <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-10 text-text-tertiary dark:bg-zinc-700">
          {entry.source === "preset" ? t("harnessLsp.sourcePreset") : t("harnessLsp.sourceCustom")}
        </span>
        {healthStatus === "unknown" && (
          <span
            data-testid="lsp-pending-badge"
            className="inline-flex items-center gap-1 rounded bg-zinc-100 px-1.5 py-0.5 text-10 text-text-secondary dark:bg-zinc-700 "
          >
            <Loader2 className="h-2.5 w-2.5 animate-spin" />
            {t("harnessLsp.pendingCheck")}
          </span>
        )}
        {healthStatus === "checking" && (
          <span
            data-testid="lsp-checking-badge"
            className="inline-flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-10 text-amber-700 dark:bg-amber-900/30 dark:text-amber-400"
          >
            <Loader2 className="h-2.5 w-2.5 animate-spin" />
            {t("harnessLsp.checking")}
          </span>
        )}
        {healthStatus === "installed" && (
          <span className="inline-flex items-center gap-1 rounded bg-green-100 px-1.5 py-0.5 text-10 text-green-700 dark:bg-green-900/30 dark:text-green-400">
            <CheckCircle2 className="h-2.5 w-2.5" />
            {t("harnessLsp.installed")}
          </span>
        )}
        {/* A running install owns the badge. Without this the probe that
            lands mid-install (binary not on PATH yet) repaints the row as
            "not installed" while the script is still running. */}
        {isInstalling && (
          <span
            data-testid="lsp-installing-badge"
            className="inline-flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-10 text-amber-700 dark:bg-amber-900/30 dark:text-amber-400"
          >
            <Loader2 className="h-2.5 w-2.5 animate-spin" />
            {t("harnessLsp.installing")}
          </span>
        )}
        {healthStatus === "not_installed" && !isInstalling && (
          <span className="inline-flex items-center gap-1 rounded bg-red-100 px-1.5 py-0.5 text-10 text-red-700 dark:bg-red-900/30 dark:text-red-400">
            <XCircle className="h-2.5 w-2.5" />
            {entry.install_state === "failed"
              ? t("harnessLsp.installFailed")
              : t("harnessLsp.notInstalled")}
          </span>
        )}
        {healthStatus === "error" && (
          <span className="inline-flex items-center gap-1 rounded bg-red-100 px-1.5 py-0.5 text-10 text-red-700 dark:bg-red-900/30 dark:text-red-400">
            <XCircle className="h-2.5 w-2.5" />
            {t("harnessLsp.checkFailed")}
          </span>
        )}
      </div>

      {/* Description */}
      {entry.description && (
        <p className="mt-0.5 text-10 text-text-tertiary  line-clamp-1">
          {entry.description}
        </p>
      )}

      {/* Health error */}
      {healthError && (
        <p className="mt-1.5 text-10 text-red-500 break-all">{healthError}</p>
      )}

      {/* Candidates list */}
      {entry.candidates.length > 0 && (
        <div className="mt-1.5 flex flex-wrap items-center gap-1">
          <span className="text-10 text-text-tertiary">{t("harnessLsp.candidates")}:</span>
          {entry.candidates.map((cmd) => (
            <code
              key={cmd}
              className="rounded bg-zinc-100 px-1.5 py-0.5 text-10 font-mono text-text-secondary dark:bg-zinc-700 "
            >
              {cmd}
            </code>
          ))}
        </div>
      )}

      {/* Install hint */}
      {entry.install_hint && (
        <div className="mt-1.5 flex items-center gap-1">
          <span className="text-10 text-text-tertiary">{t("harnessLsp.installHint")}:</span>
          <code className="rounded bg-zinc-100 px-1.5 py-0.5 text-10 font-mono text-amber-600 dark:bg-zinc-700 dark:text-amber-400">
            {entry.install_hint}
          </code>
        </div>
      )}

      {/* Persisted install output (ADR-091 §5.2): survives a remount and a
          node restart, unlike `installResult` below. */}
      {!installResult && entry.install_state === "failed" && (entry.install_stderr || entry.install_stdout) && (
        <div className="mt-1.5">
          <button
            onClick={() => setShowOutput(!showOutput)}
            className="text-10 text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-300"
          >
            {showOutput ? "Hide output" : "Show output"}
          </button>
          {showOutput && (
            <pre className="mt-1 max-h-40 overflow-auto rounded-md bg-zinc-50 p-2 text-10 leading-relaxed dark:bg-zinc-900/50">
              <code>{entry.install_stderr || entry.install_stdout}</code>
            </pre>
          )}
        </div>
      )}

      {/* Install result output */}
      {installResult && (
        <div className="mt-1.5">
          <div className="flex items-center gap-2 mb-1">
            {installResult.success ? (
              <span className="inline-flex items-center gap-1 text-10 text-green-600 dark:text-green-400">
                <CheckCircle2 className="h-2.5 w-2.5" />
                {t("harnessLsp.installSuccess")}
              </span>
            ) : (
              <span className="inline-flex items-center gap-1 text-10 text-red-600 dark:text-red-400">
                <XCircle className="h-2.5 w-2.5" />
                {t("harnessLsp.installFailed")}
              </span>
            )}
            <button
              onClick={() => setShowOutput(!showOutput)}
              className="text-10 text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-300"
            >
              {showOutput ? "Hide output" : "Show output"}
            </button>
          </div>
          {showOutput && (
            <pre className="max-h-40 overflow-auto rounded-md bg-zinc-50 p-2 text-10 leading-relaxed dark:bg-zinc-900/50">
              <code>{installResult.stdout || installResult.stderr || "(no output)"}</code>
            </pre>
          )}
        </div>
      )}
    </ListRow>
  );
}

/**
 * Create / edit dialog for one registry entry (ADR-091 D4 / D6).
 *
 * Two shapes in one form, because the Node enforces exactly this split:
 * a `custom` entry owns its whole metadata block, while a `preset` entry
 * only keeps `candidate_args` (the merge refreshes the rest from the shipped
 * suite on the next start, so editing it would silently revert).
 */
function LspServerFormDialog({
  mode,
  editName,
  initial,
  adminUrl,
  onCancel,
  onSubmit,
}: {
  mode: "add" | "edit";
  /** The registry key being edited (absent for a create). */
  editName?: string;
  initial?: LspRuntimeEntry;
  adminUrl: string;
  onCancel: () => void;
  onSubmit: (name: string, metadata: LspServerEntry, mode: "add" | "edit") => Promise<void>;
}) {
  const { t } = useTranslation();
  const isPreset = initial?.source === "preset";
  const [name, setName] = useState("");
  const [candidates, setCandidates] = useState(listToText(initial?.candidates));
  const [args, setArgs] = useState((initial?.args ?? []).join(" "));
  const [installHint, setInstallHint] = useState(initial?.install_hint ?? "");
  const [installScript, setInstallScript] = useState(initial?.install_script ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [rootMarkers, setRootMarkers] = useState(listToText(initial?.root_markers));
  /** Per-candidate overrides, edited as free text keyed by candidate. */
  const [candidateArgs, setCandidateArgs] = useState<Record<string, string>>(
    Object.fromEntries(
      Object.entries(initial?.candidate_args ?? {}).map(([k, v]) => [k, v.join(" ")]),
    ),
  );
  // Preset edit: the candidate list is fixed by the suite, so the rows to
  // edit are exactly those candidates — there is nothing to add or drop.
  const overrideRows = isPreset ? initial?.candidates ?? [] : [];
  const [presets, setPresets] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  /** §5.6-2: a config write costs a relay restart that drops the editors'
   *  live LSP WebSocket, so the submit passes an explicit confirmation
   *  instead of firing on one click. */
  const [confirming, setConfirming] = useState(false);

  // The shipped suite is the candidate source for a new entry (D1): starting
  // from a preset copy is the common case, and typing a key that already
  // exists as a preset would only produce a collision (§5.3).
  useEffect(() => {
    if (mode !== "add") return;
    let cancelled = false;
    fetchLspPresets(adminUrl)
      .then((cfg) => {
        if (!cancelled) setPresets(Object.keys(cfg.servers));
      })
      .catch(() => {
        /* the form still works without the copy-from list */
      });
    return () => { cancelled = true; };
  }, [mode, adminUrl]);

  const applyPreset = (presetName: string) => {
    if (!presetName) return;
    setName(presetName);
    void (async () => {
      try {
        const cfg = await fetchLspPresets(adminUrl);
        const p = cfg.servers[presetName];
        if (!p) return;
        setCandidates(listToText(p.candidates));
        setArgs(p.args.join(" "));
        setInstallHint(p.install_hint);
        setInstallScript(p.install_script ?? "");
        setDescription(p.description);
        setRootMarkers(listToText(p.root_markers));
      } catch {
        /* keep the typed name; the fields stay blank */
      }
    })();
  };

  const submit = async () => {
    setBusy(true);
    setFormError(null);
    // A preset's candidate list is fixed by the suite; a custom entry's
    // override rows follow whatever is typed in the candidates field.
    const overrideCandidates = isPreset ? overrideRows : splitList(candidates);
    const metadata: LspServerEntry = {
      candidates: splitList(candidates),
      args: splitArgs(args),
      candidate_args: Object.fromEntries(
        overrideCandidates
          .map((c) => [c, splitArgs(candidateArgs[c] ?? "")] as const)
          .filter(([, list]) => list.length > 0),
      ),
      install_hint: installHint,
      install_script: installScript.trim() || undefined,
      description,
      root_markers: splitList(rootMarkers),
    };
    try {
      await onSubmit(mode === "add" ? name.trim() : (editName ?? ""), metadata, mode);
    } catch (e) {
      // Registry refusals are user-facing (`invalid name`, `already exists`,
      // `preset cannot be removed`) — show them in the dialog, not the page.
      setFormError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay" role="dialog" aria-modal="true">
      <OverlayScrollbar
        maxHeight="85vh"
        className="p-6"
        containerClassName="w-[560px] overflow-hidden rounded-md bg-modal-surface shadow-xl"
      >
        <h3 className="text-sm font-semibold mb-4">
          {mode === "add" ? t("harnessLsp.addServer") : t("harnessLsp.editServer")}
        </h3>

        {isPreset && (
          <p className="mb-3 rounded bg-zinc-100 px-2 py-1.5 text-10 text-text-secondary dark:bg-zinc-700">
            {t("harnessLsp.presetEditNote")}
          </p>
        )}

        <div className="space-y-3">
          {mode === "add" && (
            <>
              <Field label={t("harnessLsp.copyFromPreset")}>
                <Dropdown
                  id="lsp-copy-from-preset"
                  size="small"
                  aria-label={t("harnessLsp.copyFromPreset")}
                  value=""
                  onChange={applyPreset}
                  options={[
                    { value: "", label: t("harnessLsp.presetNone") },
                    ...presets.map((p) => ({ value: p, label: p })),
                  ]}
                />
              </Field>
              <Field label={t("harnessLsp.formName")}>
                <StyledInput
                  fontMono
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder="my-languageserver"
                />
              </Field>
            </>
          )}

          {!isPreset && (
            <>
              <Field label={t("harnessLsp.formCandidates")}>
                <StyledInput
                  fontMono
                  value={candidates}
                  onChange={(e) => setCandidates(e.target.value)}
                  placeholder="rust-analyzer, rustup"
                />
              </Field>
              <Field label={t("harnessLsp.formArgs")}>
                <StyledInput
                  fontMono
                  value={args}
                  onChange={(e) => setArgs(e.target.value)}
                />
              </Field>
            </>
          )}

          <Field label={t("harnessLsp.formCandidateArgs")}>
            <div className="space-y-1.5">
              {(isPreset ? overrideRows : splitList(candidates)).map((c) => (
                <div key={c} className="flex items-center gap-2">
                  <code className="w-40 shrink-0 truncate rounded bg-zinc-100 px-1.5 py-0.5 text-10 font-mono text-text-secondary dark:bg-zinc-700">
                    {c}
                  </code>
                  <StyledInput
                    fontMono
                    className="flex-1"
                    value={candidateArgs[c] ?? ""}
                    placeholder="--stdio"
                    onChange={(e) => setCandidateArgs((prev) => ({ ...prev, [c]: e.target.value }))}
                  />
                </div>
              ))}
            </div>
          </Field>

          {!isPreset && (
            <>
              <Field label={t("harnessLsp.formDescription")}>
                <StyledTextarea rows={2} value={description} onChange={(e) => setDescription(e.target.value)} />
              </Field>
              <Field label={t("harnessLsp.formRootMarkers")}>
                <StyledInput
                  fontMono
                  value={rootMarkers}
                  onChange={(e) => setRootMarkers(e.target.value)}
                  placeholder="Cargo.toml, package.json"
                />
              </Field>
              <Field label={t("harnessLsp.formInstallHint")}>
                <StyledInput fontMono value={installHint} onChange={(e) => setInstallHint(e.target.value)} />
              </Field>
              <Field label={t("harnessLsp.formInstallScript")}>
                <StyledInput
                  fontMono
                  value={installScript}
                  onChange={(e) => setInstallScript(e.target.value)}
                  placeholder="rust"
                />
              </Field>
            </>
          )}

          {formError && (
            <div className="rounded bg-red-100 px-2 py-1.5 text-10 text-red-700 dark:bg-red-900/30 dark:text-red-400">
              {formError}
            </div>
          )}
        </div>

        <div className="mt-5 flex justify-end gap-2">
          <button
            onClick={onCancel}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 dark:hover:bg-zinc-700"
          >
            {t("common.cancel")}
          </button>
          <button
            onClick={() => setConfirming(true)}
            disabled={busy || (mode === "add" && !name.trim())}
            className="inline-flex items-center gap-1 rounded btn-accent px-3 py-1.5 text-xs font-medium disabled:opacity-50"
          >
            {busy && <Loader2 className="h-3 w-3 animate-spin" />}
            {t("common.confirm")}
          </button>
        </div>
      </OverlayScrollbar>

      {/* Gate on the restart cost (§5.6-2): a note the user scrolls past is
          not a confirmation. */}
      <ConfirmDialog
        open={confirming}
        title={mode === "add" ? t("harnessLsp.addServer") : t("harnessLsp.editServer")}
        message={t("harnessLsp.willRestartWarning")}
        confirmLabel={t("common.confirm")}
        onConfirm={() => {
          setConfirming(false);
          void submit();
        }}
        onCancel={() => setConfirming(false)}
      />
    </div>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="block">
      <span className="mb-1 block text-10 font-medium text-text-tertiary">{label}</span>
      {children}
    </label>
  );
}
