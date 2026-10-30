//! ADR-103 D11 position 1 — the `Ⓡ Responses` chip.
//!
//! Three visual tiers, no legend required: solid (verified), solid + yellow dot
//! (degraded — usable, but a field was trimmed), gray (offline declaration only,
//! never probed). A blocked selection renders the yellow "selected but not
//! enabled" mark instead, because `R` means "usable" and must never lie.
//!
//! Presentation only: the tier is decided by `deriveProtocolBadge`
//! (`lib/modelProtocol.ts`), which is where the D1 gate is projected. This
//! component owns nothing but the mapping badge kind → glyph/tone.

import { cn } from "../../lib/utils";
import { useTranslation } from "../../i18n/useTranslation";
import {
  showsResponsesMark,
  strippedFieldsText,
  type ProtocolBadge,
} from "../../lib/modelProtocol";

export function ProtocolChip({ badge }: { badge: ProtocolBadge }) {
  const { t } = useTranslation();
  if (badge.kind === "none") return null;
  if (badge.kind === "blocked") {
    return (
      <span className="text-amber-500" title={t(`harness.probeBlockedReason_${badge.reason}`)}>
        {t("harness.probeBlockedShort")}
      </span>
    );
  }
  const solid = showsResponsesMark(badge) && badge.kind !== "declared";
  // "degraded" must say *what* was dropped — a bare warning is not actionable.
  const stripped = strippedFieldsText(badge);
  return (
    <span
      className={cn(
        "inline-flex items-center gap-0.5",
        solid ? "text-text-secondary" : "text-text-tertiary",
      )}
      title={
        badge.kind === "verified"
          ? t("harness.probeVerifiedTip")
          : badge.kind === "degraded"
            ? stripped
              ? t("harness.probeDegradedTipFields", { fields: stripped })
              : t("harness.probeDegradedTip")
            : t("harness.probeDeclaredTip")
      }
    >
      <span aria-hidden="true">Ⓡ</span>
      {t("harness.shapeResponses")}
      {badge.kind === "degraded" && (
        <span className="h-1.5 w-1.5 rounded-full bg-amber-500" aria-hidden="true" />
      )}
    </span>
  );
}