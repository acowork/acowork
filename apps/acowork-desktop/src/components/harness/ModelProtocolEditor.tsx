//! ADR-103 D11 position 3+4 — per-model protocol dropdown, base_url override,
//! "re-verify" button and the verdict status area.
//!
//! Presentation only: every decision (does this selection take effect? why not?)
//! arrives as a `ProtocolBadge` computed by `lib/modelProtocol.ts`, and the
//! probe call is issued by `useModelProtocolState`. This component must not
//! re-implement the gate.

import type { ReactNode } from "react";
import { Dropdown } from "../common/Dropdown";
import { StyledInput } from "../common/StyledInput";
import { useTranslation } from "../../i18n/useTranslation";
import {
  type ProtocolBadge,
  type ProtocolOption,
  type ProtocolProbe,
} from "../../lib/modelProtocol";
import type { ProbeOutcomeSummary } from "./useModelProtocolState";

interface ModelProtocolEditorProps {
  modelId: string;
  /** Current dropdown value. */
  selection: ProtocolOption;
  onSelectionChange: (next: ProtocolOption) => void;
  /** base_url override (empty = inherit the provider's). */
  baseUrl: string;
  onBaseUrlChange: (next: string) => void;
  /** Whether the base_url input is relevant (only for Responses). */
  showBaseUrl: boolean;
  /** Badge projected from the stored fields + this dialog's pending choice. */
  badge: ProtocolBadge;
  /** Stored probe verdict, for the "last verified" line. */
  probe?: ProtocolProbe;
  /** Latest manual re-verify result in this dialog session. */
  outcome?: ProbeOutcomeSummary;
  verifying: boolean;
  onVerify: () => void;
  /** Whether probing is possible at all (needs a saved provider + a model). */
  canVerify: boolean;
}

/** Human label for a blocked reason (i18n key suffix). */
function blockedLabelKey(badge: ProtocolBadge): string | null {
  if (badge.kind !== "blocked") return null;
  return `harness.probeBlockedReason_${badge.reason}`;
}

export function ModelProtocolEditor({
  modelId,
  selection,
  onSelectionChange,
  baseUrl,
  onBaseUrlChange,
  showBaseUrl,
  badge,
  probe,
  outcome,
  verifying,
  onVerify,
  canVerify,
}: ModelProtocolEditorProps): ReactNode {
  const { t } = useTranslation();
  const blockedKey = blockedLabelKey(badge);

  return (
    <div className="mt-1.5 border-t border-border-divider pt-1.5" data-model-id={modelId}>
      <div className="flex gap-2">
        <div className="flex-1">
          <label className="mb-0.5 block text-xs text-text-tertiary">
            {t("harness.protocol")}
          </label>
          <Dropdown
            value={selection}
            onChange={(v) => onSelectionChange(v as ProtocolOption)}
            options={[
              // D11 red line: `Responses` is offered for every model, whether
              // or not the offline data tagged it.
              { value: "follow", label: t("harness.protocolFollowData") },
              { value: "openai", label: t("harness.protocolChatCompletions") },
              { value: "responses", label: t("harness.shapeResponses") },
            ]}
          />
        </div>
        <div className="flex items-end">
          <button
            type="button"
            onClick={onVerify}
            disabled={!canVerify || verifying}
            title={
              canVerify
                ? t("harness.probeSendsOneRequest")
                : t("harness.probeNeedsSaveFirst")
            }
            className="rounded border border-border-outer px-2 py-1 text-xs text-text-secondary hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-800"
          >
            {verifying ? t("harness.probeRunning") : t("harness.probeButton")}
          </button>
        </div>
      </div>

      {showBaseUrl && (
        <div className="mt-1.5">
          <label className="mb-0.5 block text-xs text-text-tertiary">
            {t("harness.protocolBaseUrl")}
          </label>
          <StyledInput
            type="text"
            value={baseUrl}
            onChange={(e) => onBaseUrlChange(e.target.value)}
            placeholder="https://api.example.com/v1"
          />
          <p className="mt-0.5 text-xs text-text-tertiary">
            {t("harness.protocolBaseUrlHint")}
          </p>
        </div>
      )}

      {/* Status area — three visual tiers per D11. */}
      <div className="mt-1.5 text-xs">
        {badge.kind === "verified" && (
          <span className="text-accent-green">{t("harness.probeVerified")}</span>
        )}
        {badge.kind === "degraded" && (
          <span className="text-amber-500">
            {t("harness.probeDegraded")}
            {badge.tiers?.length ? ` (${badge.tiers.join(" / ")})` : ""}
          </span>
        )}
        {badge.kind === "declared" && (
          <span className="text-text-tertiary">{t("harness.probeDeclared")}</span>
        )}
        {badge.kind === "blocked" && blockedKey && (
          <span className="text-amber-500">
            {t("harness.probeBlocked")}: {t(blockedKey)}
            {badge.detail ? ` — ${badge.detail}` : ""}
          </span>
        )}
        {badge.kind === "none" && (
          <span className="text-text-tertiary">{t("harness.probeNone")}</span>
        )}
        {outcome?.detail && badge.kind !== "blocked" && (
          <span className="ml-1 text-text-tertiary">{outcome.detail}</span>
        )}
        {probe?.probed_at && (
          <span className="ml-1 text-text-tertiary">
            {t("harness.probeLastVerified")}: {probe.probed_at}
          </span>
        )}
      </div>

    </div>
  );
}