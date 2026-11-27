import { useEffect, useRef, useState } from "react";
import { Check, Eye, EyeOff, Lock, LockOpen } from "lucide-react";
import { useTranslation } from "react-i18next";
import { useAgentStore } from "../../stores/agentStore";
import { useToast } from "../common/ToastProvider";
import { Tooltip } from "../common/Tooltip";
import { ToolbarDropdownTrigger } from "../common/ToolbarDropdown";
import { cn } from "../../lib/utils";
import { menuItemHover, menuItemSelected, toolbarIconButton } from "../../lib/ui-styles";

type Visibility = "public" | "private";

/**
 * ADR-076 §决策 4: per-session read visibility.
 *
 * Public = readable by every signed-in account; private = owner + admins
 * only. It is a **read**-side setting, so it lives on the session (not on
 * the agent, which is a Gateway-wide shared object) and only the owner may
 * flip it — `can_write === false` renders the control disabled rather than
 * hiding it, so a viewer can still see what they are looking at.
 *
 * The write is optimistic (`agentStore.setSessionVisibility` patches the
 * session row, then PUTs) and is NOT behind a confirmation dialog: the
 * change is reversible, low-stakes, and the backend re-reports the truth
 * on the next `fetchSessions`.
 *
 * Two affordances, one state: `SessionVisibilityMenu` for the composer strip
 * and `SessionVisibilityToggle` for the Chat block's 24px action row, which
 * has no room for a menu.
 */
function useSessionVisibility(agentId: string, sessionId: string) {
  const { t } = useTranslation();
  const { addToast } = useToast();
  // Selecting the row object itself (not a derived boolean) keeps this
  // re-rendering only when `renameSession` / `setSessionVisibility` swap
  // the object — same read path as ChatPanel's `readOnlySession`.
  const session = useAgentStore((s) =>
    s.agents[agentId]?.sessions?.find((x) => x.session_id === sessionId),
  );
  const fetchSessions = useAgentStore((s) => s.fetchSessions);

  // The Chat surface does not guarantee this agent's session list is loaded
  // (the exec surface opens it on selection), and an agent row can exist
  // before its `sessions` array is hydrated. Without the row there is no
  // visibility / can_write to render, so pull it once per mount — the store
  // caches per agent, and the row's arrival re-renders this via the selector.
  const pulledRef = useRef(false);
  useEffect(() => {
    if (session || !fetchSessions || pulledRef.current) return;
    pulledRef.current = true;
    void fetchSessions(agentId);
  }, [session, agentId, fetchSessions]);

  // Absent on responses from an older Runtime → treat as public (the
  // on-disk default) instead of hiding the control.
  const isPrivate = session?.visibility === "private";
  const canWrite = session?.can_write !== false;

  const setVisibility = async (next: Visibility) => {
    try {
      await useAgentStore.getState().setSessionVisibility(agentId, sessionId, next);
      addToast({
        type: "success",
        message: t(
          next === "private"
            ? "sessionVisibility.toastNowPrivate"
            : "sessionVisibility.toastNowPublic",
        ),
      });
    } catch (e) {
      addToast({
        type: "error",
        message: t("sessionVisibility.toastFailed", {
          error: e instanceof Error ? e.message : String(e),
        }),
      });
    }
  };

  return {
    isPrivate,
    canWrite,
    label: isPrivate ? t("sessionVisibility.private") : t("sessionVisibility.public"),
    setVisibility,
  };
}

/**
 * Composer-strip affordance: the strip is the label row, so visibility is a
 * dropdown like model / reasoning / workspace — `Lock` / `Unlock` spelled
 * out, the active one checked. Naming the two states is what makes this
 * usable: a bare lock glyph pair does NOT read at icon size (see the toggle
 * below), and here the text carries the meaning instead.
 */
export function SessionVisibilityMenu({
  agentId,
  sessionId,
  textHidden,
  btnId,
}: {
  agentId: string;
  sessionId: string;
  /** Fold state from `useToolbarCollapse` — the label folds with its neighbours. */
  textHidden?: boolean;
  /** Toolbar button id used by ChatPanel's collapse observer */
  btnId?: string;
}) {
  const { t } = useTranslation();
  const { isPrivate, canWrite, label, setVisibility } = useSessionVisibility(agentId, sessionId);
  const [open, setOpen] = useState(false);
  const wrapperRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (wrapperRef.current && !wrapperRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  // `lock` = private (owner + admins), `unlock` = readable by everyone. The
  // trigger shows the current state, the menu offers both.
  const options: { value: Visibility; label: string }[] = [
    { value: "private", label: t("sessionVisibility.lock") },
    { value: "public", label: t("sessionVisibility.unlock") },
  ];

  return (
    <ToolbarDropdownTrigger
      icon={isPrivate ? <Lock size={14} /> : <LockOpen size={14} />}
      label={isPrivate ? t("sessionVisibility.lock") : t("sessionVisibility.unlock")}
      tooltip={canWrite ? label : t("chatPanel.readOnlySession")}
      open={open}
      onToggle={() => setOpen(!open)}
      disabled={!canWrite}
      wrapperRef={wrapperRef}
      textHidden={textHidden}
      btnId={btnId}
    >
      {open && (
        <div
          className="absolute bottom-full left-0 z-50 mb-1 w-[140px] overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-lg"
          role="menu"
          aria-label={label}
        >
          {options.map((opt) => {
            const isActive = (opt.value === "private") === isPrivate;
            return (
              <button
                key={opt.value}
                type="button"
                role="menuitemradio"
                aria-checked={isActive}
                onClick={() => {
                  if (!isActive) void setVisibility(opt.value);
                  setOpen(false);
                }}
                className={cn(
                  "flex w-full items-center gap-2 px-3 py-1.5 text-xs font-medium transition-colors",
                  isActive ? menuItemSelected : menuItemHover,
                )}
              >
                <span>{opt.label}</span>
                {isActive && <Check size={12} className="ml-auto text-[var(--color-accent)]" />}
              </button>
            );
          })}
        </div>
      )}
    </ToolbarDropdownTrigger>
  );
}

/**
 * Icon-only variant for the Chat block's action row (24px round cluster).
 *
 * The two states are `Eye` / `EyeOff` — visible to everyone / visible to the
 * owner only, which is literally what the setting toggles. A lock pair was
 * tried first and does NOT work at this size: `Lock` and `LockOpen` share a
 * byte-identical body (both a rect 18x11 at (3,11)) and differ only in the
 * shackle arc above it — 4 units of a 24 grid, about 2.3px once rendered at
 * 14. At that scale the two are the same picture, and the shackle's open side
 * (x=10 -> x=9.9) does not survive rasterisation at all. Shape has to carry
 * this toggle, so the pair must differ in outline, not in one 2px arc: `Eye`
 * vs `EyeOff` (eyelid + strike-through) stays legible because the whole
 * silhouette moves. (The composer strip avoids the problem entirely by
 * naming the states in `SessionVisibilityMenu`.)
 *
 * Size is 16, not the cluster's usual 14, and BOTH states use the same size.
 * lucide gives every glyph the same 24-unit box but fills it very unevenly —
 * measured from the shipped path data:
 *
 *     Eye       y ≈  7.0 → 17.0   (10 units tall, 42% of the box)
 *     EyeOff    y ≈  2.0 → 22.0   (20 units tall, 83% — the
 *                                  strike-through runs corner to corner)
 *     Paperclip y ≈  4.0 → 20.6
 *     Send      y ≈  2.1 → 22.2
 *
 * `Eye` therefore carries ~7 units of dead space above and below while its
 * counterpart carries almost none. At 14 the public state drew a 5.8px-tall
 * glyph beside 11.6px neighbours — visibly small, and it changed size on every
 * toggle. 16 lifts `Eye` to 6.6px and, more importantly, pinning both states
 * to one size makes the icon hold still while the user flips it. Scaling `Eye`
 * alone to fill the button was rejected: a visibly asymmetric pair is a worse
 * tell than a slightly small public glyph.
 *
 * This was `Globe` for public until the title bar's gateway switcher took the
 * same glyph for the opposite meaning: `Globe` was "this session is open to
 * the world", the switcher is "which network am I connected to". Two live
 * meanings for one mark on the same screen is worse than either reading alone
 * — and the eye reads "who can see this", which the globe never did.
 */
export function SessionVisibilityToggle({
  agentId,
  sessionId,
  buttonClassName,
}: {
  agentId: string;
  sessionId: string;
  /** Trigger style override — the Chat block's action row uses its own 24px
   *  round cluster instead of `toolbarIconButton`. */
  buttonClassName?: string;
}) {
  const { t } = useTranslation();
  const { isPrivate, canWrite, label, setVisibility } = useSessionVisibility(agentId, sessionId);

  return (
    <Tooltip
      content={
        canWrite ? `${label} — ${t("sessionVisibility.hintToggle")}` : t("chatPanel.readOnlySession")
      }
    >
      <button
        className={buttonClassName ?? toolbarIconButton}
        onClick={() => void setVisibility(isPrivate ? "public" : "private")}
        disabled={!canWrite}
        aria-label={label}
        aria-pressed={isPrivate}
      >
        {isPrivate ? <EyeOff size={16} /> : <Eye size={16} />}
      </button>
    </Tooltip>
  );
}