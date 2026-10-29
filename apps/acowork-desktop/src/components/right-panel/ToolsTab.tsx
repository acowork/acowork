import { useCallback, useEffect, useMemo, useState } from "react";
import { Loader2 } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useMcpStore, mcpEnsureKey, type McpEnsureStatus } from "../../stores/mcpStore";
import { getGatewayUrl } from "../../lib/config";
import { with503Retry } from "../../lib/httpRetry";
import { log } from "../../lib/logger";
import { useTranslation } from "../../i18n/useTranslation";
import { Switch } from "../common/Switch";
import { ListBox, ListRow, ExpandableRow, Badge, EmptyState } from "../common/list";
import type {
  McpServerView,
  AgentMcpToolItem,
  McpRecommendationView,
} from "../../lib/types";
import { McpRecommendationGroup } from "./McpRecommendationGroup";
import { OverlayScrollbar } from "../common/OverlayScrollbar";

const EMPTY_ARRAY: string[] = [];
// Stable empty-object reference for Zustand selectors (see line ~178 below).
// Returning a fresh `{}` from a selector on every render trips
// `useSyncExternalStore`'s Object.is equality check → infinite re-render →
// "Maximum update depth exceeded". Module-level constants are the canonical
// Zustand workaround and mirror the EMPTY_ARRAY pattern above.
const EMPTY_OBJECT: Record<string, never> = {};

interface BuiltinToolEntry {
  name: string;
  enabled: boolean;
}

// ── Sub-component: collapsible MCP server card ────────────────────────

interface McpServerCardProps {
  server: McpServerView;
  /** Friendly alias joined from the Gateway catalog by `server.name` (see
   *  the parent). The Runtime's McpServerView doesn't carry display_name
   *  (it isn't part of the MQTT McpRef push), so the Desktop resolves it
   *  client-side against the catalog it already loads. */
  displayName?: string;
  isChecked: boolean;
  /** ADR-069: complete per-tool list advertised by `server`, each row
   *  carrying `name` + `enabled` + `description`. Sourced entirely
   *  from the backend `GET /agents/{id}/mcp-tools` response — the
   *  frontend never maintains its own tool list or defaults. */
  tools: AgentMcpToolItem[];
  /** True between the Switch flip and the moment the per-server tool
   *  list (reconciled via the post-toggle `/mcp-tools` re-fetch) has
   *  settled. Drives the inline spinner so a 5s `playwright` reconnect
   *  does not look like a stuck UI. */
  loading: boolean;
  onToggleTool: (toolName: string) => void;
  onToggleServer: () => void;
  switchDisabled: boolean;
  toolSwitchDisabled: boolean;
  /** ADR-090: first-enable ensure status on THIS machine (undefined =
   *  nothing being ensured — custom/local servers, or already usable).
   *  Drives the 探测中/安装中/失败 badge and the failure detail line. */
  ensure?: McpEnsureStatus;
}

/**
 * Collapsible MCP server card (ADR-068 UX, ADR-069 data).
 *
 * Layout mirrors the Debug-panel card style:
 *   - Header row: ChevronDown/Right + server name + transport badge +
 *     activation Switch. Clicking anywhere on the header toggles
 *     collapse; the Switch stops propagation so toggling activation
 *     does not also collapse.
 *   - Body (when open): separated by `border-t`, one tool per row —
 *     name + description on the left, a unified Switch on the right
 *     (same visual contract as the Builtin Tools list). Deep left
 *     indent so the nested tools read as children of the server card.
 *
 * Default collapsed. A future enhancement could persist the open
 * state per-server in localStorage; for v1 we keep it in-memory only.
 */
function McpServerCard({
  server,
  displayName,
  isChecked,
  tools,
  loading,
  onToggleTool,
  onToggleServer,
  switchDisabled,
  toolSwitchDisabled,
  ensure,
}: McpServerCardProps) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  // ADR-068/069 UX: the chevron is only meaningful when there is
  // actually something to expand — the server is active AND its tool
  // list is available. An inactive server (e.g. `playwright` before it
  // is enabled) or one with no reconciled tool data would otherwise
  // show a right-arrow that expands to an empty body, which reads as
  // broken. Hide the chevron in that case (ExpandableRow disabled);
  // enabling the server (PUT /mcp-servers → reconnect → reconcile)
  // materialises the tool list and the chevron appears.
  const hasExpandableBody = isChecked && tools.length > 0;
  const showBody = open && hasExpandableBody;
  // Note: no outer ListBox here — the caller places each server row
  // inside the MCP group card's plain body list (Debug-panel style),
  // which owns the hairline separation between rows.
  return (
    <ExpandableRow
      open={showBody}
      onToggle={() => setOpen((v) => !v)}
      disabled={!hasExpandableBody}
      title={`${displayName?.trim() || server.name}${tools.length > 0 ? ` (${tools.length})` : ""}`}
      meta={
        // Inline spinner on the right of the title — visible from the
        // moment the user flips the Switch until the post-reconcile
        // `/mcp-tools` reload settles, so a multi-second third-party
        // MCP reconnect (e.g. `playwright` ~5s) does not look like a
        // stuck UI. The spinner replaces the transport badge while
        // loading to keep the meta slot single-width and avoid a
        // layout shift when it disappears.
        ensure?.state === "probing" || ensure?.state === "installing" ? (
          <span
            className="inline-flex items-center gap-1 text-10 text-text-tertiary"
            data-testid={`mcp-ensure-${server.name}`}
          >
            <Loader2 className="h-3 w-3 animate-spin" />
            {ensure.state === "probing"
              ? t("agentSetup.ensureProbing")
              : t("agentSetup.ensureInstalling")}
          </span>
        ) : ensure?.state === "failed" ? (
          <span
            className="inline-flex items-center gap-1 text-10 text-red-500"
            data-testid={`mcp-ensure-${server.name}`}
          >
            {t("agentSetup.ensureFailed")}
          </span>
        ) : loading ? (
          <Loader2
            className="h-3 w-3 animate-spin text-text-tertiary "
            aria-label={`Connecting ${server.name}`}
          />
        ) : (
          <Badge>{server.transport}</Badge>
        )
      }
      description={
        ensure?.state === "failed" && ensure.error
          ? ensure.error
          : server.command || server.url || ""
      }
      trailing={
        // The Switch component owns its own click handler; we stop
        // propagation so clicking the activation toggle does not
        // also collapse/expand the row.
        <span onClick={(e) => e.stopPropagation()}>
          <Switch
            checked={isChecked}
            onChange={onToggleServer}
            disabled={switchDisabled}
            size="sm"
            aria-label={server.name}
          />
        </span>
      }
      surface="inset"
      bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset-2"
      ariaLabel={`Toggle ${server.name} tools`}
    >
      {/* Body — one row per tool, name on the left, Switch on the
          right (same visual contract as the Builtin Tools list) with a
          deep left indent so the nested tools read clearly as
          "children of the server row". Right edge stays aligned with
          the header Switch column. */}
      <ListBox variant="plain">
        {tools.map((tool) => (
          <ListRow
            key={tool.name}
            padding="nested"
            surface="inset"
            trailing={
              <Switch
                checked={tool.enabled}
                onChange={() => onToggleTool(tool.name)}
                disabled={toolSwitchDisabled}
                size="sm"
                aria-label={tool.name}
              />
            }
          >
            <span className="block truncate font-mono text-11 font-medium text-text-secondary ">
              {tool.name}
            </span>
            {tool.description && (
              <span className="block truncate text-9 leading-tight text-text-tertiary ">
                {tool.description}
              </span>
            )}
          </ListRow>
        ))}
      </ListBox>
    </ExpandableRow>
  );
}

// ── Component ───────────────────────────────────────────────────────────

export function ToolsTab() {
  const { t } = useTranslation();
  const { selectedAgentId } = useAgentStore();
  const selectedAgent = useAgentStore((s) => s.selectedAgentId ? s.agents[s.selectedAgentId]?.meta : undefined);

  // MCP server activation — per-agent selectors.
  // The toggle list is rendered from the per-agent merged MCP list
  // (`mcp_servers_defs` = catalog ∪ local) fetched from the merged
  // /tools endpoint, so agent-installed (local) and system-injected
  // (pm) servers appear here too — not just gateway-global catalog
  // entries. Checked state comes from `activeServers` (active_names).
  const activeServers = useMcpStore((s) => selectedAgentId ? (s.activeServers[selectedAgentId] ?? EMPTY_ARRAY) : EMPTY_ARRAY);
  const activationLoading = useMcpStore((s) => selectedAgentId ? (s.activationLoading[selectedAgentId] ?? false) : false);
  // Per-server reconcile-pending flags keyed by the current agent.
  // Re-evaluated on every store change so a flip in any server's flag
  // re-renders the matching card and shows/hides its spinner.
  const perServerLoading = useMcpStore((s) => selectedAgentId ? (s.perServerLoading[selectedAgentId] ?? EMPTY_OBJECT) : EMPTY_OBJECT);
  // ADR-090: ensure-usable status per server (probing/installing/failed
  // badges on the card; undefined once usable or for custom servers).
  const ensureStatus = useMcpStore((s) => s.ensureStatus);
  const mcpError = useMcpStore((s) => s.error);
  const toggleServer = useMcpStore((s) => s.toggleServer);
  const clearServerLoading = useMcpStore((s) => s.clearServerLoading);

  // Resolve each server's friendly alias from the Gateway catalog (the
  // source of truth the user edits display_name in). The per-agent
  // McpServerView from the Runtime doesn't carry it, so join by name here.
  const catalog = useMcpStore((s) => s.catalog);
  const loadCatalog = useMcpStore((s) => s.loadCatalog);
  useEffect(() => {
    if (catalog.length === 0) void loadCatalog();
  }, [catalog.length, loadCatalog]);
  const displayNameByName = useMemo(() => {
    const m = new Map<string, string>();
    for (const c of catalog) {
      const d = c.display_name?.trim();
      if (d) m.set(c.name, d);
    }
    return m;
  }, [catalog]);

  // Per-agent MCP server definitions (catalog ∪ local) from merged /tools.
  const [mcpServerDefs, setMcpServerDefs] = useState<McpServerView[]>([]);

  // ADR-029: Builtin tools configuration
  const [builtinToolsAll, setBuiltinToolsAll] = useState<BuiltinToolEntry[]>([]);
  const [builtinSaving, setBuiltinSaving] = useState(false);

  // ADR-069: per-MCP-server complete tool list. `mcpToolsConfig` maps
  // server name → full `AgentMcpToolItem[]` (name + enabled +
  // description) fetched from `GET /agents/{id}/mcp-tools`. The
  // backend reconciles this against the live `tools/list` at connect
  // time, so the frontend simply renders what the server sends — it
  // never maintains its own tool list or defaults.
  const [mcpToolsConfig, setMcpToolsConfig] = useState<Record<string, AgentMcpToolItem[]>>({});
  const [mcpToolsSaving, setMcpToolsSaving] = useState(false);

  // Group-level collapse state for the two tool cards (Builtin Tools /
  // MCP). Each card mirrors the Debug-panel "Context Snapshots" level-1
  // collapsible style; default open so the previous always-visible
  // behavior is preserved on first mount.
  const [builtinOpen, setBuiltinOpen] = useState(true);
  const [mcpOpen, setMcpOpen] = useState(true);

  // ADR-104: the package's `[[mcp_servers]]` recommendations, with the
  // lifecycle state the Runtime derived. Rendered in its own group above the
  // MCP switch list; `reloadToken` lets that group force the authoritative
  // re-read after its own mutation instead of patching local state.
  const [recommendations, setRecommendations] = useState<McpRecommendationView[]>([]);
  const [reloadToken, setReloadToken] = useState(0);
  const reloadTools = useCallback(() => setReloadToken((n) => n + 1), []);

  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;

    // Tools, MCP servers — fetch once from merged /tools endpoint.
    // ADR-034 Phase 5: Replaces 2 separate calls (config, mcp-servers).
    // Bug B v3 fix: the merged `/tools` endpoint proxies through the
    // Runtime and 503s during the boot window. `with503Retry` rides out
    // the transient 503 so the RightPanel/Tools tab does not have to
    // gate on `meta.lifecycle` (ADR-085; formerly `meta.ready`) — same
    // root-cause as `fetchWorkspaces` /
    // `fetchTree` / `fetchNodes` / `fetchLatestSession`.
    (async () => {
      try {
        const resp = await with503Retry(
          () => fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/tools`),
          { tag: `ToolsTab.fetchTools(${selectedAgentId})`, logger: log },
        );
        if (resp.ok && !cancelled) {
          const data = await resp.json();
          // tools — builtin tools list
          if (data.tools && Array.isArray(data.tools)) {
            setBuiltinToolsAll(data.tools as BuiltinToolEntry[]);
          }
          // mcp_servers — active MCP server names
          if (data.mcp_servers && Array.isArray(data.mcp_servers)) {
            useMcpStore.setState((s) => ({
              activeServers: { ...s.activeServers, [selectedAgentId!]: data.mcp_servers },
            }));
          }
          // mcp_servers_defs — full per-agent MCP list (catalog ∪ local)
          // with active flags; powers the per-agent toggle rows so pm /
          // agent-installed MCPs show up too.
          if (data.mcp_servers_defs && Array.isArray(data.mcp_servers_defs)) {
            setMcpServerDefs(data.mcp_servers_defs as McpServerView[]);
          }
          // ADR-104: manifest-declared recommendations (absent when the
          // package declares none — `skip_serializing_if` on the wire).
          setRecommendations(
            Array.isArray(data.recommended)
              ? (data.recommended as McpRecommendationView[])
              : [],
          );
        }

        // ADR-069: per-tool opt-in list — separate endpoint from the
        // merged /tools view (which has no `mcp_tools` field). The
        // backend is the single source of truth: it reconciles
        // agent_mcp_tools.json against the live MCP `tools/list` at
        // connect time, so this is the complete list the frontend
        // renders directly (name + enabled + description).
        try {
          const mcpToolsResp = await with503Retry(
            () => fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/mcp-tools`),
            { tag: `ToolsTab.fetchMcpTools(${selectedAgentId})`, logger: log },
          );
          if (mcpToolsResp.ok && !cancelled) {
            const mcpToolsData = await mcpToolsResp.json();
            if (mcpToolsData.servers && typeof mcpToolsData.servers === "object") {
              setMcpToolsConfig(mcpToolsData.servers as Record<string, AgentMcpToolItem[]>);
            }
          }
        } catch { /* agent not ready — user can re-open tab later */ }
      } catch { /* agent not ready — user can re-open tab later */ }
    })();

    return () => { cancelled = true; };
  }, [selectedAgentId, reloadToken]);

  // Refresh on global events (e.g. after MCP toggle)
  useEffect(() => {
    if (!selectedAgentId) return;
    const handler = async (e: Event) => {
      const ce = e as CustomEvent<{ agentId: string }>;
      if (ce.detail?.agentId !== selectedAgentId) return;
      try {
        const resp = await fetch(
          `${getGatewayUrl()}/api/agents/${selectedAgentId}/tools`,
        );
        if (!resp.ok) return;
        const data = await resp.json();
        if (data.tools && Array.isArray(data.tools)) {
          setBuiltinToolsAll(data.tools as BuiltinToolEntry[]);
        }
        if (data.mcp_servers && Array.isArray(data.mcp_servers)) {
          useMcpStore.setState((s) => ({
            activeServers: { ...s.activeServers, [selectedAgentId!]: data.mcp_servers },
          }));
        }
        if (data.mcp_servers_defs && Array.isArray(data.mcp_servers_defs)) {
          setMcpServerDefs(data.mcp_servers_defs as McpServerView[]);
        }
        setRecommendations(
          Array.isArray(data.recommended)
            ? (data.recommended as McpRecommendationView[])
            : [],
        );
        // ADR-069: refresh the per-tool opt-in list from its dedicated
        // endpoint (the merged /tools view has no `mcp_tools` field).
        try {
          const mcpToolsResp = await fetch(
            `${getGatewayUrl()}/api/agents/${selectedAgentId}/mcp-tools`,
          );
          if (mcpToolsResp.ok) {
            const mcpToolsData = await mcpToolsResp.json();
            if (mcpToolsData.servers && typeof mcpToolsData.servers === "object") {
              const fresh = mcpToolsData.servers as Record<string, AgentMcpToolItem[]>;
              setMcpToolsConfig(fresh);
              // After the new tool list lands, drop the per-server
              // spinner for any server whose reconcile is observably
              // settled. The "settled" condition is keyed off the
              // server's current `activeServers` membership:
              //   - on + fresh.tools has entries  → reconcile wrote tools, done
              //   - off                             → user explicitly wants
              //                                       it off; no reconcile
              //                                       indicator should linger
              // Servers still loading on+empty (e.g. third-party MCP
              // still connecting) keep the spinner until the next
              // event / poll settles them.
              const liveActive = useMcpStore
                .getState()
                .activeServers[selectedAgentId] ?? EMPTY_ARRAY;
              const pending = useMcpStore
                .getState()
                .perServerLoading[selectedAgentId];
              if (pending) {
                for (const serverName of Object.keys(pending)) {
                  if (!pending[serverName]) continue;
                  const isOn = liveActive.includes(serverName);
                  const hasTools = (fresh[serverName] ?? []).length > 0;
                  if (isOn ? hasTools : true) {
                    clearServerLoading(selectedAgentId, serverName);
                  }
                }
              }
            }
          }
        } catch { /* ignore */ }
      } catch { /* ignore */ }
    };
    window.addEventListener('acowork:refresh-agent-config', handler);
    return () => window.removeEventListener('acowork:refresh-agent-config', handler);
  }, [selectedAgentId]);

  // ── ADR-029: Builtin tools helpers ─────────────────────────────────

  /** Toggle a builtin tool enabled/disabled and PUT to Gateway */
  const toggleBuiltinTool = async (toolName: string) => {
    if (!selectedAgentId) return;
    const next = builtinToolsAll.map((entry) =>
      entry.name === toolName ? { ...entry, enabled: !entry.enabled } : entry
    );
    setBuiltinToolsAll(next);
    setBuiltinSaving(true);
    try {
      const enabledNames = next.filter((e) => e.enabled).map((e) => e.name);
      // ADR-040 follow-up: `builtin_tools` moved off `/config` (where
      // it conflated model knobs with tool state) onto its own route.
      // The Gateway has no special-case logic here — it transparently
      // reverse-proxies the path to the Runtime, so the desktop just
      // hits the matched endpoint.
      await fetch(
        `${getGatewayUrl()}/api/agents/${selectedAgentId}/builtin-tools`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ builtin_tools: enabledNames }),
        },
      );
    } catch {
      // silently ignore network errors; optimistic update is OK
    } finally {
      setBuiltinSaving(false);
    }
  };

  // ── ADR-069: MCP tool helpers ──────────────────────────────────────

  /**
   * PUT the current `mcpToolsConfig` to the Runtime. The whole map is
   * sent as the request body — the backend's `put_mcp_tools` persists
   * it verbatim to `agent_mcp_tools.json` (no server-side merge; the
   * desktop's list is authoritative). Wire shape is identical to the
   * GET response — three-way identity between the on-disk file, the
   * HTTP body, and the desktop render (ADR-069).
   */
  const saveMcpToolsConfig = async (
    next: Record<string, AgentMcpToolItem[]>,
  ) => {
    if (!selectedAgentId) return;
    setMcpToolsSaving(true);
    try {
      await fetch(
        `${getGatewayUrl()}/api/agents/${selectedAgentId}/mcp-tools`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ servers: next }),
        },
      );
    } catch {
      // silently ignore network errors; optimistic update is OK
    } finally {
      setMcpToolsSaving(false);
    }
  };

  /**
   * Toggle a single tool's `enabled` flag for a server. The backend
   * provides the complete per-server list (name + enabled +
   * description) via GET /mcp-tools, so the toggle is a pure local
   * flip: find the row, invert `enabled`, PUT the whole map. No
   * defaults, no hardcoded tool lists — the frontend just reflects
   * back what the backend gave it (ADR-069).
   */
  const toggleMcpTool = (serverName: string, toolName: string) => {
    const rows = mcpToolsConfig[serverName];
    if (!rows) return;
    const nextRows = rows.map((row) =>
      row.name === toolName ? { ...row, enabled: !row.enabled } : row
    );
    const next = { ...mcpToolsConfig, [serverName]: nextRows };
    setMcpToolsConfig(next);
    saveMcpToolsConfig(next);
  };

  if (!selectedAgentId || !selectedAgent) {
    return (
      <div className="flex flex-1 items-center justify-center bg-right-panel p-6">
        <span className="text-xs text-text-tertiary ">{t("agentSetup.noAgentSelected")}</span>
      </div>
    );
  }

  return (
    <OverlayScrollbar dataTabScroll className="flex-1 bg-right-panel p-3">
      {/* ── Builtin Tools card ─────────────────────────────────────
          Level-1 collapsible card in the same language as the
          Debug-panel "Context Snapshots" list: chevron on the left,
          title + count Badge on the right of it, click to expand the
          tool rows (ADR-029). Row contract: name + activation Switch. */}
      <div>
        <ListBox dividers={false}>
          <ExpandableRow
            open={builtinOpen}
            onToggle={() => setBuiltinOpen((v) => !v)}
            title={t("agentSetup.builtinTools", { count: builtinToolsAll.length })}
            ariaLabel={t("agentSetup.builtinTools", { count: builtinToolsAll.length })}
            bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
          >
            {builtinToolsAll.length === 0 ? (
              <EmptyState message={t("agentSetup.noBuiltinTools")} />
            ) : (
              <OverlayScrollbar maxHeight={192} containerClassName="flex-none w-full">
                <ListBox variant="plain">
                  {builtinToolsAll.map((entry) => (
                    <ListRow
                      key={entry.name}
                      surface="inset"
                      trailing={
                        <Switch
                          checked={entry.enabled}
                          onChange={() => toggleBuiltinTool(entry.name)}
                          disabled={builtinSaving || !selectedAgentId}
                          size="sm"
                          aria-label={entry.name}
                        />
                      }
                    >
                      <span className="truncate text-11 font-medium text-text-secondary ">
                        {entry.name}
                      </span>
                    </ListRow>
                  ))}
                </ListBox>
              </OverlayScrollbar>
            )}
          </ExpandableRow>
        </ListBox>
        <p className="mt-1 text-9 text-text-tertiary ">
          {t("agentSetup.builtinToolsDesc")}
        </p>
      </div>

      {/* Divider — full panel-width hairline separating the functional
          blocks (Builtin Tools / MCP). Matches the
          workspace/memory panel divider style. */}
      <div className="-mx-3 my-2 border-t border-border-divider" />

      {/* ADR-104: the package's MCP recommendations. Sits directly above the
          MCP switch list because installing one moves a row *into* that list —
          cause and effect have to be on screen together. Renders nothing when
          the package declares none (see the component). */}
      <McpRecommendationGroup
        agentId={selectedAgentId}
        recommendations={recommendations}
        onReload={reloadTools}
      />

      {recommendations.length > 0 && (
        <div className="-mx-3 my-2 border-t border-border-divider" />
      )}

      {/* MCP Servers card — the group is a level-1 collapsible card
          like the Debug-panel snapshot list; each server renders as a
          collapsible row inside the card body, and each server row
          expands to its per-tool list (ADR-068 UX / ADR-069 data). */}
      <div>
        <ListBox dividers={false}>
          <ExpandableRow
            open={mcpOpen}
            onToggle={() => setMcpOpen((v) => !v)}
            title={t("agentSetup.mcpServers", { count: mcpServerDefs.length })}
            ariaLabel={t("agentSetup.mcpServers", { count: mcpServerDefs.length })}
            bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
          >
            {mcpServerDefs.length === 0 ? (
              <EmptyState message={t("agentSetup.noMcpInCatalog")} />
            ) : (
              <ListBox variant="plain">
                {mcpServerDefs.map((server) => {
                  const isChecked = activeServers.includes(server.name);
                  // ADR-069: the per-tool list comes entirely from
                  // `GET /agents/{id}/mcp-tools` — the backend provides
                  // the complete list (name + enabled + description)
                  // reconciled against the live MCP `tools/list`. The
                  // frontend renders it directly and never maintains a
                  // hardcoded tool list.
                  const tools: AgentMcpToolItem[] = mcpToolsConfig[server.name] ?? [];
                  return (
                    <McpServerCard
                      key={server.name}
                      server={server}
                      displayName={displayNameByName.get(server.name)}
                      isChecked={isChecked}
                      tools={tools}
                      loading={!!perServerLoading[server.name]}
                      onToggleTool={(tool) => toggleMcpTool(server.name, tool)}
                      onToggleServer={() =>
                        selectedAgentId && toggleServer(selectedAgentId, server.name)
                      }
                      switchDisabled={activationLoading || !selectedAgentId}
                      toolSwitchDisabled={mcpToolsSaving || !selectedAgentId}
                      ensure={
                        selectedAgentId
                          ? ensureStatus[mcpEnsureKey(selectedAgentId, server.name)]
                          : undefined
                      }
                    />
                  );
                })}
              </ListBox>
            )}
          </ExpandableRow>
        </ListBox>
        <p className="mt-1 text-9 text-text-tertiary ">
          {t("agentSetup.mcpToggleDesc")}
        </p>
        {/* Surface MCP PUT errors that the store would otherwise swallow.
            The setActiveServers action optimistically updates the UI and
            rolls back on failure; this banner makes the rollback visible
            to the user instead of leaving the checkbox mysteriously
            unchecked. */}
        {mcpError && (
          <div
            role="alert"
            className="mt-1 flex items-start gap-1.5 rounded-md border border-[var(--color-destructive)]/30 bg-[var(--color-destructive)]/10 px-2 py-1 text-10 text-[var(--color-destructive)] dark:border-[var(--color-destructive)]/40 dark:bg-[var(--color-destructive)]/15"
          >
            <span className="flex-1 break-words">
              {t("agentSetup.mcpToggleError", { error: mcpError })}
            </span>
            <button
              type="button"
              onClick={() => useMcpStore.setState({ error: null })}
              className="shrink-0 text-10 underline hover:no-underline"
              aria-label="Dismiss error"
            >
              ×
            </button>
          </div>
        )}
      </div>
    </OverlayScrollbar>
  );
}
