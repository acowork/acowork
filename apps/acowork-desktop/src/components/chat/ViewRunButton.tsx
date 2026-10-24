/**
 * ViewRunButton — the "view run" drill shared by every chat surface's agent
 * row: the 1:1 reply, the group reply, and the turn-reaction / router-fallback
 * notice alike (design 27 §6.1).
 *
 * The label is fixed ("View run"), so a tooltip that repeats it tells the
 * reader nothing — it would be a hint and a half. The drill opens one specific
 * execution session, so the tooltip carries that session's NAME. The caller
 * resolves it (`useSessionTitle`, the same overlay-then-snapshot order the tab
 * strip reads) and passes it here, keeping this button presentational. No name
 * yet — a brand-new turn not titled — falls back to the label.
 */
import { useTranslation } from "../../i18n/useTranslation";
import { OutlineBrainIcon } from "../common/BrainIcon";

interface ViewRunButtonProps {
  /** Display name of the execution session this drills into; empty/undefined
   *  falls the tooltip back to the label. */
  runName?: string;
  onJump: () => void;
  /** Extra classes; defaults to the compact under-bubble affordance. */
  className?: string;
}

export function ViewRunButton({ runName, onJump, className }: ViewRunButtonProps) {
  const { t } = useTranslation();
  const label = t("chatView.viewRun");
  return (
    <button
      type="button"
      onClick={onJump}
      title={runName?.trim() || label}
      className={
        className ??
        "inline-flex shrink-0 items-center gap-1 text-10 text-text-tertiary transition-colors hover:text-[var(--color-accent)]"
      }
    >
      <OutlineBrainIcon className="h-[11px] w-[11px] shrink-0" />
      {label}
    </button>
  );
}
