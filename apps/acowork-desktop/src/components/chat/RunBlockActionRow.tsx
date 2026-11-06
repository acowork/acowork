/**
 * RunBlockActionRow — the icon row over a Chat execution block (design 27
 * §5.5.5).
 *
 * The block used to carry its affordances at both ends: a "View run" pill above
 * and a reply box below. Two chrome bands around one object read as two objects,
 * and the reply box's reserved height pushed the rest of the transcript down for
 * a control the human was not using. This row puts all four affordances above
 * the block, icon-only, in the order the reader meets them: open the run,
 * reference it, copy it, continue it.
 *
 * Why icon-only, and what that costs: four text buttons would be a toolbar
 * wider than the block header. The price is discoverability, so every button
 * keeps a `title`/`aria-label` that says what it does, and the "view run" one
 * carries the session's NAME (a fixed label repeating the glyph tells nobody
 * anything — the rule the pill this row replaced already followed). Nothing is hidden
 * behind hover: a control that only exists once the pointer finds it is a
 * control most humans never learn about.
 *
 * The reply drawer. `onSend` renders a capsule whose input slides OPEN to the
 * right of the message button rather than appearing under the block: the button
 * stays at the capsule's left edge, so the field reads as part of that button —
 * the "整体感" the two-band layout could not give. Mechanics that matter:
 * - the row's height is fixed (`h-6`), and only `max-width` transitions, so an
 *   opening drawer never moves the transcript below it;
 * - the field inside has a FIXED width while the capsule clips it — animating a
 *   container whose child re-wraps every frame is what makes CSS drawers jitter
 *   and text shimmer;
 * - the drawer closes only when the pointer has left AND the field is not
 *   focused AND the draft is empty. Losing typed words to a stray mouse-out is
 *   the one failure this pattern is famous for;
 * - the pointer-leave close is deferred a beat, so a cursor crossing the row
 *   does not strobe the field;
 * - `focusSignal` (the `@` picker's "去续投" locate) opens the drawer and moves
 *   the caret into it, so the human lands ready to type instead of hunting;
 * - the field is reachable by keyboard: the message button opens and focuses it
 *   on click/Enter, and Escape closes an empty drawer. A hover-only reveal would
 *   leave the whole continuation path pointer-only.
 *
 * Motion is CSS-only and inherits `prefers-reduced-motion` from globals.css
 * (which zeroes every transition duration), so no JS branch is needed here.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { AtSign, Check, Copy, MessageSquare, Send } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { copyText } from "../../lib/clipboard";
import { BLOCK_DETAIL_FONT_SIZE } from "./blockTypeScale";
import { OutlineBrainIcon } from "../common/BrainIcon";

/** One 24×24 hover circle shared by all four controls. Deliberately NOT
 *  `toolbarIconButton` (`h-7 w-7 rounded-md`, sized for the composer's cluster):
 *  these sit next to a 22px block header and read as one quiet row, so they are
 *  a size down and fully round. */
const ACTION_BUTTON =
  "inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/12 hover:text-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent disabled:hover:text-text-tertiary";

/**
 * The drawer's open width: the field (300px) + send + the icon's own 24px.
 *
 * Sized by what the human can actually read while typing. The first cut (168px)
 * showed a dozen characters and turned every follow-up into a horizontal-scroll
 * exercise; 300px holds roughly two dozen CJK characters at the block's detail
 * step, which is one ordinary sentence. It stays a FIXED px rather than a
 * `%`/`flex-1` because the capsule clips this child while it animates — the two
 * must not fight over the width frame by frame.
 */
const DRAWER_MAX_WIDTH = "max-w-[360px]";
const DRAWER_CLOSED_WIDTH = "max-w-6";
/** How long a pointer leave waits before it closes the drawer. Longer than the
 *  animation, short enough that the row feels responsive. */
const LEAVE_GRACE_MS = 100;
/** How long the copy button wears its checkmark. Matches `CodeBlock`. */
const COPIED_MS = 2000;
const ICON = "h-3.5 w-3.5";

interface RunBlockActionRowProps {
  /** The session's display name — the "view run" tooltip's subject. */
  runName: string;
  /** Opens the execution session this block belongs to. Absent = no drill. */
  onJump?: () => void;
  /** Attaches this session to the composer as a reference (ADR-106 D7). */
  onReference?: () => void;
  /** What "copy" puts on the clipboard. Empty disables the button rather than
   *  copying nothing. */
  copyPayload: string;
  /** In-place continuation (ADR-106 D6 level 1). Absent = the row has no drawer
   *  at all (a block nobody may answer). */
  onSend?: (text: string) => Promise<void>;
  /** A round to address. False renders the drawer inert — a brand-new block may
   *  have no round yet, and a send with no target is not a send. */
  canSend?: boolean;
  /** Rising edge: reveal and focus the drawer (the picker's locate). */
  focusSignal?: boolean;
  className?: string;
}

export function RunBlockActionRow({
  runName,
  onJump,
  onReference,
  copyPayload,
  onSend,
  canSend = true,
  focusSignal = false,
  className,
}: RunBlockActionRowProps) {
  const { t } = useTranslation();
  const [hovering, setHovering] = useState(false);
  const [focused, setFocused] = useState(false);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [copied, setCopied] = useState(false);
  /** Latched open by an explicit gesture (click or the locate signal) — survives
   *  the pointer leaving, released when the field empties and lets go. */
  const [latched, setLatched] = useState(false);
  const fieldRef = useRef<HTMLTextAreaElement | null>(null);
  const leaveTimer = useRef<number | null>(null);
  const copiedTimer = useRef<number | null>(null);
  const lastSignal = useRef(false);

  const clearLeave = useCallback(() => {
    if (leaveTimer.current !== null) {
      window.clearTimeout(leaveTimer.current);
      leaveTimer.current = null;
    }
  }, []);

  useEffect(
    () => () => {
      if (leaveTimer.current !== null) window.clearTimeout(leaveTimer.current);
      if (copiedTimer.current !== null) window.clearTimeout(copiedTimer.current);
    },
    [],
  );

  const open = hovering || focused || draft.trim().length > 0 || latched;

  /** Reveal, then move the caret. The order matters: focusing a clipped (0-wide)
   *  field scrolls the row sideways for a frame before the drawer catches up. */
  const openAndFocus = useCallback(() => {
    setLatched(true);
    requestAnimationFrame(() => fieldRef.current?.focus());
  }, []);

  useEffect(() => {
    if (focusSignal && !lastSignal.current) openAndFocus();
    lastSignal.current = focusSignal;
  }, [focusSignal, openAndFocus]);

  const submit = async () => {
    const text = draft.trim();
    if (!text || !onSend || !canSend || sending) return;
    setSending(true);
    try {
      await onSend(text);
      // Clear but stay open: an in-place continuation invites the next one.
      setDraft("");
    } finally {
      setSending(false);
    }
  };

  const copy = async () => {
    if (!copyPayload) return;
    if (!(await copyText(copyPayload))) return;
    setCopied(true);
    if (copiedTimer.current !== null) window.clearTimeout(copiedTimer.current);
    copiedTimer.current = window.setTimeout(() => setCopied(false), COPIED_MS);
  };

  const viewRunLabel = t("chatView.viewRun");
  const referenceLabel = t("chatView.block.actionReference");
  const copyLabel = t("chatView.block.actionCopy");
  const replyLabel = t("chatView.block.replyPlaceholder");

  return (
    <div
      className={cn("mb-1 flex min-w-0 items-center gap-0.5", className)}
      // One hover zone for the whole row: crossing from an icon into the drawer
      // is not "leaving".
      onMouseEnter={() => {
        clearLeave();
        setHovering(true);
      }}
      onMouseLeave={() => {
        clearLeave();
        leaveTimer.current = window.setTimeout(
          () => setHovering(false),
          LEAVE_GRACE_MS,
        );
      }}
    >
      {onJump && (
        <button
          type="button"
          onClick={onJump}
          title={runName.trim() || viewRunLabel}
          aria-label={viewRunLabel}
          className={ACTION_BUTTON}
        >
          <OutlineBrainIcon className={ICON} />
        </button>
      )}

      {onReference && (
        <button
          type="button"
          onClick={onReference}
          title={referenceLabel}
          aria-label={referenceLabel}
          className={ACTION_BUTTON}
        >
          <AtSign className={ICON} strokeWidth={1.75} />
        </button>
      )}

      <button
        type="button"
        onClick={() => void copy()}
        disabled={!copyPayload}
        title={copied ? t("chatView.block.copied") : copyLabel}
        aria-label={copyLabel}
        className={cn(ACTION_BUTTON, copied && "text-[var(--color-accent)]")}
      >
        {copied ? (
          <Check className={ICON} strokeWidth={1.75} />
        ) : (
          <Copy className={ICON} strokeWidth={1.75} />
        )}
      </button>

      {onSend && (
        <div
          data-run-action="reply"
          data-state={open ? "open" : "closed"}
          className={cn(
            "flex h-6 min-w-0 items-center overflow-hidden rounded-full bg-transparent transition-[max-width,background-color] ease-out",
            open
              ? `${DRAWER_MAX_WIDTH} bg-zinc-100 duration-200 dark:bg-zinc-800/60`
              : `${DRAWER_CLOSED_WIDTH} duration-150 ease-in`,
          )}
        >
          <button
            type="button"
            // Always open + focus: a click that sometimes closed the drawer
            // (over a row that hover keeps open anyway) read as a dead button.
            // Escape is the close gesture, and an empty drawer blurs itself away.
            onClick={openAndFocus}
            title={replyLabel}
            aria-label={replyLabel}
            aria-expanded={open}
            className={ACTION_BUTTON}
          >
            <MessageSquare className={ICON} strokeWidth={1.75} />
          </button>
          {/* Fixed width on purpose: the capsule clips this child, so the text
              never re-wraps mid-animation. */}
          <div
            className={cn(
              "flex w-[328px] shrink-0 items-center gap-1 pr-1 transition-opacity duration-150",
              open ? "opacity-100 delay-75" : "pointer-events-none opacity-0",
            )}
            // `inert` rather than only `aria-hidden`: a closed drawer must also
            // be out of the tab order, or the first Tab from the composer lands
            // in a field the human cannot see.
            inert={!open}
            aria-hidden={!open}
          >
            <textarea
              ref={fieldRef}
              rows={1}
              value={draft}
              disabled={sending}
              aria-label={replyLabel}
              placeholder={replyLabel}
              onFocus={() => setFocused(true)}
              onBlur={() => {
                setFocused(false);
                // An empty, unlatched drawer has no reason to stay open once the
                // caret leaves it; a draft or a latch keeps it.
                if (!draft.trim().length) setLatched(false);
              }}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  void submit();
                  return;
                }
                if (e.key === "Escape" && !draft.trim().length) {
                  e.preventDefault();
                  setLatched(false);
                  setHovering(false);
                  fieldRef.current?.blur();
                }
              }}
              style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
              className="h-5 w-[300px] resize-none border-0 bg-transparent px-1 leading-5 outline-none placeholder:text-text-disabled disabled:opacity-50"
            />
            <button
              type="button"
              onClick={() => void submit()}
              disabled={!draft.trim() || sending || !canSend}
              aria-label={t("chatView.block.send")}
              className={cn(ACTION_BUTTON, "h-5 w-5")}
            >
              <Send className="h-3 w-3" strokeWidth={1.75} />
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
