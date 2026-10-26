import { useCallback, useEffect, useRef, useState } from "react";
import { httpApiError } from "../../lib/api-error";
import type { ConsolidationStatusResponse } from "../../lib/types";
import { getGatewayUrl } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import { useToast } from "../common/ToastProvider";
import { Switch } from "../common/Switch";
import { StyledInput } from "../common/StyledInput";
import { ListBox, ExpandableRow } from "../common/list";
import { log } from "../../lib/logger";
import { with503Retry, WRITE_503_RETRY } from "../../lib/httpRetry";
import {
  forgettingForm,
  parseDormantThreshold,
  parsePositiveInt,
} from "./consolidationPolicy";

/**
 * `forgetting` half of a `PUT /memory/consolidation/status` body. An absent key
 * means "keep the current value" service-side, and the service runs
 * `deny_unknown_fields`, so this type is the whole writable surface.
 */
type ForgettingPatch = {
  enabled?: boolean;
  half_life_days?: number;
  dormant_threshold?: number;
  archive_days?: number;
};

/**
 * "记忆遗忘" settings card — right below the 记忆沉淀 card (ADR-057 §5.3
 * redesign).
 *
 * Reads/writes the forgetting policy that the memory service actually acts on
 * (ADR-101 P1e): `GET` / `PUT /api/agents/{id}/memory/consolidation/status`,
 * persisted per instance as `{instance_dir}/consolidation.json`. The knobs used
 * to live in `agent_config.json`; ADR-101 removed them from `AgentConfig`, so
 * writing that path was a silent no-op — the card showed the user's toggle
 * until it remounted, and every agent switch read the empty config back as
 * "off".
 *
 * - Enabled switch → `forgetting.enabled` (default off — forgetting is opt-in).
 * - Half-life (days) → `forgetting.half_life_days` (default 180). This is THE
 *   core parameter: before the half-life an episodic node keeps (almost) full
 *   retrieval weight; after it the node is progressively down-weighted
 *   (`retention = exp(-ln2 · age/half_life)`).
 * - Dormant threshold → `forgetting.dormant_threshold` (`(0, 1]`, default 0.1).
 *   When retention falls below this the node transitions Active → Dormant and
 *   is excluded from retrieval.
 * - Archive days → `forgetting.archive_days` (default 90). A Dormant node older
 *   than this is archived to the PurgeLog (30-day recovery window).
 *
 * The sediment layer (Knowledge / Procedural / Autobiographical) is
 * intentionally never decayed — this card only governs episodic nodes.
 *
 * Every field always holds a concrete value service-side, so reloading the card
 * shows what is in force rather than a blank "no opinion". Blur saves the
 * numeric fields; the switch saves immediately on change.
 */
export function MemoryForgettingSettings({
  agentId,
  running,
}: {
  agentId: string | null;
  running: boolean;
}) {
  const { t } = useTranslation();
  const { addToast } = useToast();

  const [enabled, setEnabled] = useState(false);
  const [halfLifeInput, setHalfLifeInput] = useState("");
  const [dormantInput, setDormantInput] = useState("");
  const [archiveInput, setArchiveInput] = useState("");
  const [expanded, setExpanded] = useState(false);
  const [savingField, setSavingField] = useState<string | null>(null);

  // Bumped whenever the selected agent changes. In-flight loads and saves
  // compare against it before writing state, so a slow response for the agent
  // the user just left cannot repaint the card for the agent they are on.
  const generationRef = useRef(0);

  const applyStatus = useCallback((data: ConsolidationStatusResponse) => {
    const form = forgettingForm(data.forgetting);
    setEnabled(form.enabled);
    setHalfLifeInput(form.halfLifeDays);
    setDormantInput(form.dormantThreshold);
    setArchiveInput(form.archiveDays);
  }, []);

  // ── Load the forgetting policy the service is actually running ────────
  const loadConfig = useCallback(async () => {
    if (!agentId || !running) return;
    const generation = generationRef.current;
    try {
      const res = await with503Retry(
        () =>
          fetch(
            `${getGatewayUrl()}/api/agents/${agentId}/memory/consolidation/status`,
          ),
        { tag: `MemoryForgettingSettings.loadConfig(${agentId})`, logger: log },
      );
      if (!res.ok) return;
      const data: ConsolidationStatusResponse = await res.json();
      if (generation !== generationRef.current) return;
      applyStatus(data);
    } catch {
      // Best-effort: the service is not reachable yet; keep last values.
    }
  }, [agentId, running, applyStatus]);

  useEffect(() => {
    generationRef.current += 1;
    void loadConfig();
  }, [loadConfig]);

  // ── PUT a patch to the service's consolidation policy ─────────────────
  const patchForgetting = useCallback(
    async (field: string, patch: ForgettingPatch): Promise<boolean> => {
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
                body: JSON.stringify({ forgetting: patch }),
              },
            ),
          {
            policy: WRITE_503_RETRY,
            tag: `MemoryForgettingSettings.patch(${agentId})`,
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
                : t("memoryPanel.forgettingSaveFailed", {
                    status: res.status,
                  }),
          });
          // Roll the optimistic local value back to the service's truth — a
          // refused write must not leave the switch flipped.
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
        log.warn("[MemoryForgettingSettings] save failed:", field, e);
        addToast({
          type: "error",
          message: t("memoryPanel.forgettingSaveFailed", { status: "network" }),
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
    // The switch owns the enable bit AND the expand/collapse affordance
    // (same grammar as the distill card).
    setExpanded(v);
    void patchForgetting("forgetting_enabled", { enabled: v });
  };

  /**
   * Persist one numeric field. An unparseable / out-of-range input is not a
   * value the service would accept, and there is no "unset" state to clear any
   * more — re-read the service's truth instead of writing junk or leaving the
   * input claiming a value that was never saved.
   */
  const saveNumber = (
    field: string,
    patchKey: "half_life_days" | "dormant_threshold" | "archive_days",
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
    void patchForgetting(field, { [patchKey]: value });
  };

  // ── Render ────────────────────────────────────────────────────────────
  const numInputCls =
    "rounded-md border border-input-border bg-input-bg px-2 py-1 text-11 outline-none focus:border-[var(--color-accent)] ";

  // Visual grammar mirrors the distill card (MemoryDistillSettings.tsx):
  //   - Title row: chevron + "记忆遗忘" + trailing Switch.
  //   - Expanded body: the three numeric inputs go straight in.
  //   - Below the inputs: a single pale `disabledHint` line, rendered
  //     ONLY while the switch is off. It is the user's explanation of
  //     what the feature does, not a status readout — so we deliberately
  //     do NOT echo the three numeric values here (those values already
  //     live in the inputs above and re-printing them as prose would be
  //     pure duplication). Hiding the hint once the switch is on also
  //     matches the distill card, where `distillerDisabledHint` is gated
  //     on `!enabled` for the same reason: the card stops explaining
  //     itself the moment the user has opted in.
  // See commit history: an earlier draft rendered BOTH a `runtimeHint`
  // (with half-life / dormant / archive echoed from `forgettingStatus`)
  // AND the `disabledHint` paragraph above the inputs. That produced
  // three visible problem: (1) duplicate description, (2) duplicate
  // numeric values vs. the inputs below, (3) wrong position — the
  // description should sit below the controls per the distill card.
  //
  // Vertical rhythm: the wrapper carries `border-b border-border-divider`
  // so this card is separated from the 记忆搜索 card below by the same
  // hairline that separates 记忆沉淀 from this card. Spacing above /
  // below each hairline is the wrapper's `p-3` on each side — uniform
  // across the three lifecycle / search cards.
  return (
    <div className="border-b border-border-divider p-3">
      <ListBox dividers={false}>
        <ExpandableRow
          open={expanded}
          onToggle={() => setExpanded((v) => !v)}
          title={t("memoryPanel.forgettingTitle")}
          ariaLabel={t("memoryPanel.forgettingTitle")}
          trailing={
            <span onClick={(e) => e.stopPropagation()}>
              <Switch
                checked={enabled}
                onChange={handleToggle}
                disabled={!running || savingField === "forgetting_enabled"}
                size="sm"
                aria-label={t("memoryPanel.forgettingEnabled")}
              />
            </span>
          }
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-2"
        >
          <div className="flex flex-col gap-2">
            <div className="grid grid-cols-3 gap-2">
              <label className="flex flex-col gap-1">
                <span className="text-10 text-text-tertiary ">
                  {t("memoryPanel.forgettingHalfLife")}
                </span>
                <StyledInput
                  type="number"
                  min={1}
                  value={halfLifeInput}
                  placeholder={String(180)}
                  onChange={(e) => setHalfLifeInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber(
                      "forgetting_half_life_days",
                      "half_life_days",
                      e.target.value,
                      setHalfLifeInput,
                      parsePositiveInt,
                    )
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-10 text-text-tertiary ">
                  {t("memoryPanel.forgettingDormantThreshold")}
                </span>
                <StyledInput
                  type="number"
                  min={0}
                  max={1}
                  step={0.05}
                  value={dormantInput}
                  placeholder={String(0.1)}
                  onChange={(e) => setDormantInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber(
                      "forgetting_dormant_threshold",
                      "dormant_threshold",
                      e.target.value,
                      setDormantInput,
                      parseDormantThreshold,
                    )
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-10 text-text-tertiary ">
                  {t("memoryPanel.forgettingArchiveDays")}
                </span>
                <StyledInput
                  type="number"
                  min={1}
                  value={archiveInput}
                  placeholder={String(90)}
                  onChange={(e) => setArchiveInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber(
                      "forgetting_archive_days",
                      "archive_days",
                      e.target.value,
                      setArchiveInput,
                      parsePositiveInt,
                    )
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
            </div>
            {/* Disabled-state hint — single pale block, placed BELOW the
                numeric inputs (mirrors `distillerDisabledHint` in the
                distill card). Gated on `!enabled` so it only appears
                while the user has not yet opted in. */}
            {!enabled && (
              <p className="text-10 text-text-tertiary ">
                {t("memoryPanel.forgettingDisabledHint")}
              </p>
            )}
          </div>
        </ExpandableRow>
      </ListBox>
    </div>
  );
}
