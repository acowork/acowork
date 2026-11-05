/**
 * GroupChatPanel — the middle-pane surface for a project group chat
 * (design 27 §4.4 / §5.3). Mounted by AppLayout when
 * `useProjectGroupStore.activeGroupId` is set.
 *
 * It is deliberately a thin twin of `AgentChatView`: same rows, same
 * bubble visuals, same execution blocks — but multi-party. The message
 * window lives in `chatSessionStore.windows[sessionId]` (the doorbell
 * keeps it fresh); this component only renders and sends.
 *
 * Send channels (§5.3, ADR-106 D6/D7):
 * - plain text  -> `/api/intents/mentions` with no targets: the Gateway
 *   delivers to the group's default responder, or archives the row;
 * - `@Agent`    -> the same entry with `targets` (one row + one turn each);
 * - `@Agent@session` / "new task" -> the same entry plus `target_sessions`
 *   / `force_new`, the second-level address;
 * - reply to a block -> `/api/intents/chat-message` with `in_reply_to`, which
 *   the Gateway resolves into that round's agent and session (level 1).
 * A card is answered inside its block, against the execution session (D8) —
 * it is not a chat row and never travels through this composer.
 *
 * `ponytail:` ceilings vs the 1:1 surface — no model/workspace seed
 * toolbar (group turns are seeded by the mentioned agent's defaults),
 * no scroll-anchored pagination (a "load earlier" button instead). The
 * composer matches the 1:1 chat surface's attachment abilities: Paperclip
 * upload + Ctrl+V paste into the session's `files/` dir, ids forwarded on
 * the mention/archive send. Upgrade path: fold in the seed chips once the
 * mentions channel carries `session_config`.
 */

import {
  useCallback,
  useEffect,
  useMemo,
  memo,
  useRef,
  useState,
  Fragment,
  type UIEvent,
  type MouseEvent as ReactMouseEvent,
} from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import {
  Send,
  Paperclip,
  Loader2,
  ChevronDown,
  ChevronsDown,
  ArrowRight,
  AtSign,
  Quote,
} from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import type { Components } from "react-markdown";
import { formatBubbleTime } from "../../lib/formatTime";
import { cn } from "../../lib/utils";
import { composerChip, listRowSelected, toolbarIconButton } from "../../lib/ui-styles";
import { getGatewayUrl } from "../../lib/config";
import { fetchDirectory } from "../../lib/auth-api";
import type { DirectoryUser } from "../../lib/types";
import { useStickToBottom } from "../../lib/stickToBottom";
import { Tooltip } from "../common/Tooltip";
import { AgentAvatar } from "../common/AgentAvatar";
import { UserAvatar } from "../common/UserAvatar";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { ChatAttachmentChip } from "./ChatAttachmentChip";
import { ChatUsageIcon } from "./ChatUsageIcon";
import { openChatAttachment } from "../../lib/openChatAttachment";
import { ExecAttachmentLabels } from "./ExecAttachmentLabels";
import { bubbleMetaAnchor } from "./bubbleMetaAnchor";
import { ChatNoticeEventRow } from "./ChatNoticeEventRow";
import {
  indexByMessageId,
  noticeAnchorOf,
  type NoticeAnchor,
} from "../../lib/chatNoticeAnchor";
import {
  answerWithChoice,
  groupCardAddress,
  mayAnswerCard,
  type CardChoice,
} from "../../lib/chatCardAnswer";
import { ChatTurnStatusLine } from "./ChatTurnStatusLine";
import { turnInflight } from "../../lib/chatTurnInflight";
import { ChatExecutionBlock } from "./ChatExecutionBlock";
import type { TranscriptItem } from "./chatTranscriptGroups";
import { useTranscriptLayout } from "./useTranscriptLayout";
import { ChatAddressPicker, type AddressPick } from "./ChatAddressPicker";
import { detectPaste } from "./pasteDetect";
import { ChatMarkdown } from "./ChatMarkdown";
import {
  VirtualRowList,
  type VirtualRowListHandle,
} from "../common/VirtualRowList";
import { useAuthStore } from "../../stores/authStore";
import { useAgentStore } from "../../stores/agentStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import {
  fetchCardAnswer,
  useChatSessionStore,
  type ChatMessageRow,
  type ChatAttachmentMeta,
  type ChatAddress,
  type ChatRefPick,
} from "../../stores/chatSessionStore";
import { useChatStore } from "../../stores/chatStore";
import { sessionExists } from "../../lib/session-control";
import { showToast } from "../common/ToastProvider";
import { ChatHistoryDialog } from "./ChatHistoryDialog";
import { OutlineChatHistoryIcon } from "../common/ChatHistoryIcon";
import { useLocateMessage } from "../../lib/useLocateMessage";
import {
  ContextMenu,
  useContextMenu,
  type ContextMenuItem,
} from "../common/ContextMenu";

// Same markdown shell as AgentChatView's bubbles (kept local on purpose:
// extracting a shared component would touch two surfaces for one class
// list — do that when a third chat surface appears).
/**
 * Group-surface block typography only. The behavioural overrides (fenced code,
 * links, table scroll) and the `prose` shell now live in `ChatMarkdown`, shared
 * with the 1:1 chat and the execution bubbles — see the drift note there.
 *
 * Two group behaviours were intentionally dropped when the copies merged, both
 * toward the house style rather than away from it: fenced code is dedented like
 * every other surface, and links take `.prose a` (globals.css) instead of a
 * hard-coded `text-[var(--color-accent)] underline`.
 */
const chatBlockComponents: Components = {
  p: ({ children }: { children?: React.ReactNode }) => (
    <p className="mb-2 text-sm leading-relaxed last:mb-0">{children}</p>
  ),
  code: ({ children }: { children?: React.ReactNode }) => (
    <code className="rounded bg-black/10 px-1 py-0.5 text-xs dark:bg-white/10">
      {children}
    </code>
  ),
  ul: ({ children }: { children?: React.ReactNode }) => (
    <ul className="mb-2 list-disc pl-5">{children}</ul>
  ),
  ol: ({ children }: { children?: React.ReactNode }) => (
    <ol className="mb-2 list-decimal pl-5">{children}</ol>
  ),
  li: ({ children }: { children?: React.ReactNode }) => (
    <li className="text-sm leading-relaxed">{children}</li>
  ),
};

// ── Participant naming ────────────────────────────────────────────────

/** Resolve a participant id to a display name + kind. Agents come from
 *  the local agent registry; humans from the account directory (fetched
 *  once per panel mount — it is small and bounded). Unknown id (agent on
 *  another machine, removed account) falls back to a short id. */
function useParticipantNamer(directory: DirectoryUser[]) {
  const agentsMap = useAgentStore((s) => s.agents);
  // The logged-in account is deliberately absent from `list_directory` (the
  // contact picker excludes the caller), so resolve "me" from the live profile
  // or every message addressed to me paints its raw uuid.
  const me = useAuthStore((s) => s.account?.user_id ?? "");
  const profile = useUserProfileStore((s) => s.profile);
  const dirById = useMemo(() => {
    const m = new Map<string, DirectoryUser>();
    for (const u of directory) m.set(u.user_id, u);
    return m;
  }, [directory]);
  // Name **and** avatar come from the same lookup: the account directory for
  // people, the agent registry for agents. Resolving the name but leaving the
  // avatar blank used to fall back to `userProfileStore` — i.e. the logged-in
  // account's icon on every other participant's bubble (t-9764c5fa).
  return useCallback(
    (id: string): ParticipantWho => {
      const agent = agentsMap[id]?.meta;
      if (agent) {
        return {
          name: agent.display_name ?? agent.name ?? id,
          isAgent: true,
          avatarUrl: agent.avatar ?? null,
          builtinAvatarId: agent.builtin_avatar ?? null,
        };
      }
      if (me && id === me) {
        return {
          name: profile.displayName || id.slice(0, 8),
          isAgent: false,
          avatarUrl: profile.backendAvatarUrl ?? null,
          builtinAvatarId: profile.backendBuiltinAvatarId ?? null,
        };
      }
      const user = dirById.get(id);
      if (user)
        return {
          name: user.display_name || user.username,
          isAgent: false,
          avatarUrl: user.avatar ?? null,
          builtinAvatarId: user.builtin_avatar ?? null,
        };
      return {
        name: id.slice(0, 8),
        isAgent: false,
        avatarUrl: null,
        builtinAvatarId: null,
      };
    },
    [agentsMap, dirById, me, profile],
  );
}

/** What a participant id resolves to on this surface. */
interface ParticipantWho {
  name: string;
  isAgent: boolean;
  avatarUrl: string | null;
  builtinAvatarId: string | null;
}

// ── Row renderer ──────────────────────────────────────────────────────

/** What the composer is currently addressed to (ADR-106 D6 level 1): one
 *  round's reference row. `id` is sent as `in_reply_to`, which the Gateway
 *  resolves into the round's agent and execution session — so "continue that
 *  run" never reaches a model as a guess. `quote` is the one-line reminder
 *  shown above the input. */
interface ReplyTarget {
  id: string;
  quote: string;
}

interface RowProps {
  row: ChatMessageRow;
  me: string;
  /** Chat session the row belongs to — attachment thumbnails are addressed
   *  through it (the `files/` dir is session-scoped, §4.2.1), and the
   *  execution block's read is participation-checked against it (ADR-106 D10). */
  chatSessionId: string;
  nameOf: (id: string) => ParticipantWho;
  /** Drill into the exec session that owns a round — jumps to that agent's 1:1
   *  page and opens the exec tab (mirrors AgentChatView.jumpToExec). */
  onJumpToExec: (agentId: string, execSid: string) => void;
  /** Address the composer to one round (ADR-106 D6 level 1) — how a `pause`
   *  card is continued and how a finished round gets a follow-up. */
  onReplyToRound: (rowId: string, quote: string) => void;
  /** Admin viewing a group they never joined: runs are shown, never
   *  answered — posting into someone else's project stays forbidden. */
  readOnly?: boolean;
  /** Sender of the message this row answers (`in_reply_to`'s `from`) — the
   *  counterparty of a reply whose own `to` is empty.
   *  Resolved by the parent through its message_id → from index. */
  triggerFrom?: string;
  /** Right-click on a participant's avatar / name → the panel's "@name"
   *  menu. Undefined when the panel has no mention target for that row. */
  onMentionContextMenu?: (e: ReactMouseEvent, id: string) => void;
  /** Column a channel receipt belongs in (a failed send is about ONE
   *  participant's message). Absent for non-notices and for a receipt whose
   *  subject is outside the loaded window. */
  noticeAnchor?: NoticeAnchor;
  /** Answer an addressing card (ADR-106 D6 levels 3 and 5) by re-delivering the
   *  utterance it asks about, now addressed. Undefined for a read-only viewer
   *  (D10): deciding where someone else's words go is not a bystander's call. */
  onChooseCard?: (card: ChatMessageRow, choice: CardChoice) => void;
  /** ADR-106 revision 2026-11-04: click a reference source chip (or the `@`
   *  picker's "去续投") to locate that run's block in the transcript. A
   *  navigation, not an address — it never changes what a message carries. */
  onLocateRef?: (sessionId: string) => void;
  /** Which execution session the picker just asked to continue: the matching
   *  block reveals + focuses its in-place reply box. */
  focusBlockSessionId?: string;
}

/**
 * The recipients a message is addressed to, resolved from the durable row's
 * own coordinates — the "who is this for" the group surface otherwise leaves
 * to guesswork. A `user_message` names its target in `to`, except an @-fan-out
 * broadcast which leaves `to` empty and lists the woken agents in
 * `meta.mentions`; an `agent_message` names the initiator it answers in `to`.
 * Empty when the row carries no address (a human-to-human row, an old
 * broadcast) — then no chip is painted. The whole capsule is a display
 * affordance; the transient-execution-session binding lives in the data, the
 * chip just shows it.
 */
function rowToTargets(
  row: ChatMessageRow,
  triggerFrom?: string,
): string[] {
  if (row.to) return [row.to];
  if (row.kind === "user_message") return row.meta?.mentions ?? [];
  // An execution-session echo row answers one specific message: its
  // counterparty is that trigger's sender, resolved from `in_reply_to`
  // (`to` is empty on this write path — the address lives on the row it
  // answers).
  if (triggerFrom) return [triggerFrom];
  return [];
}

/**
 * A "who is this addressed to" pill — the counterparty of the execution
 * session this message was bound to. Rendered uniformly on both user and
 * agent messages so the group reads as one exchange per row, not a wall of
 * unattributed bubbles. The display name comes from the row's caller (the
 * same `nameOf` the avatar uses); an unknown id falls back to its short form.
 */
function ToChip({ name }: { name: string }) {
  const { t } = useTranslation();
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-10 font-medium",
        "border-border-outer bg-[var(--color-accent)]/10 text-text-secondary",
      )}
    >
      <span className="font-semibold">{t("groupChat.toChip")}</span>
      <ArrowRight className="h-3 w-3 shrink-0" aria-hidden />
      <span className="max-w-[14rem] truncate">{name}</span>
    </span>
  );
}

/**
 * One transcript row. Memoized for the same reason as AgentChatView's RowView:
 * the composer re-renders this host on every keystroke, and the host re-runs
 * `rows.map` — without memo that re-parses every markdown body in the window
 * (chat-transcript render-perf plan L1). Every prop is reference-stable while
 * the window is unchanged: `nameOf` is a `useCallback` and the handlers are
 * host-level callbacks.
 */
const GroupRowView = memo(function GroupRowView({
  row,
  me,
  chatSessionId,
  nameOf,
  triggerFrom,
  onMentionContextMenu,
  onChooseCard,
  onLocateRef,
  noticeAnchor,
}: RowProps) {
  const { t } = useTranslation();
  const profile = useUserProfileStore((s) => s.profile);
  const agentsMap = useAgentStore((s) => s.agents);

  if (row.kind === "execution_round") {
    // A reference row that names no round (a missing coordinate) addresses
    // nothing, so it has no block to fold into and keeps its own explicit hole
    // (ADR-106 D3). Every row that DOES name a round is drawn by the session's
    // `ChatExecutionBlock` and reaches it as a `run` item — not as a row.
    return (
      <ChatTurnStatusLine
        className="px-4"
        text={t("chatView.block.notFound")}
      />
    );
  }

  if (row.kind === "system_notice") {
    // A notice is a fact about the channel, not an utterance, so it never wears
    // an avatar. `name` is used by one receipt: the `delivery_failed` row of
    // ADR-105 says which agent a message never reached, resolved from the roster
    // like every other participant reference here (`from` = `to` = that agent on
    // the Gateway's write). Every other receipt ignores the prop.
    const failedTarget =
      row.meta?.notice_type === "delivery_failed"
        ? nameOf(row.to || row.from).name
        : undefined;
    const noticeType = row.meta?.notice_type;
    // The card asks the person who spoke — `to` is them — so a viewer who is
    // not that person gets the question without the answer (D10).
    const choose =
      (noticeType === "session_choice" ||
        noticeType === "confirm_continue") &&
      onChooseCard &&
      mayAnswerCard(row, me)
        ? (choice: CardChoice) => onChooseCard(row, choice)
        : undefined;
    return (
      <ChatNoticeEventRow
        row={row}
        className="px-4"
        name={failedTarget}
        anchor={noticeAnchor}
        onChoose={choose}
      />
    );
  }

  const own = row.kind === "user_message" && row.from === me;
  const who: ParticipantWho = own
    ? { name: profile.displayName, isAgent: false, avatarUrl: null, builtinAvatarId: null }
    : nameOf(row.from);
  // Right-click on a participant's avatar / name → the panel's "@name" menu.
  // Everyone but me: @-ing an agent wakes it, @-ing a person is a plain
  // mention — but the picker lists both (§5.3.1), so the menu must too.
  const openMentionMenu =
    row.from !== me
      ? (e: ReactMouseEvent) => onMentionContextMenu?.(e, row.from)
      : undefined;
  const agentMeta = who.isAgent ? agentsMap[row.from]?.meta : undefined;
  // Who this row is addressed to (see `rowToTargets`). Resolved outside the
  // kind branches so the hook order stays unconditional; empty means the row
  // carries no address and no chip is painted.
  const toTargets = rowToTargets(row, triggerFrom);

  // Avatar is a 40px side column (left for others, right for own) — the same
  // primitive the user↔user inbox (`MessageRow`) and user↔agent thread
  // (`AgentChatView`) use, so all three chat surfaces read as one layout:
  // name above the bubble, bubble beside the avatar, timestamp below the
  // bubble revealed on hover.
  const avatar = who.isAgent ? (
    <span onContextMenu={openMentionMenu} className="shrink-0">
      <AgentAvatar
        agentId={row.from}
        displayName={who.name}
        avatarUrl={agentMeta?.avatar}
        version={agentMeta?.version}
        builtinAvatarId={agentMeta?.builtin_avatar ?? null}
        size={40}
        className="mt-1 shrink-0"
      />
    </span>
  ) : own ? (
    <UserAvatar
      displayName={profile.displayName}
      avatarUrl={profile.backendAvatarUrl ?? null}
      builtinAvatarId={profile.backendBuiltinAvatarId ?? undefined}
      size={40}
      className="mt-1 shrink-0"
    />
  ) : (
    <span onContextMenu={openMentionMenu} className="shrink-0">
      <UserAvatar
        displayName={who.name}
        avatarUrl={who.avatarUrl}
        builtinAvatarId={who.builtinAvatarId}
        size={40}
        className="mt-1 shrink-0"
      />
    </span>
  );

  return (
    <div
      className={cn(
        "mb-2 flex items-start gap-2 px-4",
        own ? "justify-end" : "justify-start",
      )}
    >
      {!own && avatar}
      <div
        className={cn(
          "flex min-w-0 flex-1 flex-col",
          own ? "items-end" : "items-start",
        )}
      >
        {who.name && (
          <div className="mt-[2px] flex flex-col">
            <span
              className="text-xs text-text-tertiary"
              onContextMenu={openMentionMenu}
            >
              {who.name}
            </span>
            {/* Agent role line — mirrors the user↔agent thread
                (`AgentChatView`), which pairs the name with the agent's
                role so a group reader can tell which specialist spoke. */}
            {who.isAgent && agentMeta?.role && (
              <span className="text-10 leading-tight text-text-tertiary">
                {agentMeta.role}
              </span>
            )}
          </div>
        )}
        <div
          className={cn(
            "group mt-[6px] flex max-w-[var(--content-max-width)] flex-col",
            // Same rule as the 1:1 thread: the meta block hugs the bubble's own
            // outer edge (see `bubbleMetaAnchor`). This surface already did it;
            // it now reads from the one home of the rule so the two chat
            // surfaces cannot drift apart again.
            bubbleMetaAnchor(own ? "own" : "peer"),
          )}
        >
          {/* Header chips: who the message is addressed to. One TO capsule per
              target — an @-fan-out row wakes several agents and shows them all.
              The run's own coordinates (task id, drill-down) belong to the
              execution block now (ADR-106 D3/D13), not to the utterance. */}
          {toTargets.length > 0 && (
            <div
              className={cn(
                "mb-1 flex flex-wrap items-center gap-1.5",
                own ? "justify-end" : "justify-start",
              )}
            >
              {toTargets.map((id) => (
                <ToChip key={id} name={nameOf(id).name} />
              ))}
            </div>
          )}
          {/* Attachment chips above the bubble, on the speaker's side — same
              grammar as the 1:1 chat surface. An attachments-only message
              paints chips alone and the empty-bubble placeholder is suppressed
              so the row does not collapse to a blank bubble. */}
          {row.attachments && row.attachments.length > 0 && (
            <div
              className={cn(
                "mb-1 flex flex-wrap gap-1.5",
                own ? "justify-end" : "justify-start",
              )}
            >
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
          {/* ADR-100 D12: a PM task session is bound to this group's chat, so a
              turn composed in its execution view arrives here as a `user_message`
              whose attached items are labels only — the file lives in the run,
              not in this chat session, so the pill is deliberately inert. They
              also count as content, which is what keeps a file-with-no-words
              head row from painting the empty-turn placeholder. */}
          <ExecAttachmentLabels
            labels={row.meta?.exec_attachments}
            align={own ? "end" : "start"}
          />
          {/* The group column is an `items-start` flex column, so the bubble is
             shrink-to-fit too: without the clamp, content that cannot be
             squeezed (a mermaid SVG's natural width, an unwrapped table, one
             long token) widened the bubble past the row's `max-w-[var(
             --content-max-width)]` and spilled under the next panel. The clamp
             hands that overflow to the inner scroll containers. */}
          {(row.body ||
            !(
              row.attachments?.length || row.meta?.exec_attachments?.length
            )) && (
            <div
              className={cn(
                "max-w-full rounded-md px-4 py-2.5 leading-relaxed break-words select-text",
                own
                  ? "rounded-br-sm bg-chat-user text-chat-user-text"
                  : "rounded-bl-sm bg-chat-bubble text-text-primary",
              )}
            >
              {/* Human input is literal text, markdown is agent speech — the
                same renderer split the user↔agent thread (`AgentChatView`)
                and the execution surface (`UserWithAttachmentsBubble`) apply
                to their user rows. Routing `user_message` through
                ChatMarkdown here was the drift: the prose shell paints
                `color: var(--tw-prose-body)` (a page-level near-black) on
                its own wrapper, which beats the white `text-chat-user-text`
                the accent-filled own-bubble inherits — the "user bubble text
                turned black" defect. It also made `**bold**` render bold in
                a group chat while reading as literal text in the 1:1 chat. */}
              {row.body ? (
                row.kind === "user_message" ? (
                  <div className="whitespace-pre-wrap break-words">
                    {row.body}
                  </div>
                ) : (
                  <ChatMarkdown
                    content={row.body}
                    components={chatBlockComponents}
                  />
                )
              ) : (
                <span className="text-xs text-text-tertiary italic">
                  {t("groupChat.emptyTurn")}
                </span>
              )}
            </div>
          )}
          {/* ADR-106 revision 2026-11-04: the `@session` material attached to
              this utterance, read off the row's pinned `meta.refs` (no new
              read). Source chips on the speaker's side under the bubble — one
              click jumps to that run's block. */}
          {row.meta?.refs && row.meta.refs.length > 0 && (
            <div
              className={cn(
                "mb-0.5 mt-1 flex flex-wrap gap-1",
                own ? "justify-end self-end" : "justify-start self-start",
              )}
            >
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
          <div className="mt-1 flex items-center gap-2">
            <span className="text-10 text-text-tertiary opacity-0 transition-opacity group-hover:opacity-100">
              {formatBubbleTime(row.ts * 1000)}
            </span>
          </div>
        </div>
      </div>
      {own && avatar}
    </div>
  );
});

interface GroupItemProps extends Omit<RowProps, "row"> {
  item: TranscriptItem;
  /** Something follows this item in the transcript — `ExploreBlock`'s
   *  `hasFollowUpReply`, the one condition that auto-collapses a settled run. */
  hasFollowUp: boolean;
}

/**
 * One item of the folded transcript: an execution session's block, or a row.
 *
 * This is where the session-level fold (ADR-106's presentation contract) becomes
 * a component on the group surface; `GroupRowView` stays the per-row renderer it
 * always was. The memo boundary sits on the item the list keys on, so a composer
 * keystroke re-paints nothing (render-perf plan L1) — the same reason `RowView`
 * is memoized.
 *
 * The block's author menu is wrapped HERE rather than passed through: the
 * wrapper closes over the block's own agent id, and building it inside the memo
 * keeps the host's per-keystroke renders from producing a new function identity
 * for a block that did not change.
 */
const GroupItemView = memo(function GroupItemView({
  item,
  hasFollowUp,
  ...rowProps
}: GroupItemProps) {
  if (item.kind === "run") {
    const {
      onMentionContextMenu,
      nameOf,
      chatSessionId,
      readOnly,
      focusBlockSessionId,
    } = rowProps;
    return (
      <ChatExecutionBlock
        block={item}
        chatSessionId={chatSessionId}
        // A block always starts a new turn on this surface, which shows an
        // avatar and name above every utterance — so the run wears one too.
        showHeader
        hasFollowUp={hasFollowUp}
        authorName={nameOf(item.anchorRow.from).name}
        canAnswer={!readOnly}
        onJumpToExec={rowProps.onJumpToExec}
        onReplyToRound={rowProps.onReplyToRound}
        focusInput={
          !!focusBlockSessionId && item.execSessionId === focusBlockSessionId
        }
        onAuthorContextMenu={
          onMentionContextMenu
            ? (e) => onMentionContextMenu(e, item.instanceId)
            : undefined
        }
      />
    );
  }
  return <GroupRowView {...rowProps} row={item.row} />;
});

// ── Mention resolution ────────────────────────────────────────────────

/** Resolve `@<agent name>` tokens against the session's agent
 *  participants (the client resolves mentions — the Gateway never parses
 *  free text, §5.3). Longest name wins; a name inside another word still
 *  counts (wechat-style, cheap and predictable). */
export function resolveMentions(
  text: string,
  agents: Array<{ id: string; name: string }>,
): string[] {
  const hits: string[] = [];
  const sorted = [...agents].sort((a, b) => b.name.length - a.name.length);
  for (const a of sorted) {
    if (!a.name) continue;
    if (text.includes(`@${a.name}`)) hits.push(a.id);
  }
  return [...new Set(hits)];
}

/** Active `@`-mention token under the caret, for the autocomplete picker.
 *  `start` is the index of the `@`; `query` is the (whitespace-free) text
 *  typed after it. The caret must sit inside the token, and the `@` must
 *  open a word (start of text or after whitespace) so `a@b` never triggers.
 *  Mirrors `resolveMentions`' `@Name` matching — a picked entry always
 *  fans out. Returns `null` when the caret is not in a mention token. */
export function detectMention(
  value: string,
  caret: number,
): { query: string; start: number } | null {
  const before = value.slice(0, caret);
  const at = before.lastIndexOf("@");
  if (at < 0) return null;
  const token = before.slice(at + 1);
  // Whitespace ends the query — "@foo bar" closes the picker.
  if (/\s/.test(token)) return null;
  if (at > 0 && !/\s/.test(before[at - 1]!)) return null;
  return { query: token, start: at };
}

/** Case-insensitive substring filter on the display name (wechat-style: a
 *  name inside another word still matches — same rule as resolveMentions).
 *  Generic so the caller's extra fields (e.g. `isAgent`, used to pick the
 *  row avatar) survive the filter. */
export function filterMentionCandidates<T extends { id: string; name: string }>(
  candidates: T[],
  query: string,
): T[] {
  const q = query.toLowerCase();
  if (!q) return candidates;
  return candidates.filter((c) => c.name.toLowerCase().includes(q));
}

/** Splice `@name ` over the active mention token and report the new caret
 *  position (just past the inserted text and one trailing space). */
export function applyMention(
  value: string,
  caret: number,
  mentionStart: number,
  name: string,
): { value: string; caret: number } {
  const inserted = `@${name} `;
  const next = value.slice(0, mentionStart) + inserted + value.slice(caret);
  return { value: next, caret: mentionStart + inserted.length };
}

/**
 * Turn the composer's address picks into the wire address (ADR-106 D6/D7,
 * redrawn by the 2026-11-04 revision).
 *
 * A typed `@session` is MATERIAL now, not a delivery: the picker hands back
 * `refs` (the runs attached as quoted context), which ride the send as the flat
 * `ChatAddress.refs` the Gateway pins per round. `target_sessions` is no longer
 * produced here — it survives only on the addressing-card answer path
 * (`chatCardAnswer`), where the human is genuinely choosing a delivery.
 * "new task" is still a per-agent `force_new_for` refusal to continue.
 *
 * An address rides ONLY the targets this message actually mentions: a chip
 * whose `@` the human later deleted must never reach the body. `undefined`
 * (nothing chosen, or nothing still mentioned) is not an empty object — the
 * absence of an address is what keeps the turn on the ladder's lower levels.
 */
export function buildChatAddress(
  targets: string[],
  addresses: Record<string, AddressPick>,
): ChatAddress | undefined {
  const refs: ChatRefPick[] = [];
  const forceNewFor: string[] = [];
  for (const target of targets) {
    const pick = addresses[target];
    if (!pick) continue;
    if (pick.refs?.length) refs.push(...pick.refs);
    else if (pick.forceNew) forceNewFor.push(target);
  }
  if (refs.length === 0 && forceNewFor.length === 0) return undefined;
  return {
    ...(refs.length > 0 ? { refs } : {}),
    ...(forceNewFor.length > 0 ? { forceNewFor } : {}),
  };
}

// ── View ──────────────────────────────────────────────────────────────

export function GroupChatPanel() {
  const { t } = useTranslation();
  const activeGroupId = useProjectGroupStore((s) => s.activeGroupId);
  // The active session may be a project group OR a 1:1 `user_user` DM — both
  // live in the shared `chat_sessions` store and render through this surface.
  const group = useProjectGroupStore(
    (s) =>
      s.groups.find((g) => g.session_id === s.activeGroupId) ??
      s.dms.find((d) => d.session_id === s.activeGroupId) ??
      null,
  );
  const me = useAuthStore((s) => s.account?.user_id ?? "");
  const win = useChatSessionStore((s) =>
    activeGroupId ? s.windows[activeGroupId] : undefined,
  );
  const sending = useChatSessionStore((s) =>
    activeGroupId ? s.sending[activeGroupId] : false,
  );
  // The turn this device sent here and has not seen an outcome for (ADR-106
  // D9's claim, keyed by the id the send's response carried).
  const inflightTurn = useChatSessionStore((s) =>
    activeGroupId ? s.inflight[activeGroupId] : undefined,
  );
  const pendingLocateSeq = useChatSessionStore((s) =>
    activeGroupId ? (s.windows[activeGroupId]?.pendingLocateSeq ?? null) : null,
  );

  const [historyOpen, setHistoryOpen] = useState(false);
  // Scroll viewport, captured alongside `useStickToBottom`'s callback ref so
  // the locate effect can query `[data-seq]` rows within it.
  const containerRef = useRef<HTMLDivElement | null>(null);

  const [draft, setDraft] = useState("");
  // The round the composer is answering, when it is answering one (the chip
  // above the input names it; see `ReplyTarget`).
  const [replyTarget, setReplyTarget] = useState<ReplyTarget | null>(null);
  const [directory, setDirectory] = useState<DirectoryUser[]>([]);
  const nameOf = useParticipantNamer(directory);
  const agentsMap = useAgentStore((s) => s.agents);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  // Right-click on an agent's avatar / name → the "@name" menu. One menu for
  // the whole panel: the payload carries the right-clicked participant.
  const mentionMenu = useContextMenu<{ id: string }>();
  // Composer attachments — uploaded into the group's `files/` dir (§4.2.1)
  // before send; the send carries their ids so the durable row and each
  // mentioned agent's execution turn both see them (mirrors AgentChatView).
  const [pendingAttachments, setPendingAttachments] = useState<
    ChatAttachmentMeta[]
  >([]);
  const [uploading, setUploading] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);

  // @-mention autocomplete: the active token under the caret + which popup
  // row is highlighted (keyboard nav). `null` = caret is not inside a mention.
  const [mention, setMention] = useState<{
    query: string;
    start: number;
  } | null>(null);
  const [mentionActive, setMentionActive] = useState(0);
  /** Which gesture opened the popup, so it can hang off the right thing. */
  const [mentionAnchor, setMentionAnchor] = useState<"button" | "typed">("typed");
  const pickerRef = useRef<HTMLDivElement>(null);
  // Second level of the `@` (ADR-106 D7): the agent whose task list is open.
  // It is a stage, not a choice — choosing is `addresses`.
  const [addressStage, setAddressStage] = useState<{
    instanceId: string;
    name: string;
  } | null>(null);
  // Reference picks the human has made for this draft, per agent instance. They
  // ride the send as `refs` (material) / `force_new_for` (a refusal to
  // continue), which is the one fact the Router is never allowed to re-decide
  // (D6: an address is an instruction).
  const [addresses, setAddresses] = useState<Record<string, AddressPick>>({});
  // ADR-106 revision 2026-11-04: the run whose block the `@` picker's "去续投"
  // just located — that block reveals + focuses its in-place reply box. Cleared
  // on the next commit so a stale focus never re-fires.
  const [focusBlockSessionId, setFocusBlockSessionId] = useState<string | null>(
    null,
  );

  // Virtualized transcript handle (see the adapters below). Declared before the
  // follow hook: the hook delegates bottom-positioning to the list's
  // convergence anchor, because a single `scrollTop = scrollHeight` write
  // cannot land on a virtualized bottom whose height is still an estimate.
  const listRef = useRef<VirtualRowListHandle | null>(null);

  // Bottom-positioning mechanism for the follow controller — same contract as
  // in AgentChatView; see VirtualRowList.scrollToBottom for the arming.
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
    [win?.tail.length ?? 0, activeGroupId],
    activeGroupId,
    () => {},
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

  // Directory for human sender names — best-effort, once per mount.
  useEffect(() => {
    const token = useAuthStore.getState().accessToken;
    if (!token) return;
    void fetchDirectory(getGatewayUrl(), token)
      .then(setDirectory)
      .catch(() => {});
  }, []);

  // The stream is now a straight durable log: every retired milestone renders
  // as nothing (the shared renderer returns null) and the only state line the
  // surface adds is derived below. No projection layer is needed.
  const rows = useMemo(
    () => [...(win?.older ?? []), ...(win?.tail ?? [])],
    [win],
  );
  // D9's claim read against this transcript: the send is owed its first visible
  // outcome until a row answers its id. `sending` alone only covers the POST, so
  // the wait between delivery and the round's reference row showed nothing — and
  // left the send button live next to a message still being decided, with no
  // Chat-side queue to absorb the second send.
  const turnLive = turnInflight(rows, inflightTurn);
  const composerLocked = sending || turnLive;
  // Release the claim once a row answers it, so the surface stops asserting a
  // turn the transcript has already settled.
  useEffect(() => {
    if (!activeGroupId || !inflightTurn) return;
    if (turnInflight(rows, inflightTurn)) return;
    useChatSessionStore
      .getState()
      .settleTurn(activeGroupId, inflightTurn.messageId);
  }, [rows, inflightTurn, activeGroupId]);
  // Virtualization adapters (render-perf L2): the same keys, estimates and
  // seq→item index the 1:1 view uses, scoped to this group.
  const {
    items,
    rowsRef,
    keyOfSeqRef,
    virtualKeyOf,
    virtualEstimate,
    virtualAttrs,
    seqsByItem,
  } = useTranscriptLayout(rows, activeGroupId ?? "");

  // Locate-and-flash for history jumps (ADR-097) — declared AFTER the follow
  // hook so the center-scroll wins over follow-to-bottom on the jump commit.
  const highlightSeq = useLocateMessage(
    () => containerRef.current,
    pendingLocateSeq,
    () =>
      activeGroupId &&
      useChatSessionStore.getState().clearLocate(activeGroupId),
    // Virtualization fallback: the row is in the window but not in the DOM. A
    // folded row is not an item of its own either, so its seq resolves through
    // the layout's `keyOfSeq` to the block that holds it (ADR-097).
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
  // message_id → sender index, so a reply with an empty `to` can name the
  // counterparty from the message it answers (`in_reply_to`'s `from`).
  // message_id → row, for the receipts that name the message they are about:
  // a channel failure is placed in that message's column instead of floating
  // center (design 31 §6 leaves this join to the presentation layer).
  const rowByMessageId = useMemo(() => indexByMessageId(rows), [rows]);
  const fromById = useMemo(() => {
    const m = new Map<string, string>();
    for (const r of [...(win?.older ?? []), ...(win?.tail ?? [])]) {
      if (r.meta?.message_id) m.set(r.meta.message_id, r.from);
    }
    return m;
  }, [win]);
  const hasMoreOlder = win ? win.firstPos > 0 : false;
  const hasMoreNewer = win ? win.lastPos < win.total - 1 : false;
  // Auto-pagination: scrolling into the top band fires the next older page;
  // reaching the bottom band (only after a locate parks the window mid-history)
  // pages back toward the newest. `useStickToBottom`'s prepend-anchoring holds
  // the reading position as older rows land above.
  const handleStreamScroll = (event: UIEvent<HTMLDivElement>) => {
    const el = event.currentTarget;
    if (!activeGroupId || !win) return;
    if (hasMoreOlder && !win.loadingOlder && el.scrollTop <= 48) {
      void useChatSessionStore.getState().loadOlder(activeGroupId);
    }
    if (
      hasMoreNewer &&
      !win.loadingNewer &&
      el.scrollHeight - el.scrollTop - el.clientHeight <= 48
    ) {
      void useChatSessionStore.getState().loadNewer(activeGroupId);
    }
  };
  const agentParticipants = useMemo(() => {
    if (!group?.participants) return [];
    return group.participants
      .filter((p) => p.id !== me && (p.identity === "agent" || !p.identity))
      .map((p) => ({ id: p.id, name: nameOf(p.id).name }))
      .filter((p) => nameOf(p.id).isAgent);
  }, [group, me, nameOf]);

  // Everything the @ picker lists. Design 27 §5.3.1: @-ing an Agent wakes it,
  // @-ing a person is a mention that triggers nothing — but both are pickable,
  // so the picker shows every participant, not just agents. The fan-out
  // targets (`resolveMentions`) still run over `agentParticipants` alone.
  //
  // A `user_user` DM opts out entirely: with two people and no agent there is
  // nothing to wake and the only @-able target is the peer themselves, so an
  // autocomplete popup would be pure noise. Returning an empty candidate set
  // hides the picker and makes every send archive-only (see `submit`).
  const mentionParticipants = useMemo(() => {
    if (!group?.participants) return [];
    if (group.kind === "user_user") return [];
    return group.participants
      .filter((p) => p.id !== me)
      .map((p) => {
        const who = nameOf(p.id);
        return {
          id: p.id,
          name: who.name,
          isAgent: who.isAgent,
          avatarUrl: who.avatarUrl,
          builtinAvatarId: who.builtinAvatarId,
        };
      })
      .filter((p) => p.name);
  }, [group, me, nameOf]);

  // Candidates for the open popup: mentionable participants narrowed by the
  // typed query.
  const mentionCandidates = useMemo(
    () =>
      mention
        ? filterMentionCandidates(mentionParticipants, mention.query)
        : [],
    [mention, mentionParticipants],
  );

  // Keyboard nav changes the highlight index without the browser scrolling
  // the overflow container — follow the active row into view. `block:nearest`
  // is a no-op when the row is already visible, so mouse-wheel browsing is
  // untouched.
  useEffect(() => {
    if (!mention || mentionCandidates.length === 0) return;
    pickerRef.current
      ?.querySelector<HTMLElement>(`[data-mention-index="${mentionActive}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [mentionActive, mention, mentionCandidates.length]);

  // Typing: the popup follows the text and the caret, hanging off the composer.
  const syncMention = useCallback((value: string, caret: number) => {
    setMentionAnchor("typed");
    setMention(detectMention(value, caret));
    setMentionActive(0);
  }, []);

  // A caret move. React dispatches `onSelect` on focus, so this also runs when
  // the button's own popup takes the focus: a caret still sitting where the
  // button opened the picker is that dispatch, not a human gesture, and
  // re-deriving the popup there would move it off the button and out from under
  // the click that was about to land on it.
  const syncCaret = useCallback(
    (value: string, caret: number) => {
      if (mentionAnchor === "button" && mention && caret === mention.start) {
        return;
      }
      syncMention(value, caret);
    },
    [mentionAnchor, mention, syncMention],
  );

  // The toolbar's `@` — the same picker typing `@` opens, reachable without
  // knowing the shortcut (ADR-106 D7's ladder is one gesture, not two grammars).
  // The draft is untouched either way: the button opens the picker, and picking
  // a name is what writes `@name ` into the text.
  const openMentionPicker = useCallback(() => {
    // A second click on the button closes what it opened: it advertises its own
    // state through `aria-expanded`, so it must be able to take it back.
    if (mention && mentionAnchor === "button") {
      setMention(null);
      return;
    }
    setMentionAnchor("button");
    const ta = textareaRef.current;
    const caret = ta?.selectionStart ?? draft.length;
    // Nothing is typed: the button is a door, not a keystroke. The popup opens
    // over the caret, and picking a name is the one gesture that writes one —
    // an `@` dropped into the draft would be a character the human never typed
    // (and one they then have to delete).
    setMention(detectMention(draft, caret) ?? { query: "", start: caret });
    setMentionActive(0);
    // Focus only, no `setSelectionRange`: a caret move here fires `select`,
    // which re-syncs the popup against the text and re-anchors it to the
    // composer — the button's popup would leap away from the button.
    ta?.focus();
  }, [draft, mention, mentionAnchor]);

  const pickMention = useCallback(
    (candidate: { id: string; name: string; isAgent: boolean }) => {
      const ta = textareaRef.current;
      if (!ta || !mention) return;
      const caret = ta.selectionStart ?? draft.length;
      const { value, caret: nextCaret } = applyMention(
        draft,
        caret,
        mention.start,
        candidate.name,
      );
      setDraft(value);
      setMention(null);
      // Every path leaves the caret just past the token, before anything else
      // renders. The WebView fires `select` when a focused textarea's value
      // changes under it, and a caret still sitting inside the old `@` makes the
      // picker re-open on top of the second level — the same question, asked
      // twice, with the answer already given.
      requestAnimationFrame(() => {
        ta.focus();
        ta.setSelectionRange(nextCaret, nextCaret);
      });
      // Only an agent has a second level: a person is a mention that triggers
      // nothing, so asking "which of their tasks" would offer a delivery that
      // cannot happen — and the popup would be the surface promising it.
      if (candidate.isAgent) {
        setAddressStage({ instanceId: candidate.id, name: candidate.name });
      }
    },
    [draft, mention],
  );

  // The second level's answer. "Nothing chosen" is an answer (D7 keeps the
  // ladder's lower levels reachable), and it clears the chip rather than storing
  // an empty address — an empty address is the absence, not a decision.
  const applyAddress = useCallback(
    (pick: AddressPick) => {
      const stage = addressStage;
      if (stage) {
        setAddresses((prev) => {
          const next = { ...prev };
          if (pick.refs?.length || pick.forceNew) {
            next[stage.instanceId] = pick;
          } else {
            delete next[stage.instanceId];
          }
          return next;
        });
      }
      setAddressStage(null);
      const ta = textareaRef.current;
      requestAnimationFrame(() => ta?.focus());
    },
    [addressStage],
  );

  const clearAddress = useCallback((instanceId: string) => {
    setAddresses((prev) => {
      const next = { ...prev };
      delete next[instanceId];
      return next;
    });
  }, []);

  // Remove ONE reference from an agent's pick. Dropping the last one clears the
  // chip entirely (unless a `forceNew` refusal still stands) — swallowing a
  // picked reference would make it indistinguishable from one never given.
  const removeRef = useCallback((instanceId: string, key: string) => {
    setAddresses((prev) => {
      const pick = prev[instanceId];
      if (!pick) return prev;
      const refs = (pick.refs ?? []).filter(
        (x) => `${x.agentInstanceId}|${x.sessionId}` !== key,
      );
      const labels = { ...pick.labels };
      delete labels[key];
      const next = { ...prev };
      if (refs.length === 0 && !pick.forceNew) delete next[instanceId];
      else next[instanceId] = { ...pick, refs, labels };
      return next;
    });
  }, []);

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
        showToast({
          type: "info",
          message: t("chatView.address.blockNotLoaded"),
        });
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
  // The words being re-sent come from the service, not from `rowsRef`: a card
  // whose message sits outside the loaded window still has to answer, and the
  // window lookup returned silently when it missed (design 31 §6). Only the
  // click — which agent session, or new work — is the surface's.
  const answerCard = useCallback(
    async (card: ChatMessageRow, choice: CardChoice) => {
      if (!activeGroupId) return;
      const cardId = card.meta?.message_id;
      if (!cardId) return;
      const lookup = await fetchCardAnswer(activeGroupId, cardId);
      if (!lookup.ok) {
        // A round already settled the question (`already_answered`) settles
        // itself on the next projection read; `gone` cannot be re-delivered and
        // has to be said out loud.
        if (lookup.reason === "gone") {
          showToast({ type: "error", message: t("chatView.cardAnswerGone") });
        }
        return;
      }
      const answer = answerWithChoice(lookup.facts, choice);
      if (!answer) return;
      const mentioned = resolveMentions(answer.text, agentParticipants);
      // The card came from one agent, so the answer must reach that one: the
      // original message may have named nobody (the group's default responder
      // took it, ADR-105), and an address that is not a target of this send is
      // refused by the Gateway rather than dropped.
      const targets = mentioned.includes(answer.agentId)
        ? mentioned
        : [...mentioned, answer.agentId];
      void useChatSessionStore
        .getState()
        .sendGroupText(
          activeGroupId,
          answer.text,
          targets,
          undefined,
          answer.attachmentIds,
          groupCardAddress(answer),
        );
    },
    [activeGroupId, agentParticipants, t],
  );

  // Chips follow the draft: an address whose `@` the human deleted stops being
  // an instruction for this message, so it stops being shown (and, via
  // `buildChatAddress`, stops being sent).
  const addressRows = useMemo(() => {
    const mentioned = new Set(resolveMentions(draft, agentParticipants));
    return Object.entries(addresses)
      .filter(([id]) => mentioned.has(id))
      .map(([id, pick]) => ({ instanceId: id, name: nameOf(id).name, pick }));
  }, [addresses, draft, agentParticipants, nameOf]);

  const submit = useCallback(async () => {
    const body = draft.trim();
    const attachmentIds = pendingAttachments.map((a) => a.id);
    // The lock is checked here too, not only on the button: Enter in the
    // textarea and every other call path reach the send through this function.
    if (
      (!body && attachmentIds.length === 0) ||
      !activeGroupId ||
      uploading ||
      composerLocked
    )
      return;
    setDraft("");
    const sentAttachments = pendingAttachments;
    const sentReplyTo = replyTarget?.id;
    const sentAddresses = addresses;
    setPendingAttachments([]);
    setAddresses({});
    setFocusBlockSessionId(null);
    stickNow();
    const targets = resolveMentions(body, agentParticipants);
    // A reply to a block is already level 1 — the round it points at names its
    // own session, so a second-level address must not ride the same send.
    const address = sentReplyTo
      ? undefined
      : buildChatAddress(targets, sentAddresses);
    const ok = await useChatSessionStore
      .getState()
      .sendGroupText(
        activeGroupId,
        body,
        targets,
        sentReplyTo,
        attachmentIds,
        address,
      );
    if (ok) setReplyTarget(null);
    // Refund the composer on failure so nothing typed, attached or addressed is
    // lost — an address is as much the human's input as the text is.
    if (!ok) {
      setDraft(body);
      setPendingAttachments(sentAttachments);
      setAddresses(sentAddresses);
    }
  }, [
    draft,
    activeGroupId,
    agentParticipants,
    stickNow,
    pendingAttachments,
    replyTarget,
    addresses,
    uploading,
    composerLocked,
  ]);

  // Upload each chosen file into the group's `files/` dir and stack a chip.
  // Shared by the paperclip picker and the paste handler (§4.2.1).
  const uploadFiles = useCallback(
    async (files: File[]) => {
      if (files.length === 0 || !activeGroupId) return;
      setUploading(true);
      try {
        for (const f of files) {
          const meta = await useChatSessionStore
            .getState()
            .upload(activeGroupId, f);
          setPendingAttachments((prev) => [...prev, meta]);
        }
      } catch (err) {
        console.warn("[GroupChatPanel] attachment upload failed:", err);
      } finally {
        setUploading(false);
      }
    },
    [activeGroupId],
  );

  const handleFilesChosen = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const files = Array.from(e.target.files ?? []);
    e.target.value = ""; // allow re-picking the same file
    await uploadFiles(files);
  };

  /** Insert `text` at the textarea's caret / selection range, then re-sync
   *  the mention picker against the new caret. */
  const insertTextAtCaret = useCallback(
    (text: string) => {
      const ta = textareaRef.current;
      const current = ta?.value ?? draft;
      if (!ta) {
        setDraft(current + text);
        return;
      }
      const start = ta.selectionStart ?? current.length;
      const end = ta.selectionEnd ?? current.length;
      const next = current.slice(0, start) + text + current.slice(end);
      setDraft(next);
      const caret = start + text.length;
      requestAnimationFrame(() => {
        ta.focus();
        ta.setSelectionRange(caret, caret);
        syncMention(next, caret);
      });
    },
    [draft, syncMention],
  );

  // Ctrl+V parity with the 1:1 chat surface: files, images and OS screenshots
  // paste into the attachment tray; plain text inserts at the caret. Shared
  // classification (`pasteDetect.ts`); preventDefault always runs so the
  // WebView's default paste never double-fires alongside our own handling.
  const handlePaste = async (e: React.ClipboardEvent<HTMLTextAreaElement>) => {
    const cd = e.clipboardData;
    if (!cd) return;
    e.preventDefault();
    const outcome = await detectPaste(cd);
    switch (outcome.kind) {
      case "files": {
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
            console.warn("[GroupChatPanel] clipboard file unreadable:", p, err);
          }
        }
        void uploadFiles(files);
        return;
      }
      case "image": {
        const type = outcome.blob.type || `image/${outcome.extension}`;
        const name = `screenshot-${Date.now()}.${outcome.extension}`;
        void uploadFiles([new File([outcome.blob], name, { type })]);
        return;
      }
      case "text":
        insertTextAtCaret(outcome.text);
        return;
      case "fileNames":
        insertTextAtCaret(outcome.names.join("\n"));
        return;
      case "nothing":
        return;
    }
  };

  const removeAttachment = (id: string) =>
    setPendingAttachments((prev) => prev.filter((a) => a.id !== id));

  // Address the composer to one round (ADR-106 D6 level 1). This is how a
  // `pause` card inside a block is continued and how a finished round gets a
  // follow-up: the human's next message carries the reference row's id, so the
  // delivery names the session without asking a model what the round was about.
  const handleReplyToRound = useCallback((rowId: string, quote: string) => {
    setReplyTarget({ id: rowId, quote });
    textareaRef.current?.focus();
  }, []);

  // Jump to the exec session behind a group agent reply: select that agent
  // (closes the group in the middle pane, same as picking it in the sidebar),
  // cold-load the exec tab, flip the surface to exec. Mirrors AgentChatView.
  // A deleted exec session is answered with a toast and no navigation — the
  // same probe AgentChatView.jumpToExec runs (the message's exec pointer
  // outlives the session it names, §5.5.5).
  const jumpToExec = useCallback(
    async (agentId: string, execSid: string) => {
      if (!agentId || !execSid) return;
      if (!(await sessionExists(agentId, execSid))) {
        showToast({ type: "info", message: t("chatView.execSessionGone") });
        return;
      }
      useAgentStore.getState().selectAgent(agentId);
      await useChatStore
        .getState()
        .openSession(agentId, execSid, { prepend: true });
      useChatSessionStore.getState().setMode(agentId, "exec");
    },
    [t],
  );
  // Void-returning stable handler for memoized rows — an inline
  // `(aid, sid) => void jumpToExec(aid, sid)` is a fresh identity per render
  // and would defeat `GroupRowView`'s memo on every keystroke.
  const handleJumpToExec = useCallback(
    (agentId: string, execSid: string) => void jumpToExec(agentId, execSid),
    [jumpToExec],
  );

  const participants = group?.participants ?? [];
  // Admin cross-account view: the panel lists groups the admin never joined.
  // Reading is allowed (server-side admin bypass), posting is not — hide the
  // composer and card actions for anyone who isn't a participant.
  const canPost = participants.some((p) => p.id === me);

  // Right-click on an agent's avatar / name → the "@name" menu. No menu
  // without a composer: a read-only viewer has nowhere to put the mention.
  // `mentionMenu.openAt` is the hook's stable callback (deps `[]`), so this
  // handler keeps its identity across renders — the memoized rows take it
  // as a prop and must not re-render on every composer keystroke.
  const handleMentionContextMenu = useCallback(
    (e: ReactMouseEvent, id: string) => {
      if (!canPost) return;
      mentionMenu.openAt(e, { id });
    },
    [canPost, mentionMenu.openAt],
  );

  // The menu's one item: drop `@<name> ` into the composer at the caret —
  // the same text the picker inserts, so the fan-out resolves it (see
  // `resolveMentions`). No icon: the label already reads `@<name>`, and an
  // @-icon next to it doubles the glyph.
  //
  // Right-clicking a participant is the same intent as picking it from the `@`
  // popup, so it opens the second level too — otherwise the two entry points
  // would deliver different routing for one gesture's worth of words.
  const mentionMenuItems = useMemo<ContextMenuItem<{ id: string }>[]>(() => {
    const id = mentionMenu.payload?.id;
    if (!id) return [];
    const who = nameOf(id);
    return [
      {
        key: "mention",
        label: t("groupChat.mentionMenu", { name: who.name }),
        onClick: () => {
          insertTextAtCaret(`@${who.name} `);
          if (who.isAgent) {
            setAddressStage({ instanceId: id, name: who.name });
          }
        },
      },
    ];
  }, [insertTextAtCaret, mentionMenu.payload, nameOf, t]);

  if (!activeGroupId) return null;

  // A `user_user` pair is the smallest group — two people. Render it as a DM:
  // the header shows the peer's name (not the empty session title) and drops
  // the "N members" chip, which is noise for a conversation of exactly two.
  const isDm = group?.kind === "user_user";
  const peerId = isDm ? participants.find((p) => p.id !== me)?.id : undefined;
  const title = isDm
    ? peerId
      ? nameOf(peerId).name
      : activeGroupId.slice(0, 8)
    : group?.title || activeGroupId.slice(0, 8);

  // One popup, two anchors: the toolbar button opens it under its own icon,
  // a typed `@` opens it above the composer, where the character was typed.
  const mentionPopup = mention && mentionCandidates.length > 0 && (
            <div
              onMouseDown={(e) => e.preventDefault()}
              className="w-64 overflow-hidden rounded-lg border border-border-outer bg-modal-surface shadow-xl"
            >
              <OverlayScrollbar
                ref={pickerRef}
                role="listbox"
                aria-label={t("groupChat.mentionLabel")}
                /** Inset from the shell so the cursor row's wash floats
                 *  inside the hairline — same popup grammar as the second
                 *  level (`ChatAddressPicker`), which the two levels must
                 *  match to read as one interaction. */
                className="px-1 py-1"
                maxHeight={224}
              >
                {mentionCandidates.map((c, i) => {
                  const meta = c.isAgent ? agentsMap[c.id]?.meta : undefined;
                  const avatar = c.isAgent ? (
                    <AgentAvatar
                      agentId={c.id}
                      displayName={c.name}
                      avatarUrl={meta?.avatar ?? null}
                      builtinAvatarId={meta?.builtin_avatar ?? null}
                      size={20}
                      className="shrink-0"
                    />
                  ) : (
                    <UserAvatar
                      displayName={c.name}
                      avatarUrl={c.avatarUrl}
                      builtinAvatarId={c.builtinAvatarId}
                      size={20}
                      className="shrink-0"
                    />
                  );
                  return (
                    <button
                      key={c.id}
                      type="button"
                      role="option"
                      data-mention-index={i}
                      aria-selected={i === mentionActive}
                      onClick={() => pickMention(c)}
                      onMouseEnter={() => setMentionActive(i)}
                      className={cn(
                        "flex w-full items-center gap-2 rounded px-3 py-1.5 text-left text-xs",
                        // The app-wide accent tint for the cursor row, not the
                        // neutral `--color-nav-item-hover` (icon controls only).
                        i === mentionActive
                          ? `${listRowSelected} text-text`
                          : "text-text-secondary",
                      )}
                    >
                      {avatar}
                      <span className="min-w-0 flex-1 truncate">
                        {c.name}
                      </span>
                    </button>
                  );
                })}
              </OverlayScrollbar>
            </div>
  );


  return (
    <div className="flex min-w-0 flex-1 flex-col overflow-hidden rounded-xl border border-border-outer bg-chat-body">
      <header className="flex h-9 shrink-0 items-center gap-2 border-b border-border-divider px-4 text-xs font-semibold text-text-secondary">
        <span className="truncate">{title}</span>
        {!isDm && (
          <span className="shrink-0 font-normal text-text-tertiary">
            {t("groupChat.members", { count: participants.length })}
          </span>
        )}
        {/* The history tile must sit in the header's TOP-RIGHT corner. The
            auto-margin belongs on the flex item — and `Tooltip` renders its
            own `relative inline-flex` wrapper as that flex item, so `ml-auto`
            on the nested button does nothing. Wrap the tile in a flex-item
            `div` carrying `ml-auto` (same shape as `SessionTabBar`'s action
            cluster) to push it to the right edge. */}
        <div className="ml-auto inline-flex items-center">
          <Tooltip content={t("chatHistory.title")} variant="plain">
            <button
              type="button"
              className={cn(toolbarIconButton, "h-6 w-6")}
              onClick={() => setHistoryOpen(true)}
              aria-label={t("chatHistory.title")}
            >
              <OutlineChatHistoryIcon size={14} />
            </button>
          </Tooltip>
        </div>
      </header>

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
            <div className="flex h-full items-center justify-center text-xs text-text-tertiary">
              {t("groupChat.empty")}
            </div>
          )}
          {/* Virtualized transcript window (render-perf L2) — see AgentChatView.
            `key` remounts the shell on group switch (stale-offset contract). */}
          <VirtualRowList
            key={activeGroupId}
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
            renderItem={(item, i) => {
              const row = item.kind === "row" ? item.row : null;
              return (
                <GroupItemView
                  item={item}
                  hasFollowUp={i < items.length - 1}
                  me={me}
                  chatSessionId={activeGroupId}
                  nameOf={nameOf}
                  onJumpToExec={handleJumpToExec}
                  onReplyToRound={handleReplyToRound}
                  triggerFrom={
                    row?.meta?.in_reply_to
                      ? fromById.get(row.meta.in_reply_to)
                      : undefined
                  }
                  noticeAnchor={
                    row?.kind === "system_notice"
                      ? noticeAnchorOf(row, rowByMessageId, me)
                      : undefined
                  }
                  readOnly={!canPost}
                  onMentionContextMenu={handleMentionContextMenu}
                  onChooseCard={canPost ? answerCard : undefined}
                  onLocateRef={handleLocateRef}
                  focusBlockSessionId={focusBlockSessionId ?? undefined}
                />
              );
            }}
          />
          {/* The one "still working" line this surface may add for itself: a
            send THIS device made has not been answered yet. The claim starts at
            that send's response id and ends at the first row answering it (the
            round's reference row, an agent's own reply, a receipt) or on a
            deadline — never at a shape read off the transcript, which is how a
            dead node used to look alive, and never longer than the turn it
            speaks for. Whether an agent is chewing on a round is a fact about
            the ROUND, and the round speaks for itself inside its block
            (ADR-106 D9). Generic wording: one utterance fans out to several
            agents, so naming one of them would be a guess. */}
          {composerLocked && (
            <ChatTurnStatusLine
              className="px-4"
              live
              text={t("chatView.notice.pendingTurnGeneric")}
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
                  onClick={() =>
                    activeGroupId &&
                    void useChatSessionStore.getState().loadNewer(activeGroupId)
                  }
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

      <div className="shrink-0">
        {!canPost ? (
          <div className="mx-6 mb-3 rounded-xl border border-chat-input-border bg-chat-input-bg px-3 py-2 text-center text-10 text-text-tertiary">
            {t("groupChat.readOnly")}
          </div>
        ) : (
          /* Unified input container — mirrors InboxPanel's composer card (the
             user↔user inbox) so the group chat reads as the same primitive as
             every other chat surface: rounded-xl surface wrapping a borderless
             textarea, with the toolbar (Paperclip + send) collapsed to the
             bottom row. The composer carries attachments like the 1:1 chat
             surface, so the Paperclip sits just left of the send button. */
          <div className="relative mx-6 mb-3 rounded-xl border border-chat-input-border bg-chat-input-bg shadow-[0_1px_3px_rgba(0,0,0,0.04)] dark:shadow-[0_1px_3px_rgba(0,0,0,0.4)]">
            <input
              ref={fileInputRef}
              type="file"
              multiple
              className="hidden"
              onChange={handleFilesChosen}
            />
            {/* The round this message answers (ADR-106 D6 level 1) — the same
                chip grammar the 1:1 thread uses, so a continuation reads the
                same on both surfaces. */}
            {replyTarget && (
              <div className="flex items-center gap-1 px-3 pt-2">
                <span className={composerChip}>
                  {t("chatView.replyingTo")} {replyTarget.quote.slice(0, 30)}
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
            {/* Second-level references the human attached (ADR-106 D7, revision
                2026-11-04). Each is its own chip because a reference is an
                instruction, and an instruction the surface swallowed would be
                indistinguishable from one never given. */}
            {addressRows.length > 0 && (
              <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
                {addressRows.map(({ instanceId, name, pick }) => (
                  <Fragment key={instanceId}>
                    {pick.forceNew && (
                      <span className={composerChip}>
                        @{name} · {t("chatView.block.newTask")}
                        <button
                          type="button"
                          onClick={() => clearAddress(instanceId)}
                          className="ml-0.5 inline-flex items-center justify-center rounded-sm hover:bg-[var(--color-accent)]/15"
                          aria-label={t("chatView.address.clear")}
                        >
                          ×
                        </button>
                      </span>
                    )}
                    {(pick.refs ?? []).map((r) => {
                      const key = `${r.agentInstanceId}|${r.sessionId}`;
                      return (
                        <span key={key} className={composerChip}>
                          @{name} ·{" "}
                          <Quote className="h-3 w-3 shrink-0" />{" "}
                          {pick.labels?.[key] || r.sessionId}
                          <button
                            type="button"
                            onClick={() => removeRef(instanceId, key)}
                            className="ml-0.5 inline-flex items-center justify-center rounded-sm hover:bg-[var(--color-accent)]/15"
                            aria-label={t("chatView.address.clear")}
                          >
                            ×
                          </button>
                        </span>
                      );
                    })}
                  </Fragment>
                ))}
              </div>
            )}
            {/* Pending-attachment tray — chips with a remove affordance,
                same grammar as the 1:1 chat composer. */}
            {pendingAttachments.length > 0 && (
              <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
                {pendingAttachments.map((a) => (
                  <ChatAttachmentChip
                    key={a.id}
                    chatSessionId={activeGroupId}
                    attachment={a}
                    onRemove={() => removeAttachment(a.id)}
                    removeLabel={t("chatPanel.removeAttachment")}
                  />
                ))}
              </div>
            )}
            {mentionAnchor !== "button" && (
              <div className="absolute bottom-full left-3 z-20 mb-2">
                {mentionPopup}
              </div>
            )}
            {/* Second level of the `@` (ADR-106 D7) — the agent's task list,
                opened by picking the agent. Same anchor as the first level, so
                the two read as one interaction. Unlike the first level, this
                shell must NOT cancel `mousedown`: the picker carries a search
                box, and a cancelled mousedown is a focus the input never gets. */}
            {addressStage && activeGroupId && (
              <div className="absolute bottom-full left-3 z-20 mb-2">
                <ChatAddressPicker
                  instanceId={addressStage.instanceId}
                  /** Multi-party surface, addressed from this group: the Runtime
                   *  admits only work bound to this chat that I started. */
                  chatSessionId={activeGroupId}
                  surface="group"
                  agentName={addressStage.name}
                  onPick={applyAddress}
                  onLocate={(sid) => handleLocateRef(sid)}
                  onClose={() => setAddressStage(null)}
                />
              </div>
            )}
            <div className="flex items-end gap-2 px-3 pb-2 pt-1">
              <textarea
                ref={textareaRef}
                value={draft}
                onChange={(e) => {
                  const v = e.target.value;
                  setDraft(v);
                  syncMention(v, e.target.selectionStart ?? v.length);
                }}
                onSelect={(e) => {
                  const ta = e.currentTarget;
                  syncCaret(ta.value, ta.selectionStart ?? ta.value.length);
                }}
                onKeyDown={(e) => {
                  // A second level left open is a question on the screen: Enter
                  // closes it instead of sending, or the send would go out
                  // un-addressed while the human believed they were naming a
                  // session.
                  if (addressStage) {
                    if (e.key === "Enter" && !e.shiftKey) {
                      e.preventDefault();
                      setAddressStage(null);
                    }
                    return;
                  }
                  // While the picker is open it owns Enter / arrows / Tab / Esc
                  // — Enter picks the highlighted member instead of sending.
                  if (mention && mentionCandidates.length > 0) {
                    if (e.key === "ArrowDown") {
                      e.preventDefault();
                      setMentionActive(
                        (i) => (i + 1) % mentionCandidates.length,
                      );
                      return;
                    }
                    if (e.key === "ArrowUp") {
                      e.preventDefault();
                      setMentionActive(
                        (i) =>
                          (i - 1 + mentionCandidates.length) %
                          mentionCandidates.length,
                      );
                      return;
                    }
                    if (e.key === "Enter" || e.key === "Tab") {
                      e.preventDefault();
                      const pick =
                        mentionCandidates[mentionActive] ??
                        mentionCandidates[0];
                      if (pick) pickMention(pick);
                      return;
                    }
                    if (e.key === "Escape") {
                      e.preventDefault();
                      setMention(null);
                      return;
                    }
                  }
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    void submit();
                  }
                }}
                placeholder={
                  agentParticipants.length
                    ? t("groupChat.placeholderWithMentions")
                    : t("groupChat.placeholder")
                }
                aria-label={t("groupChat.send")}
                disabled={composerLocked}
                onPaste={handlePaste}
                className="w-full resize-none border-0 bg-transparent p-3 pb-2 outline-none placeholder:text-text-disabled disabled:cursor-not-allowed disabled:opacity-50 max-h-48 overflow-y-auto min-h-[5rem]"
                style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
              />
            </div>
            {/* Toolbar row — textarea on top, actions grouped at the bottom
                right: Paperclip immediately left of Send, the same cluster
                order the 1:1 chat composer uses (attachment never drifts to
                the far side). */}
            <div className="flex items-center justify-between gap-2 px-3 pb-2">
              <div />
              <div
                data-toolbar-cluster="right"
                className="flex shrink-0 items-center gap-1"
              >
                {/* `@` — the first level of the group address (agents and
                    people, design 27 §5.3.1). Hidden where there is nobody to
                    mention: a human-only DM has no picker to open. */}
                {mentionParticipants.length > 0 && (
                  <div className="relative">
                  <Tooltip
                    content={t("groupChat.mentionLabel")}
                    variant="plain"
                  >
                    <button
                      className={cn(
                        toolbarIconButton,
                        mention && "bg-nav-item-hover",
                      )}
                      onClick={openMentionPicker}
                      disabled={sending}
                      aria-label={t("groupChat.mentionLabel")}
                      aria-expanded={!!mention}
                    >
                      <AtSign size={16} />
                    </button>
                  </Tooltip>
                    {mentionAnchor === "button" && (
                      <div className="absolute bottom-full right-0 z-20 mb-2">
                        {mentionPopup}
                      </div>
                    )}
                  </div>
                )}
                {/* Per-chat ledger read (plan §4.6 phase two): one row per
                    agent participant, merged client-side. A human-only DM
                    has no agent roster and the button hides itself. */}
                <ChatUsageIcon
                  agentIds={agentParticipants.map((p) => p.id)}
                  chatSessionId={activeGroupId ?? ""}
                  resolveName={(id) => nameOf(id).name}
                />
                <Tooltip content={t("chatPanel.attach")} variant="plain">
                  <button
                    className={cn(toolbarIconButton, uploading && "opacity-50")}
                    onClick={() => fileInputRef.current?.click()}
                    disabled={uploading || sending}
                    aria-label={t("chatPanel.attach")}
                  >
                    <Paperclip size={16} />
                  </button>
                </Tooltip>
                <button
                  type="button"
                  onClick={() => void submit()}
                  disabled={
                    (!draft.trim() && pendingAttachments.length === 0) ||
                    composerLocked ||
                    uploading
                  }
                  aria-label={t("groupChat.send")}
                  title={
                    turnLive
                      ? t("chatView.notice.turnInflightTooltip")
                      : t("groupChat.send")
                  }
                  className="rounded-md p-1.5 text-text-tertiary hover:bg-zinc-200 disabled:opacity-50 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200"
                >
                  <Send size={16} />
                </button>
              </div>
            </div>
          </div>
        )}
      </div>

      {activeGroupId && (
        <ChatHistoryDialog
          open={historyOpen}
          onClose={() => setHistoryOpen(false)}
          chatSessionId={activeGroupId}
          resolveName={(id) => nameOf(id).name}
        />
      )}
      {/* The "@name" menu — one for the whole panel, opened from any
          participant's avatar / name in the transcript. */}
      <ContextMenu
        isOpen={mentionMenu.isOpen}
        menuProps={mentionMenu.menuProps}
        items={mentionMenuItems}
        payload={mentionMenu.payload}
        selectionAtOpen={mentionMenu.selectionAtOpen}
        onClose={mentionMenu.close}
        compact
      />
    </div>
  );
}
