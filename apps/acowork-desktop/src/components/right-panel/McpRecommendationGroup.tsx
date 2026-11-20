//! ADR-104 — the "recommended MCP servers" group of the tools cards
//! (hosted by the Setup panel now that the Tools rail tab is gone).
//!
//! ## Why this group exists at all
//!
//! A package can declare `[[mcp_servers]]`: "I am much better with this
//! server installed". A declaration that only sits in a file is decoration —
//! the user pays for it with a manifest they cannot act on. So the whole point
//! of the declaration is this component, and it must produce four observable
//! results, one per state:
//!
//! 1. **Visible** — the suggestion renders with the author's `why` and the
//!    tool names it would add, next to the switches it affects.
//! 2. **Actionable in one click** — `not_installed` materializes the
//!    declaration into the Gateway catalog (the Gateway owns the recipe and
//!    the `origin` string, so this is one POST, not a client-side build).
//! 3. **Changes the tool set** — `installed_inactive` activates through the
//!    same `PUT /mcp-servers` path the regular toggle uses, so there is
//!    exactly one activation mechanism in the app.
//! 4. **The gap is visible** — when a required secret is missing the server
//!    cannot start; the card says which key, and points at the one place that
//!    can fix it.
//!
//! ## Why the state comes from the server
//!
//! `state` is derived by the Runtime (`build_recommendation_views`) from
//! `agent_mcp.json` × the merged install list. The Desktop does not compute
//! it: two clients recomputing the same join would eventually disagree, and
//! the user would see "install" for something already installed.
//!
//! The one thing the Desktop *does* own is the in-flight state of its own
//! click (`installing` / `activating` / `failed`). Those are not lifecycle
//! states — they are process state and must not be persisted, or a crash
//! would leave a permanent "installing" row (same reasoning as ADR-091 §5.8).
//!
//! ## Why the card is never open by default
//!
//! The group sits in the Setup panel's single scrolling column, between the
//! Builtin Tools card and the MCP switch list. Expanding it on its own steals
//! most of the viewport from the cards the user actually came for — the exact
//! "recommendation group becoming noise" risk ADR-104 §8 names. So the card
//! always starts collapsed; the header carries a red dot while something is
//! still unhandled (`hasUnhandledRecommendation`), which is the reminder
//! without the takeover. The dot disappears once every row is `active` or
//! `dismissed`.

import { useCallback, useMemo, useState } from "react";
import { AlertTriangle, Check, Download, Loader2, Play, RotateCcw } from "lucide-react";
import { Badge, EmptyState, ExpandableRow, ListBox, ListRow } from "../common/list";
import { useTranslation } from "../../i18n/useTranslation";
import { useMcpStore } from "../../stores/mcpStore";
import { useLayoutStore } from "../../stores/layoutStore";
import { materializeDeclaration } from "../../lib/mcpPresets";
import { hasUnhandledRecommendation } from "../../lib/mcpRecommendations";
import { log } from "../../lib/logger";
import type { McpRecommendationView, McpRecommendationState } from "../../lib/types";

/** Per-row transient state owned by this component (never persisted). */
type RowPhase = "idle" | "installing" | "activating" | "failed";

interface Props {
  agentId: string;
  recommendations: McpRecommendationView[];
  /** Re-fetch the merged tools view so `state` comes back authoritative. */
  onReload: () => void;
}

/**
 * Tools-panel group listing the package's MCP recommendations.
 *
 * Renders nothing when the package declares none — an empty "recommended"
 * card would be noise on every ordinary agent.
 *
 * ## Why a gate component with no hooks, plus a real card inside
 *
 * The card's initial expand state is a property of the *data* (see
 * `RecommendationCard`), and the data arrives asynchronously: the host starts
 * from an empty list and fills it when `/tools` answers. A `useState`
 * initializer on a component that mounts alongside that fetch would run on the
 * empty array and decide wrongly — either a collapsed card that stays
 * collapsed, or an expanded card that pops open one frame later.
 *
 * Mounting the card only once a list exists removes the whole race: this gate
 * owns no state, so the card's first render already carries real rows. The host
 * additionally scopes the list to the agent it was fetched for, so a
 * recommendation-heavy agent cannot decide the default for the next agent while
 * that agent's own list is still loading.
 */
export function McpRecommendationGroup({ agentId, recommendations, onReload }: Props) {
  if (recommendations.length === 0) return null;
  return (
    <RecommendationCard
      agentId={agentId}
      recommendations={recommendations}
      onReload={onReload}
    />
  );
}

/**
 * The card itself — header, rows, ignored disclosure and per-row in-flight
 * state.
 *
 * ## Default state (ADR-104 §8, "the recommendation group becoming noise")
 *
 * Always collapsed. While some suggestion still needs a decision
 * (`not_installed` / `installed_inactive`, see `hasUnhandledRecommendation`)
 * the header shows a red dot; a collapsed card plus a dot is a nudge, an
 * auto-expanded card is a takeover of the panel's scroll column.
 *
 * The dot is derived on every render (unlike `open`, which is the user's own
 * state): acting on the last pending row reloads the list, and the dot must go
 * out with it.
 */
function RecommendationCard({ agentId, recommendations, onReload }: Props) {
  const { t } = useTranslation();
  // The card is never auto-opened — see the doc above.
  const [open, setOpen] = useState(false);
  const [showDismissed, setShowDismissed] = useState(false);
  const [phases, setPhases] = useState<Record<string, RowPhase>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const toggleServer = useMcpStore((s) => s.toggleServer);
  const putRecommendation = useMcpStore((s) => s.putRecommendation);

  const visible = useMemo(
    () => recommendations.filter((r) => r.state !== "dismissed"),
    [recommendations],
  );
  const dismissed = useMemo(
    () => recommendations.filter((r) => r.state === "dismissed"),
    [recommendations],
  );

  const setPhase = useCallback((key: string, phase: RowPhase, error?: string) => {
    setPhases((prev) => ({ ...prev, [key]: phase }));
    setErrors((prev) => {
      if (phase === "failed" && error) return { ...prev, [key]: error };
      if (!(key in prev)) return prev;
      const next = { ...prev };
      delete next[key];
      return next;
    });
  }, []);

  /**
   * Materialize one declaration into the Gateway catalog, then reload so the
   * state flips to `installed_inactive` from the Runtime rather than from an
   * optimistic local guess.
   */
  const install = useCallback(
    async (rec: McpRecommendationView) => {
      setPhase(rec.key, "installing");
      try {
        await materializeDeclaration(declarationBodyFor(rec, agentId));
        // The catalog changed globally: let every panel re-read it.
        await useMcpStore.getState().loadCatalog();
        onReload();
        setPhase(rec.key, "idle");
      } catch (e: unknown) {
        const message = e instanceof Error ? e.message : String(e);
        log.warn("install recommended MCP failed", { key: rec.key, message });
        setPhase(rec.key, "failed", message);
      }
    },
    [agentId, onReload, setPhase],
  );

  /** Activate through the same store action the regular server toggle uses. */
  const activate = useCallback(
    async (rec: McpRecommendationView) => {
      setPhase(rec.key, "activating");
      try {
        await toggleServer(agentId, rec.key);
        // Activation is optimistic in the store; re-read the authoritative
        // view so this card and the switch list below cannot drift apart.
        onReload();
        setPhase(rec.key, "idle");
      } catch (e: unknown) {
        const message = e instanceof Error ? e.message : String(e);
        setPhase(rec.key, "failed", message);
      }
    },
    [agentId, onReload, setPhase, toggleServer],
  );

  /** Persist the dismissal flag (the only user-writable field, ADR-104). */
  const setDismissed = useCallback(
    async (rec: McpRecommendationView, dismissed: boolean) => {
      try {
        await putRecommendation(agentId, rec.key, dismissed);
        onReload();
      } catch (e: unknown) {
        const message = e instanceof Error ? e.message : String(e);
        setPhase(rec.key, "failed", message);
      }
    },
    [agentId, onReload, putRecommendation, setPhase],
  );

  // Invariant: the gate above only mounts this card with a non-empty list, so
  // an empty `recommendations` never reaches the body (and never renders the
  // "all handled" empty state by accident).
  //
  // `pending` counts only *installable* gaps, for the description line under
  // the card. The header carries the full declaration count instead, because
  // ignored rows are still declared by the package.
  const pending = visible.filter((r) => r.state === "not_installed").length;

  // Header nudge: something is still one click away from being useful.
  // `aria-hidden` because the count in the title and the description line
  // under the card already say it in words — the dot is only the attention
  // marker, not the only carrier of the information.
  const needsAttention = hasUnhandledRecommendation(recommendations);

  return (
    <div>
      <ListBox dividers={false}>
        <ExpandableRow
          open={open}
          onToggle={() => setOpen((v) => !v)}
          title={t("agentSetup.mcpRecommended", { count: recommendations.length })}
          meta={
            needsAttention ? (
              <span
                aria-hidden
                data-testid="mcp-recommendation-dot"
                className="h-1.5 w-1.5 rounded-full bg-[var(--color-destructive)]"
              />
            ) : undefined
          }
          ariaLabel={t("agentSetup.mcpRecommended", { count: recommendations.length })}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
        >
          {visible.length === 0 ? (
            <EmptyState message={t("agentSetup.mcpRecommendedAllHandled")} />
          ) : (
            <ListBox variant="plain">
              {visible.map((rec) => (
                <RecommendationRow
                  key={rec.key}
                  rec={rec}
                  phase={phases[rec.key] ?? "idle"}
                  error={errors[rec.key]}
                  onInstall={() => void install(rec)}
                  onActivate={() => void activate(rec)}
                  onDismiss={() => void setDismissed(rec, true)}
                />
              ))}
            </ListBox>
          )}

          {/* Dismissed rows stay reachable — "ignore" is not "delete". */}
          {dismissed.length > 0 && (
            <div className="border-t border-border-divider">
              <button
                type="button"
                onClick={() => setShowDismissed((v) => !v)}
                className="w-full px-3 py-1.5 text-left text-10 text-text-tertiary hover:text-text-secondary"
              >
                {showDismissed
                  ? t("agentSetup.mcpRecommendedHideIgnored", { count: dismissed.length })
                  : t("agentSetup.mcpRecommendedShowIgnored", { count: dismissed.length })}
              </button>
              {showDismissed && (
                <ListBox variant="plain">
                  {dismissed.map((rec) => (
                    <RecommendationRow
                      key={rec.key}
                      rec={rec}
                      phase={phases[rec.key] ?? "idle"}
                      error={errors[rec.key]}
                      onRestore={() => void setDismissed(rec, false)}
                    />
                  ))}
                </ListBox>
              )}
            </div>
          )}
        </ExpandableRow>
      </ListBox>
      <p className="mt-1 text-9 text-text-tertiary">
        {pending > 0
          ? t("agentSetup.mcpRecommendedDescPending", { count: pending })
          : t("agentSetup.mcpRecommendedDesc")}
      </p>
    </div>
  );
}

/** Body of `POST /api/mcp-catalog/from-declaration` for one declaration. */
function declarationBodyFor(rec: McpRecommendationView, agentId: string) {
  if (rec.source === "inline") {
    return {
      key: rec.key,
      inline: {
        transport: rec.transport ?? "http",
        url: rec.url,
        required_secrets: rec.required_secrets,
      },
      agent_id: agentId,
    };
  }
  // `hosted` servers are platform-published; they are never materialized by
  // the client (the card hides install for them), so falling through to the
  // preset branch is only reachable for `preset`.
  return { key: rec.key, preset_id: rec.preset ?? rec.key, agent_id: agentId };
}

interface RowProps {
  rec: McpRecommendationView;
  phase: RowPhase;
  error?: string;
  onInstall?: () => void;
  onActivate?: () => void;
  onDismiss?: () => void;
  onRestore?: () => void;
}

const STATE_TONE: Record<McpRecommendationState, "neutral" | "accent" | "success" | "warning"> = {
  not_installed: "warning",
  installed_inactive: "neutral",
  active: "success",
  dismissed: "neutral",
};

const STATE_LABEL: Record<McpRecommendationState, string> = {
  not_installed: "agentSetup.mcpStateNotInstalled",
  installed_inactive: "agentSetup.mcpStateInstalledInactive",
  active: "agentSetup.mcpStateActive",
  dismissed: "agentSetup.mcpStateDismissed",
};

/**
 * One recommendation row.
 *
 * The action slot holds exactly one primary action per state, because that is
 * the only thing the state machine is for. "Ignore" is a secondary text
 * action, always available while the row is live.
 */
function RecommendationRow({
  rec,
  phase,
  error,
  onInstall,
  onActivate,
  onDismiss,
  onRestore,
}: RowProps) {
  const { t } = useTranslation();
  const busy = phase === "installing" || phase === "activating";
  const hosted = rec.source === "hosted";

  const primary = (() => {
    if (busy) {
      return (
        <span className="inline-flex shrink-0 items-center gap-1 text-10 text-text-tertiary">
          <Loader2 className="h-3 w-3 animate-spin" />
          {phase === "installing"
            ? t("agentSetup.mcpRecommendInstalling")
            : t("agentSetup.mcpRecommendActivating")}
        </span>
      );
    }
    switch (rec.state) {
      case "not_installed":
        // A hosted server is published by the platform; there is nothing for
        // the user to install, so no button is offered rather than a dead one.
        return hosted ? null : (
          <button
            type="button"
            onClick={onInstall}
            className="inline-flex shrink-0 items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium"
          >
            <Download className="h-3 w-3" />
            {t("agentSetup.mcpRecommendInstall")}
          </button>
        );
      case "installed_inactive":
        return (
          <button
            type="button"
            onClick={onActivate}
            className="inline-flex shrink-0 items-center gap-1 rounded btn-accent px-2 py-1 text-11 font-medium"
          >
            <Play className="h-3 w-3" />
            {t("agentSetup.mcpRecommendActivate")}
          </button>
        );
      case "active":
        return (
          <span className="inline-flex shrink-0 items-center gap-1 text-10 text-[var(--color-ok)]">
            <Check className="h-3.5 w-3.5" />
            {t("agentSetup.mcpRecommendInUse")}
          </span>
        );
      case "dismissed":
        return (
          <button
            type="button"
            onClick={onRestore}
            className="inline-flex shrink-0 items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium"
          >
            <RotateCcw className="h-3 w-3" />
            {t("agentSetup.mcpRecommendRestore")}
          </button>
        );
    }
  })();

  return (
    <ListRow
      testId={`mcp-recommendation-${rec.key}`}
      surface="inset"
      trailing={
        <span className="flex items-center gap-2">
          {primary}
          {rec.state !== "dismissed" && rec.state !== "active" && onDismiss && (
            <button
              type="button"
              onClick={onDismiss}
              className="shrink-0 text-10 text-text-tertiary underline-offset-2 hover:underline"
            >
              {t("agentSetup.mcpRecommendIgnore")}
            </button>
          )}
        </span>
      }
    >
      <div className="flex flex-wrap items-center gap-x-1.5 gap-y-0.5">
        <span className="truncate text-11 font-medium text-text-secondary">{rec.key}</span>
        <Badge tone="neutral">{rec.source}</Badge>
        <Badge tone={STATE_TONE[rec.state]}>{t(STATE_LABEL[rec.state])}</Badge>
        {rec.required && <Badge tone="accent">{t("agentSetup.mcpRecommendRequired")}</Badge>}
      </div>
      {/* The author's own words — not paraphrased, so the user judges the
          suggestion by the reasoning the package author actually gave. */}
      {rec.why && <p className="mt-0.5 text-10 text-text-tertiary">{rec.why}</p>}
      {/* "N tools" is the concrete consequence of installing: the package
          says exactly what it gains. */}
      {rec.tools.length > 0 && (
        <p className="mt-0.5 text-10 text-text-tertiary">
          {t("agentSetup.mcpRecommendTools", { tools: rec.tools.join(", ") })}
        </p>
      )}
      {/* The gap, stated plainly: a server whose key is unset cannot start,
          and that is invisible everywhere else in the UI. */}
      {rec.required_secrets.length > 0 && (
        <p className="mt-0.5 flex items-center gap-1 text-10 text-[var(--color-warning)]">
          <AlertTriangle className="h-3 w-3 shrink-0" />
          {t("agentSetup.mcpRecommendNeedsSecrets", {
            secrets: rec.required_secrets.join(", "),
          })}
          <button
            type="button"
            onClick={() => {
              useLayoutStore.getState().requestHarnessTab("mcp");
              useLayoutStore.getState().requestNavView("harness");
            }}
            className="underline underline-offset-2 hover:no-underline"
          >
            {t("agentSetup.mcpRecommendConfigure")}
          </button>
        </p>
      )}
      {phase === "failed" && error && (
        <p className="mt-0.5 text-10 text-[var(--color-destructive)]">
          {t("agentSetup.mcpRecommendActionFailed", { error })}
        </p>
      )}
    </ListRow>
  );
}

export default McpRecommendationGroup;