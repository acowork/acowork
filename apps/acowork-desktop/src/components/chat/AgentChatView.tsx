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
// - No skill menu here at all. Skills are auto-loaded by the model, not
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
// ADR-106: the chat stream holds conversation plus ONE bodyless reference row per
// round of work (`execution_round`). What the run did, whether it is still going,
// and what it is parked on are not chat rows — they are read off the round's
// projection at render time and drawn inside that row's block
// (`ChatExecutionBlock`), which also carries the drill into the execution view
// (`chatStore.openSession(agentId, execSid)` + mode flip, plan 5.3).

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
  AtSign,
  Quote,
} from "lucide-react";
import { formatBubbleTime } from "../../lib/formatTime";
import { cn } from "../../lib/utils";
import { composerChip, toolbarIconButton } from "../../lib/ui-styles";
import { useStickToBottom } from "../../lib/stickToBottom";
import { Tooltip } from "../common/Tooltip";
import { UserAvatar } from "../common/UserAvatar";
import { AgentAvatar } from "../common/AgentAvatar";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { ChatAttachmentChip } from "./ChatAttachmentChip";
import { openChatAttachment } from "../../lib/openChatAttachment";
import { ExecAttachmentLabels } from "./ExecAttachmentLabels";
import { bubbleMetaAnchor } from "./bubbleMetaAnchor";
import { detectPaste } from "./pasteDetect";
import { useChatComposerBridge } from "../../hooks/useChatComposerBridge";
import { ChatMarkdown } from "./ChatMarkdown";
import {
  VirtualRowList,
  type VirtualRowListHandle,
} from "../common/VirtualRowList";
import { ModelMenu, ReasoningEffortMenu } from "./ChatPanel";
import { useToolbarCollapse } from "../common/useToolbarCollapse";
import { defaultEffortOf } from "../../lib/modelCapabilities";
import { WorkspaceSelector } from "../workspace/WorkspaceSelector";
import { ChatNoticeEventRow } from "./ChatNoticeEventRow";
import { ChatUsageIcon } from "./ChatUsageIcon";
import { AttachedContextChips } from "./AttachedContextChips";
import { ChatExecutionBlock } from "./ChatExecutionBlock";
import { type TranscriptItem } from "./chatTranscriptGroups";
import { useRevealPendingAsk } from "./useRevealPendingAsk";
import { useTranscriptLayout } from "./useTranscriptLayout";
import {
  ChatAddressPicker,
  MAX_PICK_REFS,
  type AddressPick,
} from "./ChatAddressPicker";
import {
  answerWithChoice,
  directCardAddress,
  mayAnswerCard,
  type CardAnswerHandler,
  type CardChoice,
} from "../../lib/chatCardAnswer";
import { ChatTurnStatusLine } from "./ChatTurnStatusLine";
import { ChatTurnHeader, type TurnIdentity } from "./ChatTurnHeader";
import { turnInflight, resolveExpectedResponders } from "../../lib/chatTurnInflight";
import { useLocateMessage } from "../../lib/useLocateMessage";
import {
  fetchCardAnswer,
  useChatSessionStore,
  type ChatMessageRow,
  type ChatSessionConfig,
  type ChatAttachmentMeta,
  type ChatAddress,
} from "../../stores/chatSessionStore";
import { DeliveryAlertLine } from "./DeliveryAlertLine";
import { useChatStore } from "../../stores/chatStore";
import type { WorkspaceRef } from "../../lib/types";
import { useAgentStore } from "../../stores/agentStore";
import { sessionExists } from "../../lib/session-control";
import { showToast } from "../common/ToastProvider";
import { useAuthStore } from "../../stores/authStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { getGatewayUrl } from "../../lib/config";

// ── Rows ───────────────────────────────────────────────────────────────

interface RowProps {
  row: ChatMessageRow;
  agentId: string;
  /** Chat session the row belongs to — attachment thumbnails are addressed
   *  through it (the `files/` dir is session-scoped, §4.2.1), and the block
   *  read is participation-checked against it (ADR-106 D10). */
  chatSessionId: string;
  showHeader: boolean;
  /** Drill into the execution session behind a round. The block names the
   *  instance that hosts it, so the handler takes it — on this surface it is
   *  always `agentId`, but the row must not have to know that. */
  onJumpToExec: (agentInstanceId: string, execSid: string) => void;
  /** Address the composer at one round (ADR-106 D6 level 1): what its
   *  `pause` card's continue button, and any follow-up on a finished round,
   *  ride. `quote` is the question the human is answering. */
  onReplyToRound: (rowId: string, quote: string) => void;
  /** A pause card's Continue, delivered as a plain "Continue" message to the
   *  execution session's own write endpoint — a control signal for that
   *  session's loop, NOT a chat utterance: the chat send path, the Chat
   *  Router and the transcript never see it. */
  onContinueRound: (instanceId: string, execSessionId: string) => void;
  /** Attach one execution session to this draft as a reference (design 27
   *  §5.5.5) — the block's `@` action, riding the same address state the `@`
   *  picker commits to. */
  onReferenceRun?: (payload: {
    instanceId: string;
    execSessionId: string;
    name: string;
  }) => void;
  /** Answer an addressing card (D6 levels 3 and 5) by re-delivering the
   *  utterance it asks about, now addressed. The parent hands it only to the
   *  person the card asked (D10) — `undefined` for anyone else. */
  onChooseCard?: CardAnswerHandler;
  /** ADR-106 revision 2026-11-04: click a reference source chip (or the `@`
   *  picker's "去续投") to locate that run's block in the transcript. A
   *  navigation, not an address — it never changes what a message carries. */
  onLocateRef?: (sessionId: string) => void;
  /** Which execution session the picker just asked to continue: the matching
   *  block reveals + focuses its in-place reply box. */
  focusBlockSessionId?: string;
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
  onChooseCard,
  onLocateRef,
}: RowProps) {
  const { t } = useTranslation();
  const agent = useAgentStore((s) => s.agents[agentId]?.meta);
  const profile = useUserProfileStore((s) => s.profile);
  const agentName = agent?.display_name ?? agent?.name ?? "";

  if (row.kind === "execution_round") {
    // A reference row that names no round (a missing coordinate) has no block to
    // fold into, so it keeps its own explicit hole (ADR-106 D3). Every row that
    // DOES name a round is drawn by `ChatExecutionBlock`, one block per execution
    // session, and reaches that block as a `run` item — not as a row.
    return (
      <ChatTurnStatusLine
        className="px-4"
        text={t("chatView.block.notFound")}
      />
    );
  }

  if (row.kind === "system_notice") {
    // Delivery receipts, the budget receipt, and the addressing questions
    // (ADR-106 D6): quiet centered lines, never agent speech. The routing
    // milestones this branch used to synthesize into a bubble are gone.
    const noticeType = row.meta?.notice_type;
    const choose =
      (noticeType === "session_choice" ||
        noticeType === "confirm_continue") &&
      onChooseCard
        ? (choice: CardChoice) => onChooseCard(row, choice)
        : undefined;
    return (
      <ChatNoticeEventRow row={row} className="px-4" onChoose={choose} />
    );
  }

  if (row.kind === "agent_message") {
    // Only the Chat Router's own answer is written under this kind now
    // (ADR-106 D12) — a plain-text reply to something that opened no run, so
    // there is no round to drill into and no outcome to report. Older
    // transcripts may carry a terminal row with no body; the placeholder keeps
    // those from collapsing into an empty bubble.
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
                      onClick={() => void openChatAttachment(chatSessionId, a)}
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
              {(row.body || !row.attachments?.length) && (
                <div className="inline-block max-w-full rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5  select-text break-words">
                  {row.body && <ChatMarkdown content={row.body} />}
                  {/* A bodyless row with no attachments has nothing to say; the
                    placeholder keeps it from collapsing into an empty bubble. */}
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

  // user_message (and anything else) — the human side, right-aligned. A
  // message that continues a round (`in_reply_to` = the reference row) needs no
  // quote above it: the block it answers is the row directly beneath.
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
              reaches the agent bubble's line length and follows the setting.
              The cross-axis anchor comes from `bubbleMetaAnchor`: every meta
              unit below (the answer quote, the bubble, the hover timestamp)
              hugs the human's side of the row. It used to be `items-start`,
              which stranded this side's timestamp under the bubble's LEFT
              edge — near the middle for a short message, far left for a long
              one — while the group panel already hugged the outer edge. */}
          <div
            className={`group mt-[6px] max-w-[var(--content-max-width)] flex flex-col ${bubbleMetaAnchor("own")}`}
          >
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
                    onClick={() => void openChatAttachment(chatSessionId, a)}
                  />
                ))}
              </div>
            )}
            {/* ADR-100 D12: a turn sent with attached items paints them as
                labels above the text — which is also what keeps a
                file-with-no-words turn from collapsing to a blank row. Since
                D12 Rev. 2026-11 a workspace arm carries its path, so it opens
                the file in the fileTab exactly like the execution session's
                chip does; upload arms and folders stay decorative (their bytes
                belong to the run / fileTab has no folder view). */}
            <ExecAttachmentLabels
              labels={row.meta?.exec_attachments}
              agentId={agentId}
            />
            {row.body && (
              <div className="max-w-full rounded-md rounded-br-sm bg-chat-user text-chat-user-text select-text">
                <div className="px-4 py-2.5 whitespace-pre-wrap break-words">
                  {row.body}
                </div>
              </div>
            )}
            {/* ADR-106 revision 2026-11-04: the `@session` material attached to
                this utterance, read off the row's pinned `meta.refs` (no new
                read). Source chips, right-aligned under the bubble — the human
                sees what they quoted and one click jumps to that run's block. */}
            {row.meta?.refs && row.meta.refs.length > 0 && (
              <div className="mb-0.5 mt-1 flex flex-wrap justify-end gap-1 self-end">
                {row.meta.refs.map((r) => (
                  <button
                    key={`${r.agent_instance_id}|${r.session_id}|${r.round_id}`}
                    type="button"
                    onClick={() => onLocateRef?.(r.session_id)}
                    className="inline-flex max-w-[12rem] items-center gap-1 rounded bg-chat-bubble px-1.5 py-0.5 text-10 text-text-tertiary hover:text-text-secondary"
                    title={r.session_id}
                  >
                    <Quote className="h-3 w-3 shrink-0" />
                    <span className="truncate">{r.session_id}</span>
                  </button>
                ))}
              </div>
            )}
            {/* A send that failed, while it is still news (ADR-105 D5 rev.
                2026-11-06): served live on this row, never a transcript row, and
                gone on its own clock. */}
            {row.delivery?.alert ? (
              <DeliveryAlertLine alert={row.delivery.alert} align="end" className="mt-0.5" />
            ) : null}
            <span className="mt-1 text-10 text-text-tertiary  opacity-0 transition-opacity group-hover:opacity-100">
              {formatBubbleTime(row.ts * 1000)}
            </span>
          </div>
        </div>
        <UserAvatar
          displayName={profile.displayName}
          avatarUrl={profile.backendAvatarUrl ?? null}
          builtinAvatarId={profile.backendBuiltinAvatarId ?? undefined}
          size={40}
          className="shrink-0 mt-1"
        />
      </div>
    </div>
  );
});

/**
 * One item of the folded transcript: either a session's block or a row.
 *
 * The layout layer (`chatTranscriptGroups`) decides the fold; this is where it
 * becomes a component. Memoized for the same reason as `RowView` — a keystroke in
 * the composer re-renders the host, and the window must not re-paint (plan L1).
 * `item` references come from the memoized layout, so they hold their identity
 * while the window is unchanged.
 */
interface ItemProps extends Omit<RowProps, "row"> {
  item: TranscriptItem;
  /** Something follows this block in the transcript — `ExploreBlock`'s
   *  `hasFollowUpReply`, the one condition that auto-collapses a settled run. */
  hasFollowUp: boolean;
}

const ItemView = memo(function ItemView({
  item,
  hasFollowUp,
  ...rowProps
}: ItemProps) {
  if (item.kind === "run") {
    return (
      <ChatExecutionBlock
        block={item}
        chatSessionId={rowProps.chatSessionId}
        showHeader={rowProps.showHeader}
        hasFollowUp={hasFollowUp}
        onJumpToExec={rowProps.onJumpToExec}
        onReplyToRound={rowProps.onReplyToRound}
        onContinueRound={rowProps.onContinueRound}
        onReferenceRun={rowProps.onReferenceRun}
        onChooseCard={rowProps.onChooseCard}
        focusInput={
          !!rowProps.focusBlockSessionId &&
          item.execSessionId === rowProps.focusBlockSessionId
        }
      />
    );
  }
  return <RowView {...rowProps} row={item.row} />;
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
/** Stable empty list for the pending-refs selector — a fresh `[]` per render
 *  would re-trigger every Zustand subscriber. */
const EMPTY_REFS: WorkspaceRef[] = [];

/** How close to the top (px) trips auto-pagination. The "load earlier" status
 *  band sits here; scrolling into it fires the next older page. */
const AUTO_LOAD_TOP_PX = 48;

/** A pending continuation of one round: the reference row's id (what the send
 *  files as `in_reply_to`, ADR-106 D6 level 1) and the question the human is
 *  answering, shown in the composer chip. A card no longer has a chat row of
 *  its own, so the block is what hands this over. */
interface ReplyTarget {
  id: string;
  quote: string;
}

export function AgentChatView({ agentId, sessionId }: AgentChatViewProps) {
  const { t } = useTranslation();
  const win = useChatSessionStore((s) => s.windows[sessionId]);
  const sending = !!useChatSessionStore((s) => s.sending[sessionId]);
  // The turn this device sent and has not seen an outcome for — the store
  // claimed it from the send's own response (ADR-106 D9). `undefined` once the
  // transcript answers it, once the deadline passes, and always after a reload.
  const inflightTurn = useChatSessionStore((s) => s.inflight[sessionId]);
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
    settleTurn,
  } = useChatSessionStore.getState();

  // The agent's own name, for the line that says it is working on this turn.
  const agentMeta = useAgentStore((s) => s.agents[agentId]?.meta);
  const agentName = agentMeta?.display_name ?? agentMeta?.name ?? "";

  const [input, setInput] = useState("");
  const [replyTarget, setReplyTarget] = useState<ReplyTarget | null>(null);
  // The second level of the `@` (ADR-106 D7). On this surface the first level is
  // degenerate — one agent — so the human names only the task. `addressStage` is
  // where the open list hangs from (`null` = closed), `address` is "what was
  // chosen for this draft".
  const [addressStage, setAddressStage] = useState<"button" | "typed" | null>(
    null,
  );
  const [address, setAddress] = useState<AddressPick | null>(null);
  // ADR-106 revision 2026-11-04: the run whose block the `@` picker's "去续投"
  // just located — that block reveals + focuses its in-place reply box. Cleared
  // on the next commit so a stale focus never re-fires.
  const [focusBlockSessionId, setFocusBlockSessionId] = useState<string | null>(
    null,
  );
  // The card asks the person who spoke, and in local mode there is exactly one
  // person but no id to compare against (see `mayAnswerCard`).
  const me = useAuthStore((s) => s.account?.user_id ?? "");
  // Composer attachments — uploaded into the Chat session's `files/` dir
  // (§4.2.1) before send; the send carries their ids so the durable row and
  // the execution turn both see them.
  const [pendingAttachments, setPendingAttachments] = useState<
    ChatAttachmentMeta[]
  >([]);
  const [uploading, setUploading] = useState(false);
  // Workspace references added with "Add to Chat" (rendered by
  // `AttachedContextChips`). They are content: a human who points at a file and
  // types nothing has said something. The send gate used to count only typed
  // text and uploaded blobs, so the reference-only send — the exact gesture
  // this whole path exists for — could not leave the composer.
  const pendingRefs = useChatSessionStore((s) => s.attachedContext[sessionId] ?? EMPTY_REFS);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  // The `@` toggle, exempt from the address picker's outside-click close so
  // clicking it again collapses the list instead of close-then-reopen.
  const addressBtnRef = useRef<HTMLButtonElement>(null);
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
  // Re-run on `gatewayStatus`: this is one of the two conditions that decide
  // whether the model button exists at all (the other is `availableModels`),
  // and a Gateway HTTP call issued before the connection is up fails. Keyed on
  // the status so an early failure retries once the Gateway answers, instead
  // of leaving the toolbar model-less until the surface remounts.
  const gatewayConnected = useGatewayStore((s) => s.status === "connected");
  useEffect(() => {
    if (!gatewayConnected) return;
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
          // Re-check after the await: with `gatewayConnected` in the deps,
          // a flap (connected → error → connected) can overlap two runs for
          // the same agentId, and the slow older response must not overwrite
          // the newer one's result.
          if (!cancelled) setAgentModel(data);
        }
      } catch {
        /* toolbar simply shows no model chip */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [agentId, gatewayConnected]);

  const rows = useMemo(() => (win ? [...win.older, ...win.tail] : []), [win]);
  // D9's claim read against the transcript: this device's send is owed its
  // first visible outcome until a row answers its id (the round's reference
  // row, the router's reply, a receipt). `sending` alone only covers the POST —
  // measured 150 ms — which is what left the 0.1 s … 81 s window blank.
  const turnLive = turnInflight(rows, inflightTurn);
  // ONE state drives the line and the lock: a wait line with an unlocked
  // composer invites the second send this Chat has no queue to absorb, and a
  // locked composer with no line reads as a broken button.
  const composerLocked = sending || turnLive;
  // The turn header's face (ADR-106 revision 2026-11-08): the claim's expected
  // responder, resolved through the registry the bubbles already use. A 1:1
  // chat's claim names exactly its peer — a delivery-decision input, not a
  // transcript inference. Anything else (no responders on a legacy claim, a
  // peer the registry lost) leaves this empty and the surface keeps the
  // centered line: a header without a resolvable name is not an upgrade.
  const turnHeaderIdentities = useMemo<TurnIdentity[]>(() => {
    const ids = inflightTurn?.responders ?? [];
    if (ids.length !== 1 || ids[0] !== agentId || !agentMeta) return [];
    return [
      {
        id: agentId,
        displayName: agentName,
        role: agentMeta.role ?? null,
        avatarUrl: agentMeta.avatar ?? null,
        builtinAvatarId: agentMeta.builtin_avatar ?? null,
        version: agentMeta.version ?? null,
      },
    ];
  }, [inflightTurn, agentId, agentMeta, agentName]);
  // Hand the claim back as soon as a row answers it, so nothing keeps
  // asserting a turn the transcript has already settled.
  useEffect(() => {
    if (!inflightTurn || turnInflight(rows, inflightTurn)) return;
    settleTurn(sessionId, inflightTurn.messageId);
  }, [rows, inflightTurn, sessionId, settleTurn]);
  // Virtualization adapters (render-perf L2): keys, estimates, per-item
  // attributes and the seq→item index shared with the group panel.
  const {
    items,
    rowsRef,
    keyOfSeqRef,
    virtualKeyOf,
    virtualEstimate,
    virtualAttrs,
    seqsByItem,
  } = useTranscriptLayout(rows, sessionId);
  const isBubble = (item: TranscriptItem | undefined) =>
    item?.kind === "row" && item.row.kind === "agent_message";
  // Optimistic exec-session title seed (ADR-099). A Router-created execute
  // session has no list row and often no open tab, so it would read "Untitled"
  // until the backend's generated title arrives over MQTT. The round's
  // reference row is the first durable place its session id appears, and its
  // `in_reply_to` names the human message that opened it — so the provisional
  // title is seeded off that body. `seedOptimisticTitle` no-ops once any title
  // exists, so the authoritative (possibly LLM-refined) push always wins
  // afterwards. Guarded per execSid so it fires at most once.
  const seededTitleRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    for (const r of rows) {
      if (
        r.kind !== "execution_round" ||
        !r.meta?.agent_session_id ||
        !r.meta?.in_reply_to
      ) {
        continue;
      }
      const execSid = r.meta.agent_session_id;
      if (seededTitleRef.current.has(execSid)) continue;
      // Only pin the execSid once the trigger's body is available — the
      // reference row can land a window before its trigger row is readable, and
      // marking it early would permanently skip the seed. `seedOptimisticTitle`
      // is itself idempotent, so re-running on a later `rows` tick is safe.
      const body = rows.find(
        (x) => x.meta?.message_id === r.meta?.in_reply_to,
      )?.body;
      if (!body) continue;
      seededTitleRef.current.add(execSid);
      useChatStore.getState().seedOptimisticTitle(agentId, execSid, body);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rows, agentId]);

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
  // A question the router asks about a run is drawn at that run's block, which
  // stays where the exchange began — this is what brings the reader there.
  // Declared AFTER the follow hook, the same rule `useLocateMessage` follows
  // below: the card lands as a new row, and follow-to-bottom would otherwise
  // write the viewport back to the tail in the same commit and hide it.
  useRevealPendingAsk(items, keyOfSeqRef.current, listRef);
  const highlightSeq = useLocateMessage(
    () => containerRef.current,
    pendingLocateSeq,
    () => clearLocate(sessionId),
    // Virtualization fallback: the row is in the window but not in the DOM. A
    // folded row is not in the transcript as its own item either, so its seq
    // resolves through the layout's `keyOfSeq` to the block that holds it.
    (seq) => {
      const key = keyOfSeqRef.current.get(seq);
      if (!key) return false;
      return listRef.current?.scrollToKey(key, "center") ?? false;
    },
    // A locate parks the reader mid-history on purpose: take them out of
    // the follow, or the persistent anchor rides the next commit back to
    // the tail (a downward jump carries no scroll-event signal to do it).
    () => unpin(),
  );

  // Stable identities: RowView is memoized, so every prop handed to it must
  // keep its reference across host re-renders (a keystroke in the composer
  // re-renders this host; it must not re-render N rows).
  // A round's `pause` card is answered with the human's own words, which make
  // the NEXT turn of that round (ADR-106 D6 level 1) — so this addresses the
  // composer at the reference row instead of sending anything on its own.
  const handleReplyToRound = useCallback((rowId: string, quote: string) => {
    setReplyTarget({ id: rowId, quote });
    textareaRef.current?.focus();
  }, []);

  // A pause card's Continue is a control signal for the execution session's
  // loop, not a chat utterance: it goes straight to that session's own write
  // endpoint — the same request the execution composer makes when a human
  // types "Continue". The chat send path never sees it, so no user bubble
  // lands in the transcript and the Chat Router cannot hijack it; the runtime
  // opens the next turn in place, the iteration counter resets, and the
  // round's reference row lands in this same block as Round N.
  // A pause card's Continue is the SAME action as the execution surface's
  // Continue banner: resume the paused turn in place (`POST /sessions/{sid}/continue`
  // → `ContinueExecution`), so the iteration counter resets and the loop carries
  // on inside the same round. Deliberately NOT a chat message: a "Continue"
  // utterance would open a new turn with a fabricated user row, leave the paused
  // round parked forever, and hand the text to the Chat Router — none of which
  // the human asked for. One pause state, one resume path, two surfaces.
  const handleContinueRound = useCallback(
    (instanceId: string, execSessionId: string) => {
      if (!execSessionId) return;
      void useChatStore
        .getState()
        .continueExecution(instanceId, execSessionId)
        .catch((err) => {
          console.error("[chat] pause-card continue failed:", err);
          showToast({ type: "error", message: t("chatView.continueFailed") });
        });
    },
    [showToast, t],
  );

  // ADR-106 revision 2026-11-04: "去续投" / a source-chip click locate the run's
  // block. The block is the newest `execution_round` row whose session matches;
  // its seq resolves through the layout's `keyOfSeq` to the virtual item, which
  // the list scrolls to. A run with no block in the loaded window answers with a
  // toast and no auto-paging (v1) — the navigation is best-effort, the reference
  // it was pointing at is already on the row regardless.
  const handleLocateRef = useCallback(
    (sessionId: string) => {
      setAddressStage(null);
      const rows = rowsRef.current;
      let seq: number | null = null;
      for (const r of rows) {
        if (
          r.kind === "execution_round" &&
          r.meta?.agent_session_id === sessionId
        ) {
          seq = r.seq;
        }
      }
      if (seq === null) {
        showToast({ type: "info", message: t("chatView.address.blockNotLoaded") });
        return;
      }
      const key = keyOfSeqRef.current.get(seq);
      if (key) listRef.current?.scrollToKey(key, "center");
      setFocusBlockSessionId(sessionId);
    },
    [t],
  );

  // Answering an addressing card (ADR-106 D6): re-send the human's OWN words
  // with the address they just chose, keyed by the original row's id so the
  // transcript keeps one message instead of narrating the decision twice.
  // The words being re-sent come from the service, not from `rowsRef`: the row
  // the card asks about may be far outside the loaded window, and looking it up
  // here is what made an old card's buttons dead (design 31 §6). The click
  // (which session, or new work) is still ours — it is what the human chose.
  //
  // Answers `true` when the click became a delivery — the card then holds its
  // busy state until the projection closes it — and `false` when it did not, so
  // the card re-opens instead of leaving a decision that never moved.
  const answerCard = useCallback(
    async (card: ChatMessageRow, choice: CardChoice): Promise<boolean> => {
      const cardId = card.meta?.message_id;
      if (!cardId) return false;
      const lookup = await fetchCardAnswer(sessionId, cardId);
      if (!lookup.ok) {
        // `already_answered` needs no message: a round settled the question and
        // the projection's next read closes the card. `gone` is the honest
        // statement that this card can no longer be re-delivered.
        if (lookup.reason === "gone") {
          showToast({ type: "error", message: t("chatView.cardAnswerGone") });
        }
        return false;
      }
      const answer = answerWithChoice(lookup.facts, choice);
      if (!answer) return false;
      void sendText(
        sessionId,
        answer.text,
        undefined,
        undefined,
        answer.attachmentIds,
        directCardAddress(answer),
        resolveExpectedResponders({ peer: agentId }),
      );
      return true;
    },
    [sendText, sessionId, t, agentId],
  );

  const handleSend = () => {
    const text = input.trim();
    const attachmentIds = pendingAttachments.map((a) => a.id);
    // Three proofs a turn carries something: words, an uploaded file, a
    // workspace reference. The Gateway's own gate accepts all three, so a
    // composer that refused the third could not send what the backend would
    // have delivered.
    if (
      (!text && attachmentIds.length === 0 && pendingRefs.length === 0) ||
      composerLocked ||
      uploading
    ) {
      return;
    }
    const replyTo = replyTarget?.id;
    // A reply that addresses one round continues THAT run deterministically
    // (it never goes through the Chat Router, ADR-106 D6 level 1), so no seed
    // is applied — carrying a draft there would be meaningless. Plain messages
    // carry the draft; per ADR-099's 2026-10-28 revision the router-side seed
    // is an initial value for the session a routing CREATES and is never
    // applied to one that already exists.
    const sessionConfig = replyTo
      ? undefined
      : Object.keys(draft).length > 0
        ? draft
        : undefined;
    // A level-1 reply already names its session, so the address the human picked
    // for a plain message must not ride that send — the two are contradictory
    // instructions and the refusal belongs to the backend, not to a guess here.
    const chosen = replyTo ? null : address;
    // ADR-106 revision 2026-11-04: a typed `@session` is now material, never a
    // delivery — the composer carries `refs` (the picked coordinates) and the
    // Gateway pins each round at send time. "new task" is still `forceNew`.
    const wire: ChatAddress | undefined = chosen?.refs?.length
      ? { refs: chosen.refs }
      : chosen?.forceNew
        ? { forceNew: true }
        : undefined;
    setInput("");
    setReplyTarget(null);
    setFocusBlockSessionId(null);
    setAddress(null);
    setPendingAttachments([]);
    stickNow();
    void sendText(
      sessionId,
      text,
      replyTo,
      sessionConfig,
      attachmentIds,
      wire,
      resolveExpectedResponders({ peer: agentId }),
    );
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

  // ── Workspace panel → composer bridge ─────────────────────────────
  // The preview in the right-hand workspace panel fires
  // `insertIntoChatComposer` / `attachImageToChatComposer` /
  // `attachFileToChatComposer` (see
  // `lib/chatComposerBridge.ts`) when the user sends a coordinate point, a
  // cropped region, or a whole PDF. Only one composer is mounted at a time, so
  // no session id is needed; `useChatComposerBridge` handles the
  // latest-closure plumbing.
  useChatComposerBridge((request) => {
    if (request.kind === "text") {
      insertTextAtCaret(request.text);
      return;
    }
    // Same path the paste handler's `image` branch takes: the bytes are
    // already in the WebView, so they upload straight through the Chat
    // session's `files/` route, no temp-file roundtrip. Both file kinds take
    // it unchanged — the runtime reads an attachment's role off the NAME the
    // sender gave it (a `…-region-…png` crop, a `….pdf` document), so there is
    // nothing for this surface to branch on.
    void uploadFiles([request.file]);
  });

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
      await useChatStore
        .getState()
        .openSession(agentId, execSid, { prepend: true });
      setMode(agentId, "exec");
    },
    [agentId, t, setMode],
  );
  // The void-returning stable handler handed to memoized rows. An inline arrow
  // at the call site is a fresh identity every render and would defeat
  // `RowView`'s memo on every keystroke. The block names the instance hosting
  // its round; on this surface that is always this agent, so the argument is
  // dropped — but the row must not have to know that.
  const handleBlockJump = useCallback(
    (_agentInstanceId: string, execSid: string) => void jumpToExec(execSid),
    [jumpToExec],
  );

  /**
   * The block's `@` action (design 27 §5.5.5): attach THAT run to this draft.
   *
   * It commits through the same `address` state the `@` picker's "引用" commits
   * to, so this surface has one kind of reference — a removable chip carrying the
   * coordinates the Gateway pins at send time — instead of a second, parallel
   * way to say "use this run".
   *
   * Two rules it inherits rather than invents:
   * - a level-1 reply already names its session, and the send path refuses to
   *   carry both (see `send`). So the reply target is cleared first: the human
   *   just asked for THIS reference, and the chip row must not be hidden behind
   *   a reply chip they had forgotten about.
   * - the fifth reference is the server's ceiling (`MAX_PICK_REFS`). Hitting it
   *   says so instead of silently dropping the click.
   */
  const handleReferenceRun = useCallback(
    ({
      instanceId,
      execSessionId,
      name,
    }: {
      instanceId: string;
      execSessionId: string;
      name: string;
    }) => {
      setReplyTarget(null);
      setAddress((prev) => {
        const key = `${instanceId}|${execSessionId}`;
        const refs = prev?.refs ?? [];
        // Re-clicking a run already attached is a no-op, not a second chip for
        // the same session.
        if (refs.some((r) => `${r.agentInstanceId}|${r.sessionId}` === key)) {
          return prev;
        }
        if (refs.length >= MAX_PICK_REFS) {
          showToast({
            type: "info",
            message: t("chatView.address.refLimit", { max: MAX_PICK_REFS }),
          });
          return prev;
        }
        return {
          refs: [...refs, { agentInstanceId: instanceId, sessionId: execSessionId }],
          labels: { ...(prev?.labels ?? {}), [key]: name },
          forceNew: prev?.forceNew,
        };
      });
      // Land the human where the reference is going to be used.
      textareaRef.current?.focus();
    },
    [t],
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
  // The depth levels the current model declares (models.dev
  // `reasoning_options`). Undefined = unknown (custom model) -> the generic
  // level set.
  const reasoningLevels = currentModelEntry?.reasoning_efforts;
  // Toolbar responsive collapse — same engine the execution-session
  // composer uses (shared `useToolbarCollapse`); folds model → reasoning →
  // workspace labels left-to-right when the pane narrows.
  const { setToolbarRef, textHidden } = useToolbarCollapse();

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

  // One picker, two anchor points: the toolbar button opens it under its own
  // icon, the typed `@` opens it above the composer. `anchorRef` is the button
  // in both cases, so a click on the toggle still closes what it opened.
  const addressPicker = (
    <ChatAddressPicker
      instanceId={agentId}
      /** The pair's own chat: the listing is narrowed to what this conversation
       *  may address, so the id is part of the request, not a label. */
      chatSessionId={sessionId}
      anchorRef={addressBtnRef}
      onPick={(pick) => {
        setAddress(pick.refs?.length || pick.forceNew ? pick : null);
        setAddressStage(null);
        textareaRef.current?.focus();
      }}
      onLocate={(sid) => handleLocateRef(sid)}
      onClose={() => {
        setAddressStage(null);
        textareaRef.current?.focus();
      }}
    />
  );

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
          {items.length === 0 && (
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
            items={items}
            keyOf={virtualKeyOf}
            scrollContainerRef={containerRef}
            followBottom={atBottom}
            estimateSize={virtualEstimate}
            rowAttrs={virtualAttrs}
            rowClassName={(item) =>
              cn(
                "scroll-mt-2 transition-colors",
                highlightSeq !== null &&
                  seqsByItem.get(virtualKeyOf(item))?.has(highlightSeq) &&
                  "bg-[var(--color-accent)]/10",
              )
            }
            renderItem={(item, i) => (
              <ItemView
                item={item}
                hasFollowUp={i < items.length - 1}
                agentId={agentId}
                chatSessionId={sessionId}
                showHeader={
                  // Adjacent bubbles from the same speaker fold the header; a
                  // block above is not a bubble, so the reply under it still
                  // wears its avatar — and a block always starts a new turn.
                  item.kind === "run" ||
                  (item.row.kind === "agent_message" &&
                    !isBubble(items[i - 1]))
                }
                onJumpToExec={handleBlockJump}
                onReplyToRound={handleReplyToRound}
                onContinueRound={handleContinueRound}
                onReferenceRun={handleReferenceRun}
                onChooseCard={
                  item.kind === "row" && mayAnswerCard(item.row, me)
                    ? answerCard
                    : undefined
                }
                onLocateRef={handleLocateRef}
                focusBlockSessionId={focusBlockSessionId ?? undefined}
              />
            )}
          />
          {/* D9, first person only: this claims that a send THIS device made
              has not been answered yet. It starts from the id that send's
              response carried and ends at the first row answering it — the
              round's reference row, the router's own reply, a receipt — or on a
              deadline, so it cannot outlive the send it belongs to and never
              fakes liveness for a node that is gone. A transcript SHAPE ("the
              newest human row has no answer") is still not evidence: after a
              reload there is nothing to claim. The same flag locks the composer
              below — a Chat has no queue, so a second send would race a router
              still deciding the first. The round's own liveness rides inside its
              block, off the `pending` projection read.
              ADR-106 revision (2026-11-08, the turn header): while the claim is
              live and its expected responder resolves (a 1:1 chat's is its
              peer — a delivery-decision input, not a transcript inference), the
              wait wears the peer's avatar + name and hands over to the block's
              own header at the same anchor when the first answering row lands.
              The POST-in-flight moment and an unresolvable identity keep the
              centered line: no face without a name to put on it. */}
          {composerLocked && (
            turnLive && turnHeaderIdentities.length > 0 ? (
              <ChatTurnHeader
                identities={turnHeaderIdentities}
                text={t("chatView.notice.pendingTurnGeneric")}
              />
            ) : (
              <ChatTurnStatusLine
                className="px-4"
                live
                text={
                  agentName
                    ? t("chatView.notice.pendingTurn", { name: agentName })
                    : t("chatView.notice.pendingTurnGeneric")
                }
              />
            )
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
        {atBottom === false && items.length > 0 && (
          <button
            onClick={stickNow}
            className="absolute bottom-3 right-4 z-10 rounded-full bg-zinc-100 dark:bg-zinc-700 border border-zinc-200 dark:border-zinc-600 shadow-md p-1.5 opacity-40 hover:opacity-100 focus-visible:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600 transition-all animate-in fade-in zoom-in"
            aria-label={t("chatPanel.ariaLabelScrollToBottom")}
          >
            <ChevronsDown className="h-4 w-4 text-text-tertiary" />
          </button>
        )}
      </div>

      {/* Composer — same chrome as the execution view (plan 5.4). `relative`
          so the address picker floats above the card like the group's `@`
          popup does. */}
      <div className="relative mx-6 mb-3 rounded-xl border border-chat-input-border bg-chat-input-bg">
        <input
          ref={fileInputRef}
          type="file"
          multiple
          className="hidden"
          onChange={handleFilesChosen}
        />
        {replyTarget && (
          <div className="flex items-center gap-1 px-3 pt-2">
            <span className={composerChip}>
              {t("chatView.replyingTo")}{" "}
              {replyTarget.quote.slice(0, 30)}
              <button
                type="button"
                onClick={() => setReplyTarget(null)}
                className="ml-0.5 inline-flex items-center justify-center rounded-sm hover:bg-[var(--color-accent)]/15"
                aria-label={t("chatView.cancelReply")}
              >
                ×
              </button>
            </span>
          </div>
        )}
        {/* The task this message is addressed to (ADR-106 D7). Shown because an
            address the surface swallowed would be indistinguishable from one
            never given — and it is the one fact no model may re-decide. */}
        {address && !replyTarget && (
          <div className="flex flex-wrap items-center gap-1 px-3 pt-2">
            {address.forceNew && (
              <span className={composerChip}>
                {t("chatView.block.newTask")}
                <button
                  type="button"
                  onClick={() =>
                    setAddress((a) => {
                      if (!a) return null;
                      const next = { ...a, forceNew: false };
                      return next.refs?.length ? next : null;
                    })
                  }
                  className="ml-0.5 inline-flex items-center justify-center rounded-sm hover:bg-[var(--color-accent)]/15"
                  aria-label={t("chatView.address.clear")}
                >
                  ×
                </button>
              </span>
            )}
            {/* ADR-106 revision 2026-11-04: each attached reference is its own
                removable chip — swallowing one the human picked would make it
                indistinguishable from never picked, and it is a fact no model
                may re-decide. */}
            {(address.refs ?? []).map((r) => {
              const key = `${r.agentInstanceId}|${r.sessionId}`;
              return (
                <span key={key} className={composerChip}>
                  <Quote className="h-3 w-3 shrink-0" />
                  {address.labels?.[key] || r.sessionId}
                  <button
                    type="button"
                    onClick={() =>
                      setAddress((a) => {
                        if (!a) return null;
                        const refs = (a.refs ?? []).filter(
                          (x) => `${x.agentInstanceId}|${x.sessionId}` !== key,
                        );
                        const labels = { ...a.labels };
                        delete labels[key];
                        if (refs.length === 0 && !a.forceNew) return null;
                        return { refs, labels, forceNew: a.forceNew };
                      })
                    }
                    className="ml-0.5 inline-flex items-center justify-center rounded-sm hover:bg-[var(--color-accent)]/15"
                    aria-label={t("chatView.address.clear")}
                  >
                    ×
                  </button>
                </span>
              );
            })}
          </div>
        )}
        {/* Second level of the `@` — the agent's own routable tasks, opened by
            typing `@` in the composer: it belongs above the text it was typed
            into. The button's own popup anchors under that button instead. No
            `mousedown` preventDefault on this shell: the picker holds a search
            box, and cancelling a mousedown is precisely what cancels the focus
            it would hand the input. Every pick path refocuses the composer. */}
        {addressStage === "typed" && (
          <div className="absolute bottom-full left-3 z-20 mb-2">
            {addressPicker}
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
          placeholder={t("chatView.address.composerHint")}
          disabled={composerLocked}
          className="w-full resize-none border-0 bg-transparent p-3 pb-2 outline-none placeholder:text-text-disabled  disabled:cursor-not-allowed disabled:opacity-50 max-h-48 overflow-y-auto min-h-[5rem]"
          style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
          onKeyDown={(e) => {
            /** Typing `@` at the start of a word opens the same task list the
             *  toolbar button opens (ADR-106 D7) — one gesture on both chats.
             *  The character is NOT inserted: here the address rides the chip
             *  above the composer, not the message text, so a leftover `@`
             *  would be noise the model has to read. A mid-word `@` (an email)
             *  stays literal, the rule the group's `detectMention` uses. */
            if (
              e.key === "@" &&
              !e.ctrlKey &&
              !e.metaKey &&
              !e.altKey &&
              !e.nativeEvent.isComposing
            ) {
              const caret = e.currentTarget.selectionStart ?? input.length;
              const prev = input[caret - 1];
              if (!prev || /\s/.test(prev)) {
                e.preventDefault();
                setAddressStage("typed");
                return;
              }
            }
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
        <div ref={setToolbarRef} className="@container/tb flex items-center justify-between gap-2 px-3 pb-2 min-w-[264px]">
          <div className="flex items-center gap-1 min-w-0 overflow-visible">
            {availableModels.length > 0 && agentModel && (
              <ModelMenu
                wrapperRef={undefined}
                textHidden={textHidden.model}
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
                textHidden={textHidden.effort}
                effort={reasoningEffort}
                onChange={(e) =>
                  void saveDraft(agentId, { ...draft, reasoning_effort: e })
                }
                btnId="effort"
                supportedLevels={reasoningLevels}
              />
            )}
            <div className="min-w-0">
              <WorkspaceSelector
                textHidden={textHidden.ws}
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
            {/* The button opens the list above itself, right-aligned to its own
                icon — a popup that appeared across the composer from the click
                would not read as belonging to it. */}
            <div className="relative">
              <Tooltip content={t("chatView.address.pick")} variant="plain">
                <button
                  ref={addressBtnRef}
                  className={cn(
                    toolbarIconButton,
                    addressStage && "bg-nav-item-hover",
                  )}
                  onClick={() => setAddressStage((v) => (v ? null : "button"))}
                  aria-label={t("chatView.address.pick")}
                  aria-expanded={addressStage !== null}
                >
                  <AtSign size={16} />
                </button>
              </Tooltip>
              {addressStage === "button" && (
                <div className="absolute bottom-full right-0 z-20 mb-2">
                  {addressPicker}
                </div>
              )}
            </div>
            {/* Per-chat ledger read (plan §4.6 phase two): the pair's
                router spend lives on this one agent. */}
            <ChatUsageIcon
              agentIds={[agentId]}
              chatSessionId={sessionId}
              editableAgentId={agentId}
            />
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
            <Tooltip
              content={
                turnLive
                  ? t("chatView.notice.turnInflightTooltip")
                  : t("chatPanel.sendMessage")
              }
              variant="plain"
            >
              <button
                className={cn(toolbarIconButton, composerLocked && "opacity-50")}
                onClick={handleSend}
                disabled={composerLocked}
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
