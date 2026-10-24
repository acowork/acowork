import { useEffect, useMemo, useRef, useState } from "react";
import { useMemoryStore } from "../../stores/memoryStore";
import { useAgentStore } from "../../stores/agentStore";
import { useLayoutStore } from "../../stores/layoutStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { MemoryNodeList } from "./MemoryNodeList";
import { MemoryNodeDetail } from "./MemoryNodeDetail";
import { MemoryDistillSettings } from "./MemoryDistillSettings";
import { MemoryForgettingSettings } from "./MemoryForgettingSettings";
import { AlertTriangle, Info, Search } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledInput } from "../common/StyledInput";
import { ErrorBox } from "../common/ErrorBox";
import { Dropdown } from "../common/Dropdown";
import { ListBox, ExpandableRow } from "../common/list";
import { subTypeOptions } from "./nodeTypeI18n";
import { indexHealthBanner } from "./indexHealth";
import { cn } from "../../lib/utils";

export function MemoryPanel() {
  const { t } = useTranslation();
  const { selectedAgentId } = useAgentStore();
  // We intentionally do NOT gate data fetching on `meta.lifecycle`
  // (ADR-085; formerly `meta.ready`). The lifecycle push arrives
  // asynchronously to Runtime HTTP readiness — gating on it caused the
  // MemoryPanel to flash "Loading…" forever when the user opened it
  // during the first second after agent start. Memory endpoints are
  // Phase-A (HTTP_READY) capabilities anyway. The store fetchers
  // (`memoryStore.fetchNodes` / `fetchStats`) now own the 503 retry
  // loop via `with503Retry`, so a transient 503 recovers
  // transparently.
  // Dead agents (`meta.alive === false`) are still skipped —
  // their Runtime process is not even alive, so retrying buys nothing.
  const isAgentRunning = useAgentStore((s) =>
    selectedAgentId ? !!s.agents[selectedAgentId]?.meta.alive : false
  );
  const {
    nodes,
    total,
    stats,
    selectedNodeId,
    filters,
    page,
    pageSize,
    loading,
    error,
    consolidateMessage,
    migrationInProgress,
    distillerStatus,
    fetchNodes,
    fetchStats,
    distill,
    fetchDistillerStatus,
    fetchForgettingStatus,
    rebuildIndex,
    resumeRebuild,
    setFilters,
    setPage,
    setSelectedNodeId,
    clearMemory,
  } = useMemoryStore();

  // Collapse state for the "记忆搜索" (Memory Search) card body. The
  // card itself is the master-detail region: when collapsed, both the
  // filter row and the list / detail body vanish together (the user is
  // saying "I'm done browsing for now"). We re-open it automatically
  // when a node is selected, otherwise selecting a row from a collapsed
  // card would silently drop the user into a blank detail view.
  const [searchOpen, setSearchOpen] = useState(true);
  useEffect(() => {
    if (selectedNodeId !== null) setSearchOpen(true);
  }, [selectedNodeId]);

  // Sub-filter dropdown is meaningful for Knowledge / Autobiographical /
  // Episodic nodes — those are the labels that carry a sub_type.
  //   - Knowledge / Autobiographical: sub_type is the schema property
  //     (`sub_type` / `category` respectively).
  //   - Episodic: sub_type is the `knowledge_subtype` distillation routing
  //     tag (ADR-068 §3.4.2) — same 4-value enum as Knowledge, surfaced
  //     here so users can drill into "episodes tagged as Preference" etc.
  // Procedural carries no secondary classification and stays hidden.
  // Computing the option list here (rather than in `subTypeOptions`) keeps
  // the i18n t() binding reactive when the user switches locales.
  const supportsSubFilter = (type: typeof filters.type): boolean =>
    type === "Knowledge" ||
    type === "Autobiographical" ||
    type === "Episodic";
  const subTypeChoices = useMemo(
    () => subTypeOptions(t, filters.type),
    [t, filters.type],
  );
  const subFilterVisible = supportsSubFilter(filters.type);

  // Live migration progress for the currently selected agent, used to drive
  // the "重建中…" button label and a tiny progress fraction in the banner.
  const agentMigration = useGatewayStore((s) =>
    selectedAgentId ? s.migrationProgress[selectedAgentId] : undefined,
  );

  const selectedNode = nodes.find((n) => n.node_id === selectedNodeId) ?? null;

  // Load data when agent changes (or transitions from stopped → running).
  useEffect(() => {
    if (!selectedAgentId || !isAgentRunning) return;
    clearMemory();
    void fetchNodes(selectedAgentId);
    void fetchStats(selectedAgentId);
    void fetchDistillerStatus(selectedAgentId);
    void fetchForgettingStatus(selectedAgentId);
    // `clearMemory` above drops the rebuild flag along with everything else,
    // but the rebuild itself is running in the Gateway and the memory service
    // (ADR-100) and does not care that we switched agents. Re-attach, or this
    // button comes back offering to queue a second migration over the same
    // store.
    void resumeRebuild(selectedAgentId);
  }, [
    selectedAgentId,
    isAgentRunning,
    clearMemory,
    fetchNodes,
    fetchStats,
    fetchDistillerStatus,
    fetchForgettingStatus,
    resumeRebuild,
  ]);

  // Re-fetch when filters or pagination change
  useEffect(() => {
    if (!selectedAgentId || !isAgentRunning) return;
    void fetchNodes(selectedAgentId);
  }, [filters, page, pageSize, selectedAgentId, isAgentRunning, fetchNodes]);

  // Re-fetch when the memory tab becomes visible (e.g. agent was started
  // while another tab was active, so data was never loaded for the running agent)
  const activePanelTab = useLayoutStore((s) => s.activePanelTab);
  useEffect(() => {
    if (!selectedAgentId || !isAgentRunning) return;
    if (activePanelTab !== "memory") return;
    void fetchNodes(selectedAgentId);
    void fetchStats(selectedAgentId);
    void fetchDistillerStatus(selectedAgentId);
    void fetchForgettingStatus(selectedAgentId);
    void resumeRebuild(selectedAgentId);
  }, [
    activePanelTab,
    selectedAgentId,
    isAgentRunning,
    fetchNodes,
    fetchStats,
    fetchDistillerStatus,
    fetchForgettingStatus,
    resumeRebuild,
  ]);

  // Auto-dismiss consolidate message after 6 seconds
  const dismissTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (!consolidateMessage) return;
    if (dismissTimer.current) clearTimeout(dismissTimer.current);
    dismissTimer.current = setTimeout(() => {
      useMemoryStore.setState({ consolidateMessage: null });
    }, 6000);
    return () => {
      if (dismissTimer.current) clearTimeout(dismissTimer.current);
    };
  }, [consolidateMessage]);

  const handleDistill = async () => {
    if (!selectedAgentId) return;
    // ADR-071 D2: "立即蒸馏" replaces the retired "合并节点" action.
    // The distiller is opt-in — a disabled distiller returns 409 and the
    // store surfaces the runtime error string in the feedback banner.
    const data = await distill(selectedAgentId);
    if (!data) return; // store already set the error/banner message
    const promoted =
      data.facts_promoted +
      data.preferences_promoted +
      data.relations_promoted +
      data.procedures_promoted +
      data.autobio_promoted;
    useMemoryStore.setState({
      consolidateMessage:
        data.episodes_scanned > 0
          ? t("memoryPanel.distillDone", {
              scanned: data.episodes_scanned,
              promoted,
            })
          : t("memoryPanel.distillEmpty"),
    });
  };

  const handleRefresh = () => {
    if (!selectedAgentId) return;
    void fetchNodes(selectedAgentId);
    void fetchStats(selectedAgentId);
  };

  // Which index-health problem, if any, to show the banner for. The rule lives
  // in ./indexHealth so it can be tested without rendering the panel.
  const healthBanner = indexHealthBanner(stats);

  const handleRebuildIndex = () => {
    if (!selectedAgentId) return;
    // The banner is the only entry point to this button, and the rule that
    // fired the banner also decides the repair scope — a model swap re-embeds
    // everything, a foreign-width/missing-vector defect re-embeds only the
    // rows retrieval cannot see. The scope travels with the banner decision
    // (see ./indexHealth) so the two can never drift apart.
    void rebuildIndex(selectedAgentId, healthBanner?.scope ?? "all");
  };

  // While migration is in flight, show the rebuilt/total fraction so the
  // user can see progress. Falls back to plain "重建中…" if the Gateway has
  // not yet produced a progress payload.
  const migrationProgressLabel = (() => {
    if (!migrationInProgress) return t("memoryPanel.rebuildIndex");
    const p = agentMigration?.progress;
    if (p && p.total_scanned > 0) {
      return `${t("memoryPanel.rebuildIndexInProgress")} ${p.rebuilt}/${p.total_scanned}`;
    }
    return t("memoryPanel.rebuildIndexInProgress");
  })();

  const totalPages = Math.max(1, Math.ceil(total / pageSize));

  // ── Empty state: no agent selected ──
  if (!selectedAgentId) {
    return (
      <div className="flex flex-1 items-center justify-center bg-right-panel p-6 text-xs text-text-tertiary ">
        {t("memoryPanel.selectAgent")}
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden bg-right-panel">
      {/* 1. Stats cards — moved to the top of the panel so the four
          status indicators are the first thing the user sees on entry.
          They are part of the overview strip, paired with the
          health-degradation banner immediately below. */}
      {stats && (
        <div className="grid grid-cols-2 gap-2 border-b border-border-divider px-panel-gutter py-2 sm:grid-cols-4">
          <StatCard label={t("memoryPanel.totalNodes")} value={stats.total_nodes} />
          {/* Optional chain on by_status defends against any future wire-format
              drift on the stats endpoint — the panel must render zeros rather
              than crash the entire panel tree if a contract field is missing
              (see MemoryStatsResponse in acowork-gateway). */}
          <StatCard label={t("memoryPanel.active")} value={stats.by_status?.["Active"] ?? 0} />
          <StatCard label={t("memoryPanel.dormant")} value={stats.by_status?.["Dormant"] ?? 0} />
          <StatCard
            label={t("memoryPanel.version")}
            value={stats.schema_version}
          />
        </div>
      )}

      {/* Index-health banner: dimension mismatch, vectors left at a foreign
          width, or memory nodes without embeddings. The button triggers the
          same /api/embedding-models/{id}/start-migration flow the Harness tab
          uses. */}
      {healthBanner && stats && (
        <div
          className="flex items-center gap-2 border-b border-amber-200 bg-amber-50 px-panel-gutter py-2 text-amber-900 dark:border-amber-900/60 dark:bg-amber-950/40 dark:text-amber-100"
          role="alert"
          data-testid="index-health-banner"
        >
          <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
          <div className="min-w-0 flex-1">
            <p className="truncate text-11 font-semibold">
              {t(healthBanner.titleKey)}
            </p>
            <p className="truncate text-10 opacity-80">
              {t(healthBanner.detailKey, healthBanner.vars)}
              {healthBanner.extraKey &&
                ` · ${t(healthBanner.extraKey, healthBanner.vars)}`}
            </p>
          </div>
          <button
            onClick={handleRebuildIndex}
            disabled={migrationInProgress}
            data-testid="rebuild-index-button"
            className="shrink-0 rounded btn-solid px-2.5 py-1 text-11 font-medium disabled:opacity-50"
          >
            {migrationProgressLabel}
          </button>
        </div>
      )}

      {/* ADR-071 D3/D5: memory distiller settings card (enabled switch,
          model pick, periodic trigger tuning). Reads/writes
          agent_config.json via GET/PUT /agents/{id}/config.
          Sits between the overview strip (stats + health banner) and the
          main search workflow — the distiller is a control surface, not
          part of the search row. */}
      <MemoryDistillSettings
        agentId={selectedAgentId}
        running={isAgentRunning}
        distillerStatus={distillerStatus}
      />

      {/* ADR-057 §5.3 redesign: episodic forgetting settings card
          (enabled switch + half-life / dormant / archive tuning).
          Reads/writes the four `agent_config.json` forgetting fields via
          GET/PUT /agents/{id}/config. Sits right below the distill card —
          both are memory lifecycle control surfaces. */}
      <MemoryForgettingSettings
        agentId={selectedAgentId}
        running={isAgentRunning}
      />

      {/* Error banner */}
      {error && (
        <div className="border-b border-red-200 dark:border-red-900">
          <ErrorBox message={error} className="!rounded-none !border-0" />
        </div>
      )}

      {/* Consolidate feedback banner */}
      {consolidateMessage && (
        <div className="flex items-center gap-1.5 border-b border-[var(--color-accent)]/30 bg-[var(--color-accent)]/10 px-panel-gutter py-1.5">
          <Info className="h-3 w-3 shrink-0 text-[var(--color-accent)]" />
          <span className="text-11 text-[var(--color-accent)]">{consolidateMessage}</span>
        </div>
      )}

      {/* Memory Search card — wraps the search row, the type / time /
          sub_type filters, and the master-detail body in a single
          level-1 collapsible card. The card collapses as one (same
          grammar as Snapshot / Distill cards). A useEffect at the top
          of this component re-opens the card automatically when a node
          is selected, so collapsing the search row never strands the
          user with a hidden detail view.

          Height contract: when OPEN the body claims the remaining
          flex-1 height (so the master-detail list / detail fills the
          panel). When COLLAPSED the ListBox drops `flex-1` and shrinks
          back to the header row's natural height — otherwise the
          title row keeps claiming `flex-1` and the card stays the
          same size with an empty body, which is the bug we just fixed.
      */}
      <div
        className={cn(
          "flex min-h-0 flex-col p-3",
          searchOpen ? "flex-1" : "shrink-0",
        )}
      >
        <ListBox
          dividers={false}
          className={cn(
            "flex min-h-0 flex-col overflow-hidden",
            searchOpen && "flex-1",
          )}
        >
          <ExpandableRow
            open={searchOpen}
            onToggle={() => setSearchOpen((v) => !v)}
            title={t("memoryPanel.searchSectionTitle")}
            ariaLabel={t("memoryPanel.searchSectionTitle")}
            bodyClassName="flex min-h-0 flex-1 flex-col overflow-hidden rounded-b-md border-t border-border-divider bg-panel-inset"
          >
            {/* Search row + filters. shrink-0 so the flex-1 master-detail
                below always claims the remaining height regardless of how
                many filter controls are visible (sub_type only shows for
                Knowledge / Autobiographical). border-b separates the
                control strip from the list region below so the two
                surfaces read as distinct blocks on the inset body. */}
            <div className="flex shrink-0 flex-col gap-2 border-b border-border-divider px-3 py-2">
              {/* Search input — Search icon pinned to the left edge,
                  input gets `pl-7` so the placeholder text never sits
                  under the icon. Mirrors the Session-tab search row at
                  SessionTabBar.tsx:127 so all search inputs share the
                  same affordance. */}
              <div className="relative">
                <Search className="pointer-events-none absolute left-2 top-1/2 h-3 w-3 -translate-y-1/2 text-text-tertiary " />
                <StyledInput
                  type="text"
                  value={filters.keyword}
                  onChange={(e) => setFilters({ keyword: e.target.value })}
                  placeholder={t("memoryPanel.searchNodes")}
                  className="rounded-md py-1.5 pl-7 pr-2.5"
                />
              </div>
              <div className="flex gap-2">
                <Dropdown
                  className="min-w-0 flex-1"
                  value={filters.type}
                  onChange={(v) => {
                    const nextType = v as
                      | "All"
                      | "Knowledge"
                      | "Episodic"
                      | "Procedural"
                      | "Autobiographical";
                    // When the user moves off a label that supports sub_type, the
                    // previous sub-filter becomes meaningless. Clearing it here
                    // keeps the URL state honest and avoids sending a stale
                    // `sub_type=` param on subsequent fetches. Episodic joins
                    // Knowledge / Autobiographical in carrying a sub_type
                    // (`knowledge_subtype`, ADR-068 §3.4.2) so its filter is
                    // preserved across type switches.
                    setFilters({
                      type: nextType,
                      subType: supportsSubFilter(nextType) ? filters.subType : "",
                    });
                  }}
                  options={[
                    { value: "All", label: t("memoryPanel.allTypes") },
                    { value: "Knowledge", label: t("memoryPanel.typeKnowledge") },
                    { value: "Episodic", label: t("memoryPanel.typeEpisodic") },
                    { value: "Procedural", label: t("memoryPanel.typeProcedural") },
                    { value: "Autobiographical", label: t("memoryPanel.typeAutobiographical") },
                  ]}
                />
                <Dropdown
                  className="min-w-0 flex-1"
                  value={filters.timeRange}
                  onChange={(v) =>
                    setFilters({
                      timeRange: v as "1h" | "1d" | "7d" | "30d" | "all",
                    })
                  }
                  options={[
                    { value: "all", label: t("memoryPanel.allTime") },
                    { value: "1h", label: t("memoryPanel.lastHour") },
                    { value: "1d", label: t("memoryPanel.lastDay") },
                    { value: "7d", label: t("memoryPanel.last7Days") },
                    { value: "30d", label: t("memoryPanel.last30Days") },
                  ]}
                />
              </div>
              {subFilterVisible && subTypeChoices.length > 0 && (
                <Dropdown
                  className="w-full"
                  value={filters.subType}
                  onChange={(v) => setFilters({ subType: v })}
                  aria-label={t("memoryPanel.subTypeAriaLabel")}
                  data-testid="memory-sub-type-filter"
                  options={[
                    { value: "", label: t("memoryPanel.allSubTypes") },
                    ...subTypeChoices.map((opt) => ({ value: opt.value, label: opt.label })),
                  ]}
                />
              )}
            </div>

            {/* Master-detail body: list when no node is selected, detail
                otherwise. Both children live inside the same expand
                region so the collapse animation stays consistent. */}
            <div className="flex min-h-0 flex-1 overflow-hidden">
              {!selectedNode ? (
                <MemoryNodeList
                  nodes={nodes}
                  total={total}
                  page={page}
                  pageSize={pageSize}
                  totalPages={totalPages}
                  loading={loading}
                  selectedNodeId={selectedNodeId}
                  onSelectNode={setSelectedNodeId}
                  onPageChange={setPage}
                />
              ) : (
                <MemoryNodeDetail
                  node={selectedNode}
                  onClose={() => setSelectedNodeId(null)}
                  onDelete={(nodeId) => {
                    if (!selectedAgentId) return;
                    void useMemoryStore.getState().deleteNode(selectedAgentId, nodeId);
                  }}
                />
              )}
            </div>
          </ExpandableRow>
        </ListBox>
      </div>

      {/* Bottom actions */}
      <div className="flex gap-3 border-t border-border-divider px-panel-gutter py-2">
        <button
          onClick={handleDistill}
          disabled={loading}
          data-testid="distill-now-button"
          className="flex-1 rounded btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
        >
          {t("memoryPanel.distillNow")}
        </button>
        <button
          onClick={handleRefresh}
          disabled={loading}
          className="flex-1 rounded btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
        >
          {t("memoryPanel.refresh")}
        </button>
      </div>
    </div>
  );
}

function StatCard({
  label,
  value,
}: {
  label: string;
  value: string | number;
}) {
  return (
    <div className="min-w-0 overflow-hidden rounded border border-border-outer bg-panel-block p-2">
      <p className="truncate text-10 text-text-tertiary " title={label}>{label}</p>
      <p
        className="mt-0.5 truncate text-xs font-semibold text-text-secondary "
        title={String(value)}
      >
        {value}
      </p>
    </div>
  );
}
