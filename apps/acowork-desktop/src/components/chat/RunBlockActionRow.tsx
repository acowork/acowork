/**
 * RunBlockActionRow — the icon row over a Chat execution block (design 27
 * §5.5.5).
 *
 * The block used to carry its affordances at both ends: a "View run" pill above
 * and a reply box below. Two chrome bands around one object read as two objects,
 * and the reply box's reserved height pushed the rest of the transcript down for
 * a control the human was not using. This row puts all the affordances above
 * the block, icon-only: open the run, reference it, copy it, then the run's own
 * composer controls (tool/attachment `+`, usage, visibility) so the human can
 * manage the run without entering it, and continue it.
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
 * - the button's accent tint lasts exactly as long as the drawer (`open`), not
 *   as long as the pointer is on the button: crossing from the button into its
 *   own field is the gesture the drawer exists for, and a tint that dropped
 *   there split the two halves of one object apart. See `ACTION_BUTTON_ACTIVE`;
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
import { AtSign, Check, Copy, MessageSquare, Paperclip, Send, X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { copyText } from "../../lib/clipboard";
import { BLOCK_DETAIL_FONT_SIZE } from "./blockTypeScale";
import { OutlineBrainIcon } from "../common/BrainIcon";
import { ComposerPlusMenu } from "./ComposerPlusMenu";
import { ContextUsageIcon } from "./ContextUsageIcon";
import { SessionVisibilityToggle } from "./SessionVisibilityToggle";
import { useChatStore } from "../../stores/chatStore";
import {
  useChatSessionStore,
  type ChatAttachmentMeta,
} from "../../stores/chatSessionStore";

/** One 24×24 hover circle shared by all four controls. Deliberately NOT
 *  `toolbarIconButton` (`h-7 w-7 rounded-md`, sized for the composer's cluster):
 *  these sit next to a 22px block header and read as one quiet row, so they are
 *  a size down and fully round.
 *
 *  `focus-visible:` mirrors `hover:`: an icon-only control has no label to
 *  announce that it is the one the keyboard is on, so the same tint stands in
 *  for the pointer's. */
const ACTION_BUTTON =
  "inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/12 hover:text-[var(--color-accent)] focus-visible:bg-[var(--color-accent)]/12 focus-visible:text-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent disabled:hover:text-text-tertiary";

/** The tint of an icon that currently OWNS something open — today only the
 *  message button, while its drawer is out.
 *
 *  Why it is not just `hover:`. The drawer is an extension of the button, so
 *  the human reads the pair as one object and the button has to keep its tint
 *  for exactly as long as the drawer is visible — including the whole time the
 *  pointer is in the field, which is where it goes to type. A `hover:` tint
 *  dies at that boundary and the object appears to fall apart mid-gesture
 *  (measured: the field stayed open, only the paint dropped). Binding the tint
 *  to the same `open` flag that sizes the capsule gives the two one lifetime:
 *  they light and go out together, by construction, whatever the pointer, the
 *  caret or a latched draft is doing.
 *
 *  Appended in the same `cn()` call as `ACTION_BUTTON`, so tailwind-merge drops
 *  the resting colour outright (verified: `text-text-tertiary` does not survive)
 *  — no reliance on stylesheet order. */
const ACTION_BUTTON_ACTIVE = "bg-[var(--color-accent)]/12 text-[var(--color-accent)]";

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
   *  at all (a block nobody may answer). `attachmentIds` are the files picked
   *  through the row's `+` menu, uploaded into THIS chat session; the router
   *  forwards them to the run alongside the text (`chat_attachment_ids`). */
  onSend?: (text: string, attachmentIds: string[]) => Promise<void>;
  /** A round to address. False renders the drawer inert — a brand-new block may
   *  have no round yet, and a send with no target is not a send. */
  canSend?: boolean;
  /** Rising edge: reveal and focus the drawer (the picker's locate). */
  focusSignal?: boolean;
  /** The execution session's agent (ADR-073 instance id) + session id. Present
   *  = the row carries the three composer controls (tools `+`, usage,
   *  visibility) so the human can manage the run without entering it. */
  agentId?: string;
  sessionId?: string;
  /** The Chat session these replies are sent into — where the drawer's
   *  attachments are uploaded (its `files/` dir, §4.2.1). */
  chatSessionId?: string;
  /** Whether this viewer may write the run (round answerer, ADR-106 D10).
   *  False = the `+` menu is read-only; usage stays readable, and the
   *  visibility toggle gates itself on `can_write`. */
  canManage?: boolean;
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
  agentId,
  sessionId,
  chatSessionId,
  canManage = true,
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
  // The usage ring renders ONLY when the store actually holds context usage
  // for this run. A never-opened, never-pushed run has no numbers — and a
  // ring pinned at 0% is a lie the row need not tell ("与其全0不如不显示").
  // The two paths that fill the store also flip this: opening the exec
  // session (loadSession's snapshot pull) and the run pushing
  // `sessions/+/state` while it works. The `+` menu and the visibility
  // toggle have no such condition — both their read AND write endpoints
  // fall back to persisted session meta for closed sessions (verified:
  // session_manager's `session_tool_policy` fs fallback, and
  // `set_session_tool_policy_persists_for_live_and_closed_sessions`).
  const usageKnown = useChatStore((s) =>
    !!(agentId && sessionId && s.agentStates[agentId]?.sessionStates[sessionId]?.contextUsage),
  );
  const leaveTimer = useRef<number | null>(null);
  const copiedTimer = useRef<number | null>(null);
  const lastSignal = useRef(false);
  // The drawer's pending attachments — files chosen through the row's `+`
  // menu, uploaded into the Chat session's `files/` dir at pick time (same
  // grammar as the Chat composer), and carried as ids by the next send.
  const attachInputRef = useRef<HTMLInputElement | null>(null);
  const [pendingAttachments, setPendingAttachments] = useState<ChatAttachmentMeta[]>([]);
  const [uploading, setUploading] = useState(false);

  const handleFilesChosen = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const files = Array.from(e.target.files ?? []);
    e.target.value = ""; // allow re-picking the same file
    if (!chatSessionId || files.length === 0) return;
    setUploading(true);
    try {
      for (const f of files) {
        const meta = await useChatSessionStore.getState().upload(chatSessionId, f);
        setPendingAttachments((prev) => [...prev, meta]);
      }
    } catch (err) {
      console.warn("[RunBlockActionRow] attachment upload failed:", err);
    } finally {
      setUploading(false);
    }
  };

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
      await onSend(text, pendingAttachments.map((a) => a.id));
      // Clear but stay open: an in-place continuation invites the next one.
      // The attachments rode THIS send — a pending file is never implicit
      // context for the next message.
      setDraft("");
      setPendingAttachments([]);
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

      {/* The run's composer controls, surfaced on the block so the human
          manages the run WITHOUT entering it: usage (the ring — click to
          open; the ring's percent is live via the Desktop's wildcard
          `sessions/+/state` subscription, the click only pulls the ≤3s
          coalescing gap) and read-visibility. They address the run by its
          own (agentId, sessionId), never the global selection. */}
      {agentId && sessionId && usageKnown && (
        <ContextUsageIcon
          agentId={agentId}
          sessionId={sessionId}
          refreshOnOpen
          trigger="click"
          buttonClassName={ACTION_BUTTON}
        />
      )}
      {agentId && sessionId && (
        <SessionVisibilityToggle
          agentId={agentId}
          sessionId={sessionId}
          buttonClassName={ACTION_BUTTON}
        />
      )}

      {/* The `+` menu's Attachments row drives this hidden picker; the chosen
          files become the drawer's pending attachments. */}
      <input
        ref={attachInputRef}
        type="file"
        multiple
        className="hidden"
        onChange={(e) => void handleFilesChosen(e)}
      />

      {/* The `+` sits directly LEFT of the reply button, deliberately: the
          attachments it picks ride that button's drawer send, so adjacency
          states the dependency (tools policy included — one menu, one run). */}
      {agentId && sessionId && (
        <ComposerPlusMenu
          readOnly={!canManage}
          disabled={!onSend}
          onPickAttachments={() => attachInputRef.current?.click()}
          agentId={agentId}
          sessionId={sessionId}
          showSkills={false}
          placement="bottom"
          portal
          buttonClassName={ACTION_BUTTON}
          openClassName={ACTION_BUTTON_ACTIVE}
        />
      )}

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
            className={cn(ACTION_BUTTON, open && ACTION_BUTTON_ACTIVE)}
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
          {pendingAttachments.length > 0 && (
            <div className="flex min-w-0 shrink-0 max-w-[150px] items-center gap-1 overflow-hidden">
              {pendingAttachments.map((a) => (
                <span
                  key={a.id}
                  data-run-attachment={a.id}
                  title={a.filename}
                  className="inline-flex h-5 min-w-0 max-w-[72px] shrink items-center gap-1 rounded-full bg-[var(--color-accent)]/12 px-1.5 text-text-secondary"
                  style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
                >
                  <Paperclip className="h-3 w-3 shrink-0" strokeWidth={1.75} />
                  <span className="min-w-0 truncate">{a.filename}</span>
                  <button
                    type="button"
                    onClick={() =>
                      setPendingAttachments((prev) => prev.filter((x) => x.id !== a.id))
                    }
                    aria-label={t("chatPanel.removeAttachment")}
                    className="shrink-0 text-text-tertiary hover:text-text"
                  >
                    <X className="h-3 w-3" strokeWidth={1.75} />
                  </button>
                </span>
              ))}
            </div>
          )}
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
              className={cn(
                "h-5 resize-none border-0 bg-transparent px-1 leading-5 outline-none placeholder:text-text-disabled disabled:opacity-50",
                // Fixed width while empty (the animation's no-jitter contract);
                // a pending-attachment row is a discrete user event, so the
                // one-step shrink cannot reflow mid-animation.
                pendingAttachments.length > 0 ? "w-[150px]" : "w-[300px]",
              )}
            />
            <button
              type="button"
              onClick={() => void submit()}
              // `uploading` gates the send: a file still in flight has no id
              // yet, and a send that silently drops a chosen attachment is
              // worse than a send button that visibly waits one beat.
              disabled={!draft.trim() || sending || !canSend || uploading}
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
