import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { httpApiError } from "../../lib/api-error";
import type {
  CompactModelRef,
  ConsolidationStatusResponse,
  DistillerStatus,
  VaultKeyEntry,
} from "../../lib/types";
import { fetchProviders } from "../../lib/gateway-api";
import { getGatewayUrl } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import { useToast } from "../common/ToastProvider";
import { Switch } from "../common/Switch";
import { ModelPriorityList } from "../common/ModelPriorityList";
import { StyledInput } from "../common/StyledInput";
import { ListBox, ExpandableRow } from "../common/list";
import { log } from "../../lib/logger";
import { with503Retry, WRITE_503_RETRY } from "../../lib/httpRetry";
import {
  distillerForm,
  minutesToSecs,
  parsePositiveInt,
} from "./consolidationPolicy";

/**
 * `distiller` half of a `PUT /memory/consolidation/status` body. An absent key
 * means "keep the current value" service-side, and the service runs
 * `deny_unknown_fields`, so this type is the whole writable surface.
 */
type DistillerPatch = {
  enabled?: boolean;
  interval_secs?: number;
  accumulation_threshold?: number;
  idle_secs?: number;
  models?: CompactModelRef[];
};

/**
 * "记忆沉淀" settings card — top of the memory panel (ADR-071 D3/D5).
 *
 * Reads/writes the distiller policy that the memory service actually acts on
 * (ADR-101 P1e): `GET` / `PUT /api/agents/{id}/memory/consolidation/status`,
 * persisted per instance as `{instance_dir}/consolidation.json`. The knobs used
 * to live in `agent_config.json`; that copy is gone from `AgentConfig`, so
 * writing it was a silent no-op — the card showed the user's toggle until it
 * remounted, and every agent switch read the empty config back as "off".
 *
 * - Enabled switch → `distiller.enabled`
 * - Model list → `distiller.models` (ordered `{provider_id, model_id}[]`), same
 *   provider/model option source as the Harness compact-model card
 *   (vault keys + provider display names); empty = no chain of its own, the
 *   service falls back to the Gateway's global compact chain.
 * - Interval / accumulation / idle inputs → the three trigger fields, minutes
 *   on screen and seconds on the wire.
 *
 * Every field always holds a concrete value service-side, so reloading the
 * card shows what is in force rather than a blank "no opinion". Blur saves the
 * numeric fields; the switch and model pick save immediately on change.
 *
 * Visual grammar: level-1 collapsible card (same shell as the Status-tab
 * Session Status / Tools-tab Builtin Tools / Debug-tab Context Snapshots
 * / Memory tab node list). The header is chevron + title + trailing
 * Switch — nothing else, so the row matches every other level-1 card
 * visually. The Switch owns the enable bit AND auto-expands the body
 * when enabled / auto-collapses when disabled (`handleToggle`), so the
 * user never has to first flip the switch then click the title row.
 * The backlog / last-run hint is intentionally rendered INSIDE the
 * expanded body — it is runtime status, not card chrome.
 */
export function MemoryDistillSettings({
  agentId,
  running,
  distillerStatus,
}: {
  agentId: string | null;
  running: boolean;
  distillerStatus: DistillerStatus | null;
}) {
  const { t } = useTranslation();
  const { addToast } = useToast();

  const [loaded, setLoaded] = useState(false);
  const [enabled, setEnabled] = useState(false);
  // Ordered distiller-model candidates (ADR-056 list semantics); empty = this
  // instance has no chain of its own, so the service falls back to the
  // Gateway's global compact chain.
  const [models, setModels] = useState<CompactModelRef[]>([]);
  const [intervalInput, setIntervalInput] = useState("");
  const [accInput, setAccInput] = useState("");
  const [idleInput, setIdleInput] = useState("");
  const [expanded, setExpanded] = useState(false);
  const [savingField, setSavingField] = useState<string | null>(null);

  // Bumped whenever the selected agent changes. In-flight loads and saves
  // compare against it before writing state, so a slow response for the agent
  // the user just left cannot repaint the card for the agent they are on.
  const generationRef = useRef(0);

  const applyStatus = useCallback((data: ConsolidationStatusResponse) => {
    const form = distillerForm(data.distiller);
    setEnabled(form.enabled);
    setModels(form.models);
    setIntervalInput(form.intervalMinutes);
    setAccInput(form.accumulation);
    setIdleInput(form.idleMinutes);
  }, []);

  // ── Load the distiller policy the service is actually running ─────────
  const loadConfig = useCallback(async () => {
    if (!agentId || !running) return;
    const generation = generationRef.current;
    try {
      const res = await with503Retry(
        () =>
          fetch(
            `${getGatewayUrl()}/api/agents/${agentId}/memory/consolidation/status`,
          ),
        { tag: `MemoryDistillSettings.loadConfig(${agentId})`, logger: log },
      );
      if (!res.ok) return;
      const data: ConsolidationStatusResponse = await res.json();
      if (generation !== generationRef.current) return;
      applyStatus(data);
    } catch {
      // Best-effort: the service is not reachable yet; keep last values.
    } finally {
      setLoaded(true);
    }
  }, [agentId, running, applyStatus]);

  useEffect(() => {
    generationRef.current += 1;
    setLoaded(false);
    void loadConfig();
  }, [loadConfig]);

  // ── Model options: vault keys + provider display names ───────────────
  // Same data source as `GlobalCompactModelCard` (Harness tab): vault
  // entries define which (provider, model) pairs the user can actually
  // select; `/api/models` supplies human-readable provider names.
  const [options, providerNames] = useModelOptions();

  // ── PUT a patch to the service's consolidation policy ────────────────
  const patchDistiller = useCallback(
    async (field: string, patch: DistillerPatch): Promise<boolean> => {
      if (!agentId) return false;
      const generation = generationRef.current;
      setSavingField(field);
      try {
        const res = await with503Retry(
          () =>
            fetch(
              `${getGatewayUrl()}/api/agents/${agentId}/memory/consolidation/status`,
              {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ distiller: patch }),
              },
            ),
          {
            policy: WRITE_503_RETRY,
            tag: `MemoryDistillSettings.patch(${agentId})`,
            logger: log,
          },
        );
        if (!res.ok) {
          const err = await httpApiError(res);
          addToast({
            type: "error",
            message:
              err.status === 403
                ? err.message
                : t("memoryPanel.distillerSaveFailed", {
                    status: res.status,
                  }),
          });
          // Roll the optimistic local update back to the service's truth.
          if (generation === generationRef.current) void loadConfig();
          return false;
        }
        // The PUT answers with the new status: adopt it so the card shows what
        // is actually in force rather than what we hoped was in force.
        const data: ConsolidationStatusResponse = await res.json();
        if (generation !== generationRef.current) return true;
        applyStatus(data);
        return true;
      } catch (e) {
        log.warn("[MemoryDistillSettings] save failed:", field, e);
        addToast({
          type: "error",
          message: t("memoryPanel.distillerSaveFailed", { status: "network" }),
        });
        if (generation === generationRef.current) void loadConfig();
        return false;
      } finally {
        setSavingField(null);
      }
    },
    [agentId, addToast, t, loadConfig, applyStatus],
  );

  const handleToggle = (v: boolean) => {
    setEnabled(v);
    // Switch owns the enable bit, so it should also own the affordance
    // of showing / hiding the configuration body: enabling auto-expands
    // so the user lands on the controls they just turned on; disabling
    // auto-collapses because the body becomes inert (all inputs are
    // disabled when `!enabled` anyway).
    setExpanded(v);
    void patchDistiller("distiller_enabled", { enabled: v });
  };

  const handleModelsChange = (next: CompactModelRef[]) => {
    // Optimistic local update; a failed save reloads the persisted chain.
    setModels(next);
    void patchDistiller("distiller_models", { models: next });
  };

  /**
   * Persist one numeric field. An unparseable / out-of-range input is not a
   * value the service would accept, and there is no "unset" state to clear
   * any more — re-read the service's truth instead of writing junk or leaving
   * the input claiming a value that was never saved.
   */
  const saveNumber = (
    field: string,
    patchKey: "interval_secs" | "accumulation_threshold" | "idle_secs",
    raw: string,
    setter: (v: string) => void,
    coerce: (raw: string) => number | null,
  ) => {
    setter(raw);
    const value = coerce(raw);
    if (value == null) {
      void loadConfig();
      return;
    }
    void patchDistiller(field, { [patchKey]: value });
  };

  // ── Render ────────────────────────────────────────────────────────────
  // Tighter padding than StyledInput's default for the 3-column grid, but
  // deliberately NO font-size override: the field text stays at StyledInput's
  // `text-xs`, which is also what the model list above is timed to.
  const numInputCls =
    "rounded-md border border-input-border bg-input-bg px-2 py-1 outline-none focus:border-[var(--color-accent)] ";

  // Backlog / last-run hint lives INSIDE the expanded body — it is
  // status info about the runtime, not metadata about the card itself,
  // and only meaningful once the user has opted in to the distiller.
  // Showing it in the title row made the header feel busier than its
  // peers (Session Status, Builtin Tools, Context Snapshots, Memory
  // Nodes), which keep the title row to chevron + title + trailing.
  const runtimeHint =
    loaded && distillerStatus ? (
      <p className="flex items-center gap-1.5 text-10 text-text-tertiary ">
        <span>
          {t("memoryPanel.distillerBacklog", {
            count: distillerStatus.episode_backlog,
          })}
        </span>
        <span aria-hidden>·</span>
        {distillerStatus.last_run ? (
          <>
            <span>
              {t("memoryPanel.distillerLastRun", {
                scanned: distillerStatus.last_run.episodes_scanned,
                promoted: distillerStatus.last_run.total_promoted,
              })}
            </span>
            {/* The three zero cases look identical without this: nothing to
                do, everything deferred for want of a model, and the run
                failing. Only the last one used to be distinguishable. */}
            {distillerStatus.last_run.error ? (
              <>
                <span aria-hidden>·</span>
                <span className="text-[var(--color-danger,#e5484d)]">
                  {t("memoryPanel.distillerRunFailed", {
                    error: distillerStatus.last_run.error,
                  })}
                </span>
              </>
            ) : (
              distillerStatus.last_run.funnel &&
              distillerStatus.last_run.funnel.episodes_deferred > 0 && (
                <>
                  <span aria-hidden>·</span>
                  <span>
                    {t("memoryPanel.distillerDeferred", {
                      deferred: distillerStatus.last_run.funnel.episodes_deferred,
                    })}
                  </span>
                </>
              )
            )}
          </>
        ) : (
          <span>{t("memoryPanel.distillerNeverRun")}</span>
        )}
      </p>
    ) : null;

  return (
    // Level-1 collapsible card in the same grammar as Session Status /
    // Builtin Tools / Context Snapshots / Memory Nodes: `ListBox` shell
    // + `ExpandableRow`. The title row stays clean (chevron + title +
    // trailing Switch) so it visually matches every other level-1
    // card on the right panel. The trailing Switch owns enable AND the
    // auto expand/collapse (see `handleToggle`), so the user never has
    // to first enable then click the title row.
    //
    // `border-b` on the wrapper is the shared vertical hairline that
    // separates this card from the 记忆遗忘 card below — same zinc-200 /
    // zinc-800 tone as the stats-grid hairline at the top of the panel
    // and the hairline between 记忆遗忘 and 记忆搜索. The wrapper owns
    // its own border (not the inner ListBox) so the line spans the
    // full panel width and the spacing above / below the hairline is
    // always the wrapper's `p-3` on each side — uniform across the
    // three cards.
    <div className="border-b border-border-divider p-3">
      <ListBox dividers={false}>
        <ExpandableRow
          open={expanded}
          onToggle={() => setExpanded((v) => !v)}
          title={t("memoryPanel.distillerTitle")}
          ariaLabel={t("memoryPanel.distillerTitle")}
          trailing={
            // The Switch component does not stop propagation itself;
            // ExpandableRow's contract requires trailing interactive
            // elements to swallow the click so toggling does not also
            // collapse/expand the card.
            <span onClick={(e) => e.stopPropagation()}>
              <Switch
                checked={enabled}
                onChange={handleToggle}
                disabled={!running || savingField === "distiller_enabled"}
                size="sm"
                aria-label={t("memoryPanel.distillerEnabled")}
              />
            </span>
          }
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-2"
        >
          <div className="flex flex-col gap-2">
            {runtimeHint}
            <label className="flex flex-col gap-1">
              <span className="text-10 text-text-tertiary ">
                {t("memoryPanel.distillerModel")}
              </span>
              <ModelPriorityList
                items={models}
                options={options}
                providerNameById={providerNames}
                disabled={!running || !enabled || savingField === "distiller_models"}
                onChange={handleModelsChange}
                className="text-xs"
              />
            </label>

            <div className="grid grid-cols-3 gap-2">
              <label className="flex flex-col gap-1">
                <span className="text-10 text-text-tertiary ">
                  {t("memoryPanel.distillerInterval")}
                </span>
                <StyledInput
                  type="number"
                  min={1}
                  value={intervalInput}
                  placeholder={String(60)}
                  onChange={(e) => setIntervalInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber(
                      "distiller_interval_minutes",
                      "interval_secs",
                      e.target.value,
                      setIntervalInput,
                      minutesToSecs,
                    )
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-10 text-text-tertiary ">
                  {t("memoryPanel.distillerAccumulation")}
                </span>
                <StyledInput
                  type="number"
                  min={1}
                  value={accInput}
                  placeholder={String(50)}
                  onChange={(e) => setAccInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber(
                      "distiller_accumulation_threshold",
                      "accumulation_threshold",
                      e.target.value,
                      setAccInput,
                      parsePositiveInt,
                    )
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-10 text-text-tertiary ">
                  {t("memoryPanel.distillerIdle")}
                </span>
                <StyledInput
                  type="number"
                  min={1}
                  value={idleInput}
                  placeholder={String(30)}
                  onChange={(e) => setIdleInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber(
                      "distiller_idle_minutes",
                      "idle_secs",
                      e.target.value,
                      setIdleInput,
                      minutesToSecs,
                    )
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
            </div>

            {!enabled && (
              <p className="text-10 text-text-tertiary ">
                {t("memoryPanel.distillerDisabledHint")}
              </p>
            )}
          </div>
        </ExpandableRow>
      </ListBox>
    </div>
  );
}

/** Vault (provider, model) options + provider display-name map. */
function useModelOptions(): [
  { key: string; providerId: string; modelId: string }[],
  Map<string, string>,
] {
  const [options, setOptions] = useState<
    { key: string; providerId: string; modelId: string }[]
  >([]);
  const [names, setNames] = useState<Map<string, string>>(new Map());

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        // Provider display names (Gateway models cache).
        const providers = await fetchProviders();
        if (cancelled) return;
        const nameMap = new Map<string, string>();
        for (const p of providers) nameMap.set(p.id, p.name);
        setNames(nameMap);
      } catch {
        // Gateway unreachable — fall back to raw provider ids in labels.
      }
      try {
        // Configured (provider, model) pairs from the vault — same source
        // as the Harness compact-model card.
        const keys = await invoke<VaultKeyEntry[]>("list_keys");
        if (cancelled) return;
        const opts: { key: string; providerId: string; modelId: string }[] = [];
        for (const k of keys) {
          const modelIds =
            k.models && k.models.length > 0
              ? k.models
              : k.default_model
                ? [k.default_model]
                : [];
          for (const modelId of modelIds) {
            opts.push({
              key: `${k.provider}::${modelId}`,
              providerId: k.provider,
              modelId,
            });
          }
        }
        setOptions(opts);
      } catch {
        // Tauri bridge unavailable (web preview) — dropdown stays empty, so the
        // saved chain is still listed above but cannot be extended.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  return [options, names];
}
