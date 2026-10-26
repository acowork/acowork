// Design 27 Wave 5: the Chat session surface — the Agent page's primary
// view. One Chat session: a pure durable EVENT LOG of point-in-time rows
// (messages / decision cards / system notices) from the user service, and a
// composer that is the SAME visual language as the execution view (same
// container, same textarea, same toolbar components —
// plan 5.4: "输入框能力与执行视图一致").
//
// Deliberate deltas from the execution composer, all of them semantic:
// - ModelMenu / ReasoningEffortMenu / WorkspaceSelector are WRITABLE, but a
//   pick is a `session_config` DRAFT held on the Chat session ROW (design 27
//   §4.5) — the store only caches what the server returned: `setCurrentModel`
//   needs an active execution session, and a Chat turn's session is created
//   server-side when the intent lands. The draft rides `sendText`, and per
//   ADR-099's 2026-10-28 revision it is an initial value for a session the
//   router CREATES — it is never applied to a session that already exists,
//   not model, not reasoning, and above all not `workspace`. That is deliberate: the workspace is the directory that
//   session's files, tools, history and Add-to-Chat references already point
//   at, and moving it mid-conversation invalidates them. So on the Chat
//   surface the pick is the conversation's own workspace (it drives this
//   surface's right-hand panel, see `WorkspaceExplorer`), not a live switch of
//   anything. Either way the choice is fixed BEFORE the turn, not after, which
//   is the whole point (pick model/workspace/reasoning first).
//   Persisting it on the session (not in component state) is what keeps the
//   last pick across tab switches, reloads and devices.
// - No SkillsPanel here at all. Skills are auto-loaded by the model, not
//   session-seeded (they are deliberately outside the §4.5 seed), and the
//   chat-message channel carries no skill/command field — there is nothing
//   this surface could write per-message, so the chip is hidden rather than
//   shown disabled with a "can't use this" tooltip.
// - Attachments: the composer's Paperclip uploads into the Chat session's
//   `files/` dir (§4.2.1) and the send carries the resolved ids on the intent
//   `chat-message` request, so the durable row shows them and the Gateway
//   forwards them to the execution turn.
// - No @-mention UI: a 1:1 user_agent session has exactly one other
//   participant (design 27 §5.5.4 "单聊无需 @").
//
// The chat stream persists conversation, not progress (ADR-100 revision): the
// turn's dispatch / round-open milestones are no longer rows, and the live
// waiting line is DERIVED from "the newest human message has no answer yet"
// (`chatTurnState.pendingTurn`). Each row that resolved an execution session
// carries a discrete drill-down into the execution view:
// `chatStore.openSession(agentId, execSid)` + mode flip (plan 5.3).

import {
  useCallback,
  useEffect,
  useMemo,
  memo,
  useRef,
  useState,
  type UIEvent,
} from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { useTranslation } from "../../i18n/useTranslation";
import {
  Send,
  Paperclip,
  Loader2,
  ChevronDown,
  ChevronsDown,
} from "lucide-react";
import { formatBubbleTime } from "../../lib/formatTime";
import { cn } from "../../lib/utils";
import { toolbarIconButton } from "../../lib/ui-styles";
import { useStickToBottom } from "../../lib/stickToBottom";
import { Tooltip } from "../common/Tooltip";
import { UserAvatar } from "../common/UserAvatar";
import { AgentAvatar } from "../common/AgentAvatar";
import { ViewRunButton } from "./ViewRunButton";
import { useSessionTitle } from "../../lib/useSessionTitle";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { ChatAttachmentChip } from "./ChatAttachmentChip";
import { ExecAttachmentLabels } from "./ExecAttachmentLabels";
import { ReplyQuote } from "./ReplyQuote";
import { detectPaste } from "./pasteDetect";
import { ChatMarkdown } from "./ChatMarkdown";
import {
  VirtualRowList,
  type VirtualRowListHandle,
} from "../common/VirtualRowList";
import { ModelMenu, ReasoningEffortMenu } from "./ChatPanel";
import { defaultEffortOf } from "../../lib/modelCapabilities";
import { WorkspaceSelector } from "../workspace/WorkspaceSelector";
import { ChatNoticeEventRow, truncateTaskName } from "./ChatNoticeEventRow";
import { ChatTurnStatusLine } from "./ChatTurnStatusLine";
import { AttachedContextChips } from "./AttachedContextChips";
import {
  isFailureStatus,
  isEmptyTerminalReply,
  isTurnRoutedNotice,
  pendingTurn,
  terminalNoticeKey,
} from "./chatTurnState";
import { InlineInteractionCard } from "./InlineInteractionCard";
import { useLocateMessage } from "../../lib/useLocateMessage";
import {
  useChatSessionStore,
  closedCardIds,
  humanAnsweredCardIds,
  type ChatMessageRow,
  type ChatSessionConfig,
  type ChatAttachmentMeta,
} from "../../stores/chatSessionStore";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { sessionExists } from "../../lib/session-control";
import { showToast } from "../common/ToastProvider";
import { useAuthStore } from "../../stores/authStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { getGatewayUrl } from "../../lib/config";

// ── Rows ───────────────────────────────────────────────────────────────

interface RowProps {
  row: ChatMessageRow;
  agentId: string;
  /** Chat session the row belongs to — attachment thumbnails are addressed
   *  through it (the `files/` dir is session-scoped, §4.2.1). */
  chatSessionId: string;
  showHeader: boolean;
  /** Drill into the execution session that produced this agent message.
   *  Every `agent_message` with a resolved exec session id is clickable —
   *  finished, failed or empty-bodied alike (§5.5.5: the exec JSONL stays
   *  in Agent home). */
  onJumpToExec: (execSid: string) => void;
  /** Cards that can no longer be answered — the inline card paints an
   *  answered/closed receipt for these instead of an answer affordance. */
  closed: Set<string>;
  /** The subset of closed cards the human actually replied to. */
  answered: Set<string>;
  /** Answer an `interaction_request` card through the chat answer channel. */
  onAnswer: (row: ChatMessageRow, text: string, immediate: boolean) => void;
  /** Body of the message that triggered this row's turn (`meta.in_reply_to`).
   *  Names the run in the drill-down tooltip while the exec session still has
   *  no title. Resolved by the parent through its message_id → body index. */
  triggerBody?: string;
  /** Question text of the `interaction_request` card this row answers, when it
   *  answers one. Paints the one-line quote above the answer bubble — the
   *  visible half of the §5.5.4 `in_reply_to` link the answered card used to
   *  carry. Only cards are quoted: the parent resolves this through a
   *  card-only index so a reply to any other row kind paints nothing. */
  replyQuote?: string;
}

/**
 * One transcript row. Memoized: the composer's per-keystroke re-render of the
 * host must not cascade into `rows.map` re-parsing every markdown body
 * (chat-transcript render-perf plan L1). All props are value-stable while the
 * window is unchanged — `row` references come from the store window, Sets are
 * memoized per window, callbacks are host-level `useCallback`s. The
 * render-count regression test (`AgentChatView.memo.test.tsx`) locks this.
 */
const RowView = memo(function RowView({
  row,
  agentId,
  chatSessionId,
  showHeader,
  onJumpToExec,
  closed,
  answered,
  onAnswer,
  triggerBody,
  replyQuote,
}: RowProps) {
  const { t } = useTranslation();
  const agent = useAgentStore((s) => s.agents[agentId]?.meta);
  const profile = useUserProfileStore((s) => s.profile);
  const agentName = agent?.display_name ?? agent?.name ?? "";

  // The exec session this row can drill into. Its name feeds the "view run"
  // tooltip so the hover says which run it opens, not just "view run". Read
  // before the kind branches so the store hook stays unconditional. The
  // routing reaction row (ADR-100 D11) names the task in its bubble, so it
  // resolves its title through the same hook.
  const routed = isTurnRoutedNotice(row);
  const drillSid =
    row.kind === "agent_message" || routed
      ? row.meta?.agent_session_id || row.meta?.sender_session_id || ""
      : undefined;
  const execRunName = useSessionTitle(agentId, drillSid);

  if (row.kind === "system_notice") {
    if (routed) {
      // ADR-100 D11: the routing reaction — the agent's immediate in-chat
      // answer to "where did my message go". It is a runtime-written row, but
      // it speaks as the agent: synthesized into the ordinary agent-bubble
      // path below so it carries the avatar + name header and the drill, and
      // never paints as a centered system line. The conversational rule it
      // exists for: a user message is always followed by an agent message.
      const taskName =
        execRunName?.trim() || truncateTaskName(triggerBody) || "";
      row = {
        ...row,
        kind: "agent_message",
        body: t(
          row.meta?.turn_mode === "new"
            ? "chatView.notice.turnRoutedNew"
            : "chatView.notice.turnRoutedContinue",
          { task: taskName },
        ),
      };
    } else {
      // Legacy receipts only — the milestones and the router-fallback receipt
      // are gone from the stream (ADR-100 revision). Whatever survived paints
      // as a quiet centered line, never as an agent bubble.
      return <ChatNoticeEventRow row={row} className="px-4" />;
    }
  }

  if (row.kind === "interaction_request") {
    // A decision the agent asked a human to make (ask_question / approval).
    // It is a durable, actionable row in the stream — answered in place
    // through the chat answer channel (§5.5.4).
    return (
      <InlineInteractionCard
        row={row}
        closed={closed}
        answered={answered}
        askerName={agentName}
        onAnswer={onAnswer}
      />
    );
  }

  if (row.kind === "agent_message") {
    // §7.1 outcomes: `failed` / `timeout` are errors and paint as an error
    // line; `aborted` is the user stopping a run — a fact of the transcript,
    // not a fault, so it never paints red.
    const failed = isFailureStatus(row.meta?.status);
    // The execution session that produced this reply: a 1:1 agent message
    // carries it as `sender_session_id` (the responder's own session); a
    // group / routed reply carries `agent_session_id` (routing-resolved).
    // Either way every agent message is drillable, finished or empty alike.
    const execSid =
      row.meta?.agent_session_id || row.meta?.sender_session_id || "";
    // A terminal row with nothing in it IS its outcome ("interrupted",
    // "failed"): paint the centered status line with the drill into the run,
    // never an agent bubble announcing it in the agent's voice.
    if (isEmptyTerminalReply(row)) {
      return (
        <ChatTurnStatusLine
          className="px-4"
          text={t(terminalNoticeKey(row.meta?.status))}
          danger={failed}
          onJump={execSid ? () => onJumpToExec(execSid) : undefined}
          runName={
            execRunName?.trim() || truncateTaskName(triggerBody) || undefined
          }
        />
      );
    }
    return (
      <div className="px-4 py-1">
        {showHeader && (
          <div className="flex items-center gap-2 mb-2 mt-1">
            <AgentAvatar
              agentId={agentId}
              displayName={agentName}
              avatarUrl={agent?.avatar}
              version={agent?.version}
              builtinAvatarId={agent?.builtin_avatar ?? null}
              size={40}
              className="shrink-0"
            />
            <div className="flex flex-col">
              <span className="text-xs font-medium text-text-tertiary ">
                {agentName}
              </span>
              {agent?.role && (
                <span className="text-10 leading-tight text-text-tertiary ">
                  {agent.role}
                </span>
              )}
            </div>
          </div>
        )}
        <div className="flex items-start gap-2">
          <div className="w-10 shrink-0" />
          {/* Content column mirroring GroupChatPanel's grammar: the percentage
            line-length clamp resolves against the column (row minus the avatar
            gutter), so the bubble hugs the gutter and stops at the same line
            length as on the group-chat / execute-session surfaces. Clamping
            against the full row instead let the bubble reach 90% of the row
            while starting 48px in — it read as a fixed-width centered column.
            The clamp stays on this shrink-wrap group, not on the bubble: on
            the inline-block bubble itself it double-clamps and the text
            overflows right. */}
          <div className="flex min-w-0 flex-1 flex-col items-start">
            <div className="group min-w-0 max-w-[var(--content-max-width)]">
              {/* Attachment chips above the bubble, left-aligned — the mirror
                of the user row; the bubble never wraps them. An attachments-
                only reply paints chips alone, no empty bubble. */}
              {row.attachments && row.attachments.length > 0 && (
                <div className="mb-1 flex flex-wrap gap-1.5">
                  {row.attachments.map((a) => (
                    <ChatAttachmentChip
                      key={a.id}
                      chatSessionId={chatSessionId}
                      attachment={a}
                    />
                  ))}
                </div>
              )}
              {/* `inline-block`: the bubble hugs its text (symmetric padding,
                no width jump when the hover timestamp widens the group).
                `max-w-full` is what keeps that hug honest: shrink-to-fit falls
                back to the content's *minimum* width whenever the content
                cannot be squeezed — a mermaid SVG's natural width, an
                unwrapped table, one long unbreakable token — and an uncapped
                hug then grew the bubble past the row clamp and spilled the
                reply under the next panel. Bounding the used width lets the
                inner scroll containers (`CodeBlock`, `MermaidBlock`,
                `.prose-table-scroll`) do their job inside it, which is also
                the contract Mobile's `.bubble` has always had. */}
              {(row.body || failed || !row.attachments?.length) && (
                <div className="inline-block max-w-full rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5  select-text break-words">
                  {failed && (
                    <div className="mb-1 rounded bg-red-50 px-2 py-0.5 text-10 text-red-500 dark:bg-red-900/20">
                      {row.meta?.status}
                    </div>
                  )}
                  {row.body && <ChatMarkdown content={row.body} />}
                  {/* Legacy rows without a §7.1 status: there is no outcome to
                    name, so the placeholder stays deliberately neutral. Modern
                    empty terminals never reach here — they paint as the status
                    line above. */}
                  {!row.body && !row.attachments?.length && (
                    <span className="italic text-text-tertiary whitespace-nowrap">
                      {t("chatView.emptyAnswer")}
                    </span>
                  )}
                </div>
              )}
              {/* Meta row under the bubble. The timestamp collapses to zero width
                instead of just going transparent: an invisible span still
                reserves its width, which is what stranded the button
                mid-bubble. Collapsed, the button sits on the bubble's left
                edge and the timestamp unfurls right after it on hover —
                `justify-between` would fling them to opposite ends of a
                wide bubble, leaving a gap far larger than the meta row
                warrants. */}
              <div className="mt-1 flex items-center gap-2">
                {/* Drill-into-exec affordance — a discrete control, not a
                  whole-bubble click, so selecting the reply text never fires a
                  jump. Present on every agent message with a resolved exec
                  session, empty body included. The brain glyph (design 27
                  §6.1) is the same one the exec tabs wear, so the button and
                  its destination read as the same thing; outline variant
                  because at 11px the two filled lobes smear into a blob. */}
                {execSid && (
                  <ViewRunButton
                    runName={
                      execRunName?.trim() ||
                      truncateTaskName(triggerBody) ||
                      undefined
                    }
                    onJump={() => onJumpToExec(execSid)}
                  />
                )}
                <span className="w-0 shrink overflow-hidden whitespace-nowrap text-10 text-text-tertiary opacity-0 transition-opacity group-hover:w-auto group-hover:opacity-100">
                  {formatBubbleTime(row.ts * 1000)}
                </span>
              </div>
            </div>
          </div>
        </div>
      </div>
    );
  }

  // user_message (and anything else) — the human side, right-aligned.
  return (
    <div className="px-4 py-1">
      <div className="flex items-start justify-end gap-2">
        <div className="min-w-0 flex-1 flex flex-col items-end">
          {profile.displayName && (
            <span className="mt-[2px] text-xs text-text-tertiary ">
              {profile.displayName}
            </span>
          )}
          {/* Clamp on the global content-width variable, not a hardcoded
              percentage: same as GroupChatPanel's own row — a long message
              reaches the agent bubble's line length and follows the setting. */}
          <div className="group mt-[6px] max-w-[var(--content-max-width)] flex flex-col items-start">
            {/* Attachment chips — independent units above the text bubble,
                right-aligned: same grammar as the execution session's
                `UserWithAttachmentsBubble`. The bubble never wraps them. */}
            {row.attachments && row.attachments.length > 0 && (
              <div className="mb-1 flex flex-wrap justify-end gap-1.5 self-end">
                {row.attachments.map((a) => (
                  <ChatAttachmentChip
                    key={a.id}
                    chatSessionId={chatSessionId}
                    attachment={a}
                  />
                ))}
              </div>
            )}
            {/* ADR-100 D12: this turn was sent from an execution view's
                composer, so its attached items belong to that run, not to
                this chat session — labels only, and not clickable. They paint
                above the text like the chat's own chips, which is also what
                keeps a file-with-no-words turn from collapsing to a blank
                row. */}
            <ExecAttachmentLabels labels={row.meta?.exec_attachments ?? []} />
            {/* What this answer answers. The card itself is gone once answered
                (§5.5.4 records the answer here, not on a receipt). */}
            <ReplyQuote quote={replyQuote} />
            {row.body && (
              <div className="max-w-full rounded-md rounded-br-sm bg-chat-user text-chat-user-text select-text">
                <div className="px-4 py-2.5 whitespace-pre-wrap break-words">
                  {row.body}
                </div>
              </div>
            )}
            <span className="mt-1 text-10 text-text-tertiary  opacity-0 transition-opacity group-hover:opacity-100">
              {formatBubbleTime(row.ts * 1000)}
            </span>
          </div>
        </div>
        <UserAvatar
          displayName={profile.displayName}
          avatarUrl={profile.backendAvatarUrl ?? null}
          builtinAvatarId={profile.backendBuiltinAvatarId ?? null}
          size={40}
          className="shrink-0 mt-1"
        />
      </div>
    </div>
  );
});

// ── View ───────────────────────────────────────────────────────────────

interface AgentChatViewProps {
  agentId: string;
  /** The pair's single Chat session id (from the store — pinned, there is
   *  no session switch on this surface). */
  sessionId: string;
}

/** Stable empty-draft reference for the selector (zustand would re-render
 *  on every store tick with a fresh literal). */
const EMPTY_DRAFT: ChatSessionConfig = {};

/** How close to the top (px) trips auto-pagination. The "load earlier" status
 *  band sits here; scrolling into it fires the next older page. */
const AUTO_LOAD_TOP_PX = 48;

/** Optimistically dismiss the corresponding question/approval card in the
 *  execute session view when the user answers from the chat surface. The
 *  MQTT `event_cleared` path (zero-byte retained publish) is unreliable with
 *  rumqttd for already-connected subscribers, so we clear local state
 *  immediately rather than waiting for the signal. */
function dismissExecCard(card: ChatMessageRow) {
  const execSid = card.meta?.agent_session_id;
  const reqId = card.meta?.interaction?.request_id;
  const agentId = card.from;
  if (execSid && reqId && agentId) {
    useChatStore.getState().resolveQuestion(agentId, reqId, execSid);
  }
}

export function AgentChatView({ agentId, sessionId }: AgentChatViewProps) {
  const { t } = useTranslation();
  const win = useChatSessionStore((s) => s.windows[sessionId]);
  const sending = !!useChatSessionStore((s) => s.sending[sessionId]);
  const pendingLocateSeq = useChatSessionStore(
    (s) => s.windows[sessionId]?.pendingLocateSeq ?? null,
  );
  // The seed draft is the CACHE of the session row's `session_config`
  // (design 27 §4.5, backend-persisted): hydrated by `openAgent`, updated
  // by `saveDraft`'s PATCH response. Stable ref so the selector never
  // re-renders on an absent draft.
  const draft = useChatSessionStore((s) => s.draft[sessionId] ?? EMPTY_DRAFT);
  const {
    sendText,
    loadOlder,
    loadNewer,
    clearLocate,
    setMode,
    saveDraft,
    upload,
  } = useChatSessionStore.getState();

  const [input, setInput] = useState("");
  const [answerTarget, setAnswerTarget] = useState<ChatMessageRow | null>(null);
  // Composer attachments — uploaded into the Chat session's `files/` dir
  // (§4.2.1) before send; the send carries their ids so the durable row and
  // the execution turn both see them.
  const [pendingAttachments, setPendingAttachments] = useState<
    ChatAttachmentMeta[]
  >([]);
  const [uploading, setUploading] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const lastCompositionEndRef = useRef(0);
  // The scroll viewport, captured alongside `useStickToBottom`'s callback ref
  // so the locate effect can query `[data-seq]` rows within it.
  const containerRef = useRef<HTMLDivElement | null>(null);
  // Virtualized row window (render-perf L2): imperative handle for locate
  // fallbacks (an off-viewport row is NOT in the DOM) and visibility queries.
  const listRef = useRef<VirtualRowListHandle | null>(null);

  // The agent's current model, read-only here (see header comment).
  const [agentModel, setAgentModel] = useState<{
    model: string;
    provider: string;
  } | null>(null);
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const token = useAuthStore.getState().accessToken;
        const resp = await fetch(
          `${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/model`,
          {
            headers: token ? { Authorization: `Bearer ${token}` } : {},
          },
        );
        if (resp.ok && !cancelled) {
          const data = (await resp.json()) as {
            model: string;
            provider: string;
          };
          setAgentModel(data);
        }
      } catch {
        /* toolbar simply shows no model chip */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [agentId]);

  const rows = useMemo(() => (win ? [...win.older, ...win.tail] : []), [win]);
  // Virtualization adapters (render-perf L2). Keys are session-qualified:
  // the shell's measured-height cache is module-level, and `seq` is unique
  // only WITHIN a chat session. `rowsRef` lets the locate fallback resolve a
  // seq to an index without re-creating the (stamp-keyed) locate effect.
  const rowKeyOf = useCallback(
    (r: ChatMessageRow) => `${sessionId}:${r.seq}`,
    [sessionId],
  );
  const rowsRef = useRef(rows);
  rowsRef.current = rows;
  // Coarse pre-measure estimate from body length; `measureElement` corrects
  // it on first mount and the shell's cache keeps the real height after.
  const estimateRow = useCallback((r: ChatMessageRow) => {
    const body = r.body ?? "";
    const lines = Math.max(1, Math.ceil(body.length / 70));
    return Math.min(480, 32 + lines * 22);
  }, []);
  // message_id → body index, so a reply (whose trigger is only an
  // `in_reply_to` id) can name the task in its drill-down tooltip when the
  // execution session has no title yet.
  const bodyById = useMemo(() => {
    const m = new Map<string, string>();
    for (const r of rows) {
      if (r.meta?.message_id) m.set(r.meta.message_id, r.body);
    }
    return m;
  }, [rows]);
  // Card rows only, so an answer can quote the question it answers. Deliberately
  // separate from `bodyById`: `in_reply_to` also points at plain messages, and
  // quoting one of those would paint a quote where there is no decision. The
  // text resolves question-first, exactly as the card itself renders it, so the
  // quote says what the reader saw on the card (the row body is only a summary).
  const cardQuoteById = useMemo(() => {
    const m = new Map<string, string>();
    for (const r of rows) {
      if (r.kind !== "interaction_request" || !r.meta?.message_id) continue;
      m.set(r.meta.message_id, r.meta.interaction?.question || r.body);
    }
    return m;
  }, [rows]);
  // Optimistic exec-session title seed (ADR-099). A Router-created execute
  // session has no list row and often no open tab, so it would read "Untitled"
  // until the backend's generated title arrives over MQTT. The reply row is now
  // the first durable place the exec session id appears (the routing pointer
  // that used to expose it earlier is gone with the ADR-100 revision), so seed
  // a provisional title off the message that triggered it. `seedOptimisticTitle`
  // no-ops once any title exists, so the authoritative (possibly LLM-refined)
  // push always wins afterwards. Guarded per execSid so it fires at most once.
  const seededTitleRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    for (const r of rows) {
      if (
        r.kind !== "agent_message" ||
        !r.meta?.sender_session_id ||
        !r.meta?.in_reply_to
      ) {
        continue;
      }
      const execSid = r.meta.sender_session_id;
      if (seededTitleRef.current.has(execSid)) continue;
      // Only pin the execSid once a body is available — the reply row can land a
      // window before its trigger row is readable, and marking it early would
      // permanently skip the seed. `seedOptimisticTitle` is itself idempotent,
      // so re-running on a later `rows` tick is safe.
      const body = rows.find(
        (x) => x.meta?.message_id === r.meta?.in_reply_to,
      )?.body;
      if (!body) continue;
      seededTitleRef.current.add(execSid);
      useChatStore.getState().seedOptimisticTitle(agentId, execSid, body);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rows, agentId]);
  // Cards that can no longer be answered, and the subset the human replied to
  // — driving the inline card's answered/closed receipt (§5.5.4). The chat
  // stream is a pure durable event log now; there is no live projection.
  const closed = useMemo(() => closedCardIds(rows), [rows]);
  const answered = useMemo(() => humanAnsweredCardIds(rows), [rows]);
  // Is the newest human message still waiting for an answer? Derived, never
  // stored — see `chatTurnState`. Suppressed only when a locate is KNOWN to
  // have parked the window mid-history (a window with newer pages has no
  // newest message to wait on); a window without position info is treated as
  // at the tail.
  const waiting =
    pendingTurn(rows, sending, closed) && !(win && win.lastPos < win.total - 1);
  // Name for the waiting line. The turn may have been routed to another agent,
  // but this surface talks to exactly one — naming it is what makes the line
  // read as "someone is working on it" instead of a generic spinner.
  const agentMeta = useAgentStore((s) => s.agents[agentId]?.meta);
  const pendingName = agentMeta?.display_name ?? agentMeta?.name ?? "";

  // Follow rule shared by every chat surface (AgentChatView / GroupChatPanel /
  // InboxPanel): stick to the newest row only while pinned, and never let a
  // refresh (poll tick, re-render with a fresh object) yank an upward-scrolling
  // reader back down. Deps key on `tail.length` — the appended newest page —
  // NOT the total rows, so `loadOlder` (which prepends into `older`) leaves the
  // trigger unchanged and does not jump to the bottom; the hook's prepend
  // anchoring (see `useStickToBottom`) holds the visible rows put as older
  // content appears above them — the viewport sets `overflow-anchor: none`
  // so the browser does not double-compensate (its native anchor latches onto
  // the top control and shoves the reading position down instead).
  // Bottom-positioning mechanism for the follow controller: the transcript is
  // virtualized, so a single `scrollTop = scrollHeight` write cannot land on a
  // bottom whose height is still an estimate (and may not be sized at all on
  // the session-switch commit). The list handle's scrollToBottom arms the
  // shell's measurement-convergence anchor instead.
  const positionAtBottom = useCallback(
    () => listRef.current?.scrollToBottom() ?? false,
    [],
  );
  const {
    ref: scrollRef,
    stickNow,
    unpin,
    atBottom,
  } = useStickToBottom(
    [win?.tail.length ?? 0, sessionId],
    sessionId,
    undefined,
    win?.older.length ?? 0,
    positionAtBottom,
  );

  // Merged container capture — a STABLE callback on purpose. An inline arrow is
  // a new identity every render, which makes React detach the ref (`null`) and
  // re-attach the same node: the follow controller would un-bind/re-bind on
  // every keystroke in the composer, and `OverlayScrollbar` would rebuild its
  // own ref setter each time.
  const setViewport = useCallback(
    (el: HTMLDivElement | null) => {
      containerRef.current = el;
      // `useStickToBottom` hands back a callback ref in production, but a
      // plain object ref is a valid React ref too — forward only the former so
      // a merged container capture never calls a non-function.
      if (typeof scrollRef === "function") scrollRef(el);
    },
    [scrollRef],
  );

  // Locate-and-flash for history jumps (ADR-097): declared AFTER the follow
  // hook so the center-scroll wins over follow-to-bottom on the jump commit.
  const highlightSeq = useLocateMessage(
    () => containerRef.current,
    pendingLocateSeq,
    () => clearLocate(sessionId),
    // Virtualization fallback: the row is in the window but not in the DOM.
    (seq) => {
      const idx = rowsRef.current.findIndex((r) => r.seq === seq);
      if (idx < 0) return false;
      return (
        listRef.current?.scrollToKey(`${sessionId}:${seq}`, "center") ?? false
      );
    },
    // A locate parks the reader mid-history on purpose: take them out of
    // the follow, or the persistent anchor rides the next commit back to
    // the tail (a downward jump carries no scroll-event signal to do it).
    () => unpin(),
  );

  // Stable identities: RowView is memoized, so every prop handed to it must
  // keep its reference across host re-renders (a keystroke in the composer
  // re-renders this host; it must not re-render N rows).
  const handleAnswer = useCallback(
    (card: ChatMessageRow, text: string, immediate: boolean) => {
      if (immediate && text) {
        stickNow();
        void sendText(sessionId, text, card.meta?.message_id);
        dismissExecCard(card);
        return;
      }
      setAnswerTarget(card);
      textareaRef.current?.focus();
    },
    [sessionId, stickNow, sendText],
  );

  const handleSend = () => {
    const text = input.trim();
    const attachmentIds = pendingAttachments.map((a) => a.id);
    if ((!text && attachmentIds.length === 0) || sending || uploading) return;
    const replyTo = answerTarget?.meta?.message_id;
    // A card answer continues the ORIGINAL turn deterministically (it does
    // not go through the Chat Router), so no seed is applied — carrying a
    // draft there would be meaningless. Plain messages carry the draft;
    // per ADR-099's 2026-10-28 revision the router-side seed is an initial
    // value for the session a routing CREATES and is never applied to one
    // that already exists.
    const sessionConfig = replyTo
      ? undefined
      : Object.keys(draft).length > 0
        ? draft
        : undefined;
    setInput("");
    const target = answerTarget;
    setAnswerTarget(null);
    setPendingAttachments([]);
    stickNow();
    void sendText(sessionId, text, replyTo, sessionConfig, attachmentIds);
    if (target && replyTo) dismissExecCard(target);
  };

  // Upload each chosen file into the session's `files/` dir and stack a
  // chip. Shared by the paperclip picker and the paste handler.
  const uploadFiles = async (files: File[]) => {
    if (files.length === 0) return;
    setUploading(true);
    try {
      for (const f of files) {
        const meta = await upload(sessionId, f);
        setPendingAttachments((prev) => [...prev, meta]);
      }
    } catch (err) {
      console.warn("[AgentChatView] attachment upload failed:", err);
    } finally {
      setUploading(false);
    }
  };

  const handleFilesChosen = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const files = Array.from(e.target.files ?? []);
    e.target.value = ""; // allow re-picking the same file
    await uploadFiles(files);
  };

  /** Insert `text` at the textarea's caret / selection range (same
   *  caret-aware shape the execution composer uses). */
  const insertTextAtCaret = (text: string) => {
    const ta = textareaRef.current;
    const current = ta?.value ?? input;
    if (!ta) {
      setInput(current + text);
      return;
    }
    const start = ta.selectionStart ?? current.length;
    const end = ta.selectionEnd ?? current.length;
    setInput(current.slice(0, start) + text + current.slice(end));
    const caret = start + text.length;
    requestAnimationFrame(() => {
      ta.focus();
      ta.setSelectionRange(caret, caret);
    });
  };

  // Ctrl+V parity with the execution composer: files, images and OS
  // screenshots paste straight into the attachment tray; plain text
  // inserts at the caret. Classification is shared (`pasteDetect.ts`) —
  // including its rule that only the Rust clipboard reader decides
  // "these are real files". preventDefault ALWAYS runs (every branch
  // inserts itself; letting the browser default paste fire alongside is
  // the known double-paste bug in Tauri WebViews).
  const handlePaste = async (e: React.ClipboardEvent<HTMLTextAreaElement>) => {
    const cd = e.clipboardData;
    if (!cd) return;
    e.preventDefault();
    const outcome = await detectPaste(cd);
    switch (outcome.kind) {
      case "files": {
        // OS file paths → bytes via the asset protocol (scope `**`) →
        // File objects into the same upload pipeline as the paperclip.
        const files: File[] = [];
        for (const p of outcome.paths) {
          try {
            const resp = await fetch(convertFileSrc(p));
            if (!resp.ok) throw new Error(`asset fetch → ${resp.status}`);
            const blob = await resp.blob();
            const name = p.replace(/^.*[\\/]/, "") || "attachment";
            files.push(
              new File([blob], name, {
                type: blob.type || "application/octet-stream",
              }),
            );
          } catch (err) {
            console.warn("[AgentChatView] clipboard file unreadable:", p, err);
          }
        }
        void uploadFiles(files);
        return;
      }
      case "image": {
        // The screenshot bytes are already in the WebView — wrap them as
        // a File and upload directly; no temp-file roundtrip (the
        // execution composer needs one only because its pipeline is
        // path-based).
        const type = outcome.blob.type || `image/${outcome.extension}`;
        const name = `screenshot-${Date.now()}.${outcome.extension}`;
        void uploadFiles([new File([outcome.blob], name, { type })]);
        return;
      }
      case "text":
        insertTextAtCaret(outcome.text);
        return;
      case "fileNames":
        // Name-only Files (no OS path, unreadable) — surface the names.
        insertTextAtCaret(outcome.names.join("\n"));
        return;
      case "nothing":
        return;
    }
  };

  const removeAttachment = (id: string) =>
    setPendingAttachments((prev) => prev.filter((a) => a.id !== id));

  // Jump to the execution session behind a specific agent message — for a
  // resolved turn (finished, failed or empty). openSession cold-loads the exec
  // tab; the pointer stays valid because the exec JSONL remains
  // in Agent home (§5.5.5).
  // A DELETED session would otherwise cold-open as an empty exec tab whose
  // every load fails into the retry banner, so probe existence first: a
  // definitive 404 is answered with a toast and no navigation.
  const jumpToExec = useCallback(
    async (execSid: string) => {
      if (!execSid) return;
      if (!(await sessionExists(agentId, execSid))) {
        showToast({ type: "info", message: t("chatView.execSessionGone") });
        return;
      }
      await useChatStore.getState().openSession(agentId, execSid);
      setMode(agentId, "exec");
    },
    [agentId, t, setMode],
  );
  // The void-returning stable handler handed to memoized rows. An inline
  // `(sid) => void jumpToExec(sid)` at the call site is a fresh identity every
  // render and would defeat `RowView`'s memo on every keystroke.
  const handleJumpToExec = useCallback(
    (execSid: string) => void jumpToExec(execSid),
    [jumpToExec],
  );

  const hasMoreOlder = win ? win.firstPos > 0 : false;
  // A locate can park the window mid-history; `!atTail` then opens the
  // downward path back to the newest page.
  const hasMoreNewer = win ? win.lastPos < win.total - 1 : false;
  const availableModels = useChatStore((s) => s.availableModels);
  const providerAccounts = useChatStore((s) => s.providerAccounts);

  // The chip reflects the draft when set, else the agent's current config.
  const currentModelId = draft.model ?? agentModel?.model ?? null;
  const currentProviderId = draft.provider ?? agentModel?.provider ?? null;
  // Reasoning affordance comes from the model inventory (the Chat surface
  // has no session to read `reasoningEffort` off). The label is resolved
  // through `defaultEffortOf` — the same three-level chain the execution
  // surface and the backend use (draft → model default → reasoning-capable
  // → "auto"), so a reasoning-capable model with no configured default
  // still shows the toggle here.
  const currentModelEntry =
    availableModels.find(
      (m) => m.name === currentModelId && m.provider === currentProviderId,
    ) ??
    availableModels.find((m) => m.name === currentModelId) ??
    null;
  const reasoningEffort =
    draft.reasoning_effort ?? defaultEffortOf(currentModelEntry);

  // Auto-pagination: reaching the top band triggers the next older page — no
  // click. Guarded by `hasMore` and the in-flight `loadingOlder` flag so it
  // can neither run past the oldest row nor overlap a request. The
  // prepend-anchoring in `useStickToBottom` then holds the reading position
  // as the new page lands above.
  const handleStreamScroll = (event: UIEvent<HTMLDivElement>) => {
    const el = event.currentTarget;
    if (
      hasMoreOlder &&
      !win?.loadingOlder &&
      el.scrollTop <= AUTO_LOAD_TOP_PX
    ) {
      void loadOlder(sessionId);
    }
    if (
      hasMoreNewer &&
      !win?.loadingNewer &&
      el.scrollHeight - el.scrollTop - el.clientHeight <= AUTO_LOAD_TOP_PX
    ) {
      void loadNewer(sessionId);
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {/* Message stream — a relative box so the "back to bottom" arrow floats
          over the list (and above the composer), matching the execute-session
          affordance. No "to top" arrow: a Chat session grows without bound, so
          jumping to the head would page through the whole history. */}
      <div className="relative flex min-h-0 flex-1 flex-col">
        <OverlayScrollbar
          ref={setViewport}
          className="py-2 [overflow-anchor:none]"
          onViewportScroll={handleStreamScroll}
        >
          {hasMoreOlder && (
            <div className="py-2 text-center text-10 text-text-tertiary">
              {win?.loadingOlder ? (
                <span className="inline-flex items-center justify-center gap-1.5">
                  <Loader2 className="h-3 w-3 animate-spin" />
                  {t("chatHistory.loadingEarlier")}
                </span>
              ) : (
                <span>{t("chatHistory.scrollForEarlier")}</span>
              )}
            </div>
          )}
          {rows.length === 0 && (
            <div className="py-10 text-center text-sm text-text-tertiary">
              {t("chatView.empty")}
            </div>
          )}
          {/* Virtualized transcript window (render-perf L2): only viewport rows
            + overscan are in the DOM, so a keystroke — or a doorbell refresh —
            costs a bounded number of row renders regardless of loaded pages.
            Prepend anchoring stays with `useStickToBottom` (scroll-container
            level; virtualization does not move it). The shell remounts on
            session change via `key` — the documented contract against a
            stale scroll offset (white pane). */}
          <VirtualRowList
            key={sessionId}
            ref={listRef}
            items={rows}
            keyOf={rowKeyOf}
            scrollContainerRef={containerRef}
            followBottom={atBottom}
            estimateSize={estimateRow}
            rowAttrs={(r) => ({ "data-seq": r.seq })}
            rowClassName={(r) =>
              cn(
                "scroll-mt-2 transition-colors",
                highlightSeq === r.seq && "bg-[var(--color-accent)]/10",
              )
            }
            renderItem={(r, i) => (
              <RowView
                row={r}
                agentId={agentId}
                chatSessionId={sessionId}
                showHeader={
                  // Header rule (ADR-100 D11 + the interaction-feel invariant):
                  // an agent bubble — the routing reaction included — wears the
                  // avatar+name header unless the row directly above already
                  // painted one. An empty terminal row is a CENTERED status
                  // line, not a bubble: it must not fold the header off the
                  // reply that follows it (that left the answer "孤零零" with no
                  // avatar). And the reaction row never folds the turn's real
                  // reply — they are minutes or hours apart, each is its own
                  // utterance.
                  (r.kind === "agent_message" || isTurnRoutedNotice(r)) &&
                  !(
                    rows[i - 1]?.kind === "agent_message" &&
                    !isEmptyTerminalReply(rows[i - 1]!)
                  )
                }
                onJumpToExec={handleJumpToExec}
                triggerBody={
                  r.meta?.in_reply_to
                    ? bodyById.get(r.meta.in_reply_to)
                    : undefined
                }
                replyQuote={
                  r.meta?.in_reply_to
                    ? cardQuoteById.get(r.meta.in_reply_to)
                    : undefined
                }
                closed={closed}
                answered={answered}
                onAnswer={handleAnswer}
              />
            )}
          />
          {/* The waiting line sits where the answer will land: one aggregated
            "the agent has your message" indicator, derived from the newest
            unanswered human row. It replaces the removed dispatch / round-open
            milestones, and disappears the moment a terminal row arrives. */}
          {waiting && (
            <ChatTurnStatusLine
              className="px-4"
              live
              text={
                pendingName
                  ? t("chatView.notice.pendingTurn", { name: pendingName })
                  : t("chatView.notice.pendingTurnGeneric")
              }
            />
          )}
          {hasMoreNewer && (
            <div className="py-2 text-center text-10 text-text-tertiary">
              {win?.loadingNewer ? (
                <span className="inline-flex items-center justify-center gap-1.5">
                  <Loader2 className="h-3 w-3 animate-spin" />
                  {t("chatHistory.loadingNewer")}
                </span>
              ) : (
                <button
                  type="button"
                  className="inline-flex items-center gap-1 hover:text-text-secondary"
                  onClick={() => void loadNewer(sessionId)}
                >
                  <ChevronDown size={12} />
                  {t("chatHistory.jumpToLatest")}
                </button>
              )}
            </div>
          )}
        </OverlayScrollbar>
        {/* Back-to-bottom arrow — visible only while the reader has scrolled
            up off the tail (and there is content). Same treatment as the
            execute-session button: default-reduced opacity, full on hover. */}
        {atBottom === false && rows.length > 0 && (
          <button
            onClick={stickNow}
            className="absolute bottom-3 right-4 z-10 rounded-full bg-zinc-100 dark:bg-zinc-700 border border-zinc-200 dark:border-zinc-600 shadow-md p-1.5 opacity-40 hover:opacity-100 focus-visible:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600 transition-all animate-in fade-in zoom-in"
            aria-label={t("chatPanel.ariaLabelScrollToBottom")}
          >
            <ChevronsDown className="h-4 w-4 text-text-tertiary" />
          </button>
        )}
      </div>

      {/* Composer — same chrome as the execution view (plan 5.4). */}
      <div className="mx-6 mb-3 rounded-xl border border-chat-input-border bg-chat-input-bg">
        <input
          ref={fileInputRef}
          type="file"
          multiple
          className="hidden"
          onChange={handleFilesChosen}
        />
        {answerTarget && (
          <div className="flex items-center gap-1 px-3 pt-2">
            <span className="inline-flex items-center gap-1 rounded bg-[var(--color-accent)]/10 px-1.5 py-0.5 text-xs font-medium border border-border-outer text-text-tertiary">
              {t("chatView.replyingTo")}{" "}
              {answerTarget.meta?.interaction?.question?.slice(0, 30) || ""}
              <button
                type="button"
                onClick={() => setAnswerTarget(null)}
                className="ml-0.5 inline-flex items-center justify-center rounded-sm hover:bg-[var(--color-accent)]/15"
                aria-label={t("chatView.cancelReply")}
              >
                ×
              </button>
            </span>
          </div>
        )}
        {/* Workspace references picked with "Add to Chat" in the right-hand
            panel — paths the Agent reads live, not uploaded copies. The
            uploaded bytes above are a separate list with a separate channel. */}
        <AttachedContextChips surface="chat" />
        {pendingAttachments.length > 0 && (
          <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
            {pendingAttachments.map((a) => (
              <ChatAttachmentChip
                key={a.id}
                chatSessionId={sessionId}
                attachment={a}
                onRemove={() => removeAttachment(a.id)}
                removeLabel={t("chatPanel.removeAttachment")}
              />
            ))}
          </div>
        )}
        <textarea
          ref={textareaRef}
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder={t("chatPanel.inputMessage")}
          disabled={sending}
          className="w-full resize-none border-0 bg-transparent p-3 pb-2 outline-none placeholder:text-text-disabled  disabled:cursor-not-allowed disabled:opacity-50 max-h-48 overflow-y-auto min-h-[5rem]"
          style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
          onKeyDown={(e) => {
            if (e.key !== "Enter" || e.shiftKey) return;
            const elapsed = Date.now() - lastCompositionEndRef.current;
            if (elapsed < 300) return; // IME confirmation — do not send
            e.preventDefault();
            handleSend();
          }}
          onCompositionEnd={() => {
            lastCompositionEndRef.current = Date.now();
          }}
          onPaste={handlePaste}
        />
        {/* Bottom toolbar — same layout as the execution composer. Model /
            reasoning / workspace are WRITABLE: a pick becomes a per-message
            draft that seeds the execution session on send (design 27 §4.5).
            Skills are intentionally absent — not session-seeded, no chat-message
            channel, so nothing to show here (hidden, not disabled). */}
        <div className="@container/tb flex items-center justify-between gap-2 px-3 pb-2 min-w-[264px]">
          <div className="flex items-center gap-1 min-w-0 overflow-visible">
            {availableModels.length > 0 && agentModel && (
              <ModelMenu
                wrapperRef={undefined}
                textHidden={false}
                models={availableModels}
                accountsByProvider={providerAccounts}
                currentModel={currentModelId}
                currentProvider={currentProviderId}
                currentAccountId={null}
                onSelect={(m, p) =>
                  // Whole-draft replace: picking a model keeps the rest of
                  // the draft but resets the effort (the previous level may
                  // not exist on the new model). Persisted to the session row.
                  void saveDraft(agentId, {
                    ...draft,
                    model: m,
                    provider: p,
                    reasoning_effort: undefined,
                  })
                }
                btnId="model"
              />
            )}
            {reasoningEffort != null && (
              <ReasoningEffortMenu
                textHidden={false}
                effort={reasoningEffort}
                onChange={(e) =>
                  void saveDraft(agentId, { ...draft, reasoning_effort: e })
                }
                btnId="effort"
              />
            )}
            <div className="min-w-0">
              <WorkspaceSelector
                textHidden={false}
                value={draft.workspace}
                onChange={(id) =>
                  void saveDraft(agentId, { ...draft, workspace: id })
                }
              />
            </div>
          </div>
          <div
            data-toolbar-cluster="right"
            className="flex shrink-0 items-center gap-1"
          >
            <Tooltip content={t("chatPanel.attach")} variant="plain">
              <button
                className={cn(toolbarIconButton, uploading && "opacity-50")}
                onClick={() => fileInputRef.current?.click()}
                disabled={uploading}
                aria-label={t("chatPanel.attach")}
              >
                <Paperclip size={16} />
              </button>
            </Tooltip>
            <Tooltip content={t("chatPanel.sendMessage")} variant="plain">
              <button
                className={cn(toolbarIconButton, sending && "opacity-50")}
                onClick={handleSend}
                disabled={sending}
                aria-label={t("chatPanel.sendMessage")}
              >
                <Send size={16} />
              </button>
            </Tooltip>
          </div>
        </div>
      </div>
    </div>
  );
}
