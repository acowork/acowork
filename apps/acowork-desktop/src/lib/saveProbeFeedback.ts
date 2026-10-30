//! ADR-103 D12.1 — turn a provider save's probe verdicts into one message.
//!
//! The Gateway probes the `responses` rows a save changed and returns the
//! verdicts in the same round trip (`POST /api/providers` → `probes`,
//! `PUT /api/providers/{provider}` → `probes`). Before this module the shell
//! decoded the body as `{message}` only, so the verdicts were dropped: the user
//! was told "save to verify", the save did probe, and a failed verification
//! closed the dialog in silence. That is the defect this file exists to close.
//!
//! Keeping the summarising here (pure, no React, no i18n import) means both
//! save paths — the harness edit dialog and the add-provider flow — report the
//! same way, and the wording is unit-tested instead of duplicated.

/** One model's save-time probe verdict (mirror of `ProviderProbeReport`). */
export interface SaveProbeReport {
  model: string;
  protocol?: string;
  /** `standard` (L1+L2) or `deep` (+L3 round trip). */
  tier?: string;
  /** The verdict came out of the 24 h cache rather than a live call. */
  cached?: boolean;
  outcome: SaveProbeOutcome;
}

/** Mirror of `ProviderProbeOutcome`. */
export interface SaveProbeOutcome {
  /** Passed the D1 gate — `true` also for the degraded case below. */
  success: boolean;
  tier_passed?: string[];
  latency_ms?: number;
  observed_base_url?: string | null;
  error?: { class?: string; message?: string; http_status?: number | null } | null;
  /** Fields the endpoint rejected, so the probe sent without them (D8 trim). */
  stripped_fields?: string[];
}

/** The part of a save response this module reads. */
export interface ProviderSaveResultLike {
  probes?: SaveProbeReport[] | null;
}

export type SaveProbeFeedbackType = "success" | "warning" | "error";

export interface SaveProbeFeedback {
  type: SaveProbeFeedbackType;
  message: string;
}

type Translate = (key: string, options?: Record<string, unknown>) => string;

/** Models per toast before the list is abbreviated. */
const MAX_NAMED_MODELS = 3;

function nameModels(models: string[]): string {
  if (models.length <= MAX_NAMED_MODELS) return models.join(", ");
  const shown = models.slice(0, MAX_NAMED_MODELS).join(", ");
  return `${shown} +${models.length - MAX_NAMED_MODELS}`;
}

function isDegraded(report: SaveProbeReport): boolean {
  return report.outcome.success && (report.outcome.stripped_fields?.length ?? 0) > 0;
}

/**
 * Summarise the verdicts of one save.
 *
 * Returns `null` when nothing was probed — a save that touched no `responses`
 * row must stay silent rather than claim a verification that never ran.
 *
 * The three outcomes are ranked: a failure is never hidden behind a success,
 * and a partial success still names what did pass, so the message can never
 * be read as "everything is fine" when something is not.
 */
export function summarizeSaveProbes(
  probes: SaveProbeReport[] | null | undefined,
  t: Translate,
): SaveProbeFeedback | null {
  const list = probes ?? [];
  if (list.length === 0) return null;

  const failed = list.filter((p) => !p.outcome.success);
  const degraded = list.filter(isDegraded);
  const passed = list.length - failed.length;

  if (failed.length > 0) {
    const details = failed
      .map((p) => {
        const reason =
          p.outcome.error?.message?.trim() ||
          p.outcome.error?.class ||
          t("harness.saveProbeUnknownReason");
        return `${p.model}: ${reason}`;
      })
      .join("; ");
    const message =
      passed > 0
        ? t("harness.saveProbeFailedPartial", { details, count: passed })
        : t("harness.saveProbeFailed", { details });
    return { type: "error", message };
  }

  if (degraded.length > 0) {
    const fields = Array.from(
      new Set(degraded.flatMap((p) => p.outcome.stripped_fields ?? [])),
    ).join(", ");
    return {
      type: "warning",
      message: t("harness.saveProbeDegraded", {
        fields,
        models: nameModels(degraded.map((p) => p.model)),
      }),
    };
  }

  return {
    type: "success",
    message: t("harness.saveProbeVerified", {
      models: nameModels(list.map((p) => p.model)),
    }),
  };
}
