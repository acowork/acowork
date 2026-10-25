/**
 * GroupChatPanel — the middle-pane surface for a project group chat
 * (design 27 §4.4 / §5.3). Mounted by AppLayout when
 * `useProjectGroupStore.activeGroupId` is set.
 *
 * It is deliberately a thin twin of `AgentChatView`: same rows, same
 * bubble visuals, same card answering — but multi-party. The message
 * window lives in `chatSessionStore.windows[sessionId]` (the doorbell
 * keeps it fresh); this component only renders and sends.
 *
 * Send channels (§5.3):
 * - plain text  -> archive-only `user_message` row (agents stay quiet);
 * - `@Agent`    -> `/api/intents/mentions` fan-out (one row + turns);
 * - card answer -> `/api/intents/chat-message` with `in_reply_to`
 *   (the Gateway resolves the card's own agent — groups accepted).
 *
 * `ponytail:` ceilings vs the 1:1 surface — no model/workspace seed
 * toolbar (group turns are seeded by the mentioned agent's defaults),
 * no scroll-anchored pagination (a "load earlier" button instead). The
 * composer matches the 1:1 chat surface's attachment abilities: Paperclip
 * upload + Ctrl+V paste into the session's `files/` dir, ids forwarded on
 * the mention/archive send. Upgrade path: fold in the seed chips once the
 * mentions channel carries `session_config`.
 */

import { Children, isValidElement, useCallback, useEffect, useMemo, useRef, useState, type UIEvent } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { Send, Paperclip, Loader2, ChevronDown, ChevronsDown, ClipboardList } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { formatBubbleTime } from "../../lib/formatTime";
import { cn } from "../../lib/utils";
import { toolbarIconButton } from "../../lib/ui-styles";
import { getGatewayUrl } from "../../lib/config";
import { fetchDirectory } from "../../lib/auth-api";
import type { DirectoryUser } from "../../lib/types";
import { useStickToBottom } from "../../lib/stickToBottom";
import { Tooltip } from "../common/Tooltip";
import { AgentAvatar } from "../common/AgentAvatar";
import { UserAvatar } from "../common/UserAvatar";
import { ViewRunButton } from "./ViewRunButton";
import { useSessionTitle } from "../../lib/useSessionTitle";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { CodeBlock } from "./CodeBlock";
import { ChatAttachmentChip } from "./ChatAttachmentChip";
import { ExecAttachmentLabels } from "./ExecAttachmentLabels";
import { ChatNoticeEventRow, truncateTaskName } from "./ChatNoticeEventRow";
import { ChatTurnStatusLine } from "./ChatTurnStatusLine";
import {
  isFailureStatus,
  isEmptyTerminalReply,
  isTurnRoutedNotice,
  pendingTurn,
  terminalNoticeKey,
} from "./chatTurnState";
import { InlineInteractionCard } from "./InlineInteractionCard";
import { detectPaste } from "./pasteDetect";
import { handleChatMarkdownLinkClick, markdownUrlTransform } from "../editor/markdownLinkResolver";
import { useAuthStore } from "../../stores/authStore";
import { useAgentStore } from "../../stores/agentStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import {
  useChatSessionStore,
  closedCardIds,
  humanAnsweredCardIds,
  type ChatMessageRow,
  type ChatAttachmentMeta,
} from "../../stores/chatSessionStore";
import { useChatStore } from "../../stores/chatStore";
import { sessionExists } from "../../lib/session-control";
import { showToast } from "../common/ToastProvider";
import { ChatHistoryDialog } from "./ChatHistoryDialog";
import { OutlineChatHistoryIcon } from "../common/ChatHistoryIcon";
import { useLocateMessage } from "../../lib/useLocateMessage";

// Same markdown shell as AgentChatView's bubbles (kept local on purpose:
// extracting a shared component would touch two surfaces for one class
// list — do that when a third chat surface appears).
const chatMarkdownComponents = {
  p: ({ children }: { children?: React.ReactNode }) => (
    <p className="mb-2 text-sm leading-relaxed last:mb-0">{children}</p>
  ),
  pre: ({ children }: { children?: React.ReactNode }) => {
    const codeEl = Children.toArray(children).find(
      (child): child is React.ReactElement<{ className?: string; children?: React.ReactNode }> =>
        isValidElement(child) && child.type === "code",
    );
    if (codeEl) {
      const { className, children: codeContent } = codeEl.props;
      const language = className?.replace(/^language-/, "") || "";
      const code = Children.toArray(codeContent).join("");
      return <CodeBlock language={language} code={code} />;
    }
    return <pre>{children}</pre>;
  },
  code: ({ children }: { children?: React.ReactNode }) => (
    <code className="rounded bg-black/10 px-1 py-0.5 text-xs dark:bg-white/10">
      {children}
    </code>
  ),
  a: ({ href, children }: { href?: string; children?: React.ReactNode }) => {
    if (!href) return <span>{children}</span>;
    return (
      <a
        href={href}
        onClick={(e) => {
          e.preventDefault();
          handleChatMarkdownLinkClick(href);
        }}
        className="text-[var(--color-accent)] underline"
      >
        {children}
      </a>
    );
  },
  ul: ({ children }: { children?: React.ReactNode }) => (
    <ul className="mb-2 list-disc pl-5">{children}</ul>
  ),
  ol: ({ children }: { children?: React.ReactNode }) => (
    <ol className="mb-2 list-decimal pl-5">{children}</ol>
  ),
  li: ({ children }: { children?: React.ReactNode }) => (
    <li className="text-sm leading-relaxed">{children}</li>
  ),
  /** Same scroll wrapper as every other desktop markdown surface
   *  (`MessageBubble` / `CompactionCard` / `MarkdownPreviewView`): a GFM table
   *  cannot shrink below its widest cell, so unwrapped it widened the bubble
   *  instead of scrolling inside it. */
  table: ({ children, ...rest }: React.TableHTMLAttributes<HTMLTableElement>) => (
    <div className="prose-table-scroll">
      <table {...rest}>{children}</table>
    </div>
  ),
};

function ChatMarkdown({ content }: { content: string }) {
  return (
    // The prose shell is load-bearing, not cosmetic: every table rule in
    // globals.css (`.prose table` borders / rounded frame, `.prose th`
    // header shade, `.prose td` padding, the zebra-striping override) is
    // scoped under `.prose`. Without it remark-gfm still emits a real
    // <table>, but unstyled — the group-chat defect where an agent's
    // markdown table read as plain text while the same reply rendered
    // correctly in the execution session (`MessageBubble` / `AgentChatView`,
    // both wrapped in this exact class list). Keep in sync with
    // `AgentChatView`'s ChatMarkdown shell.
    <div className="prose prose-sm prose-zinc max-w-none prose-h1:text-lg prose-h2:text-base prose-h3:text-sm prose-h4:text-sm prose-headings:font-semibold select-text break-words [&_th]:bg-chat-title [&_td]:bg-chat-body [&_tbody_tr]:!bg-transparent">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={chatMarkdownComponents}
        urlTransform={markdownUrlTransform}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
}

// ── Participant naming ────────────────────────────────────────────────

/** Resolve a participant id to a display name + kind. Agents come from
 *  the local agent registry; humans from the account directory (fetched
 *  once per panel mount — it is small and bounded). Unknown id (agent on
 *  another machine, removed account) falls back to a short id. */
function useParticipantNamer(directory: DirectoryUser[]) {
  const agentsMap = useAgentStore((s) => s.agents);
  const dirById = useMemo(() => {
    const m = new Map<string, DirectoryUser>();
    for (const u of directory) m.set(u.user_id, u);
    return m;
  }, [directory]);
  return useCallback(
    (id: string): { name: string; isAgent: boolean } => {
      const agent = agentsMap[id]?.meta;
      if (agent) {
        return { name: agent.display_name ?? agent.name ?? id, isAgent: true };
      }
      const user = dirById.get(id);
      if (user) return { name: user.display_name || user.username, isAgent: false };
      return { name: id.slice(0, 8), isAgent: false };
    },
    [agentsMap, dirById],
  );
}

// ── Row renderer ──────────────────────────────────────────────────────

interface RowProps {
  row: ChatMessageRow;
  me: string;
  /** Chat session the row belongs to — attachment thumbnails are addressed
   *  through it (the `files/` dir is session-scoped, §4.2.1). */
  chatSessionId: string;
  nameOf: (id: string) => { name: string; isAgent: boolean };
  /** Cards that can no longer be answered (§5.5.4). */
  closed: Set<string>;
  /** The subset the human actually replied to — the line says "Answered",
   *  anything else that closed (turn ended, timeout, stop) says "Closed". */
  answered: Set<string>;
  onAnswer: (row: ChatMessageRow, text: string, immediate: boolean) => void;
  /** Drill into the exec session that produced an agent reply — jumps to that
   *  agent's 1:1 page and opens the exec tab (mirrors AgentChatView.jumpToExec). */
  onJumpToExec: (agentId: string, execSid: string) => void;
  /** Admin viewing a group they never joined: cards are shown, never
   *  answered — posting into someone else's project stays forbidden. */
  readOnly?: boolean;
  /** Body of the message that triggered this row's turn (`meta.in_reply_to`).
   *  Names the run in the drill-down tooltip while the exec session still has
   *  no title. Resolved by the parent through its message_id → body index. */
  triggerBody?: string;
}

/**
 * A PM task dispatch chip: the task id as a compact, copyable pill shown
 * above the human-readable dispatch text. The claim/submit playbook that
 * wakes the agent is model-facing only (injected into the execution
 * session's system block, never on this row) — the chip is the sole
 * machine coordinate the human surface needs. `project_id` is deliberately
 * absent: the row already lives in that project's own group chat.
 */
function TaskChip({ taskId }: { taskId: string }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const onCopy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(taskId);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable — the id is still visible as the label */
    }
  }, [taskId]);
  return (
    <button
      type="button"
      onClick={onCopy}
      title={copied ? undefined : t("groupChat.taskChipCopy")}
      className={cn(
        "inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-10 font-medium transition-colors",
        "border-border-outer bg-[var(--color-accent)]/10 text-text-secondary",
        "hover:bg-[var(--color-accent)]/20",
      )}
    >
      <ClipboardList className="h-3 w-3 shrink-0" aria-hidden />
      <span className="max-w-[14rem] truncate">{t("groupChat.taskChip", { id: taskId })}</span>
      {copied && <span aria-hidden>✓</span>}
    </button>
  );
}

function GroupRowView({ row, me, chatSessionId, nameOf, closed, answered, onAnswer, onJumpToExec, readOnly, triggerBody }: RowProps) {
  const { t } = useTranslation();
  const profile = useUserProfileStore((s) => s.profile);
  const agentsMap = useAgentStore((s) => s.agents);

  // The exec session behind this row and the agent that owns it (`row.from`).
  // Its name feeds the "view run" tooltip. Read before the kind branches so
  // the store hook stays unconditional. The routing reaction row (ADR-100
  // D11) names the task in its bubble, so it resolves through the same hook.
  const routed = isTurnRoutedNotice(row);
  const drillSid =
    row.kind === "agent_message" || routed
      ? row.meta?.agent_session_id || row.meta?.sender_session_id || ""
      : undefined;
  const execRunName = useSessionTitle(row.from, drillSid);

  if (row.kind === "system_notice") {
    if (routed) {
      // ADR-100 D11: the routing reaction — runtime-written, but it speaks as
      // the agent, so it is synthesized into the ordinary agent-bubble path
      // (avatar + name + bubble), never a centered system line. A user
      // message must always be followed by an agent message: that exchange IS
      // the chat.
      const taskName = execRunName?.trim() || truncateTaskName(triggerBody) || "";
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
      // as a quiet centered line, never on an agent's side.
      return <ChatNoticeEventRow row={row} className="px-4" />;
    }
  }

  if (row.kind === "interaction_request") {
    // The shared inline card — same grammar, affordances and answered/closed
    // receipt as the 1:1 surface (design 27 §5.5.4).
    return (
      <InlineInteractionCard
        row={row}
        closed={closed}
        answered={answered}
        askerName={nameOf(row.from).name}
        readOnly={readOnly}
        onAnswer={onAnswer}
      />
    );
  }

  const own = row.kind === "user_message" && row.from === me;
  const who = own
    ? { name: profile.displayName, isAgent: false }
    : nameOf(row.from);
  const failed = isFailureStatus(row.meta?.status);
  const agentMeta = who.isAgent ? agentsMap[row.from]?.meta : undefined;
  // The exec session behind an agent reply — a group / routed reply carries
  // `sender_session_id` (the answering agent's own session). Same formula as
  // AgentChatView, so finished / failed / empty-bodied replies are all
  // drillable.
  const execSid = row.meta?.agent_session_id || row.meta?.sender_session_id || "";

  // A terminal reply with nothing in it IS its outcome ("interrupted",
  // "failed"): paint the centered status line with the drill into the run,
  // never a bubble speaking in the agent's voice.
  if (row.kind === "agent_message" && isEmptyTerminalReply(row)) {
    return (
      <ChatTurnStatusLine
        className="px-4"
        text={t(terminalNoticeKey(row.meta?.status))}
        danger={failed}
        onJump={execSid ? () => onJumpToExec(row.from, execSid) : undefined}
        runName={execRunName?.trim() || truncateTaskName(triggerBody) || undefined}
      />
    );
  }

  // Avatar is a 40px side column (left for others, right for own) — the same
  // primitive the user↔user inbox (`MessageRow`) and user↔agent thread
  // (`AgentChatView`) use, so all three chat surfaces read as one layout:
  // name above the bubble, bubble beside the avatar, timestamp below the
  // bubble revealed on hover.
  const avatar = who.isAgent ? (
    <AgentAvatar
      agentId={row.from}
      displayName={who.name}
      avatarUrl={agentMeta?.avatar}
      version={agentMeta?.version}
      builtinAvatarId={agentMeta?.builtin_avatar ?? null}
      size={40}
      className="mt-1 shrink-0"
    />
  ) : own ? (
    <UserAvatar
      displayName={profile.displayName}
      avatarUrl={profile.backendAvatarUrl ?? null}
      builtinAvatarId={profile.backendBuiltinAvatarId ?? null}
      size={40}
      className="mt-1 shrink-0"
    />
  ) : (
    <UserAvatar
      displayName={who.name}
      avatarUrl={null}
      builtinAvatarId={null}
      size={40}
      className="mt-1 shrink-0"
    />
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
            <span className="text-xs text-text-tertiary">{who.name}</span>
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
            own ? "items-end" : "items-start",
          )}
        >
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
                <ChatAttachmentChip key={a.id} chatSessionId={chatSessionId} attachment={a} />
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
            labels={row.meta?.exec_attachments ?? []}
            align={own ? "end" : "start"}
          />
          {/* A PM task dispatch: the task id rides a chip, not the body —
              the human text stays clean and the claim/submit playbook never
              reaches this surface at all. */}
          {row.kind === "user_message" && row.meta?.task_id && (
            <div className={cn("mb-1 flex flex-wrap gap-1.5", own ? "justify-end" : "justify-start")}>
              <TaskChip taskId={row.meta.task_id} />
            </div>
          )}
          {/* The group column is an `items-start` flex column, so the bubble is
             shrink-to-fit too: without the clamp, content that cannot be
             squeezed (a mermaid SVG's natural width, an unwrapped table, one
             long token) widened the bubble past the row's `max-w-[var(
             --content-max-width)]` and spilled under the next panel. The clamp
             hands that overflow to the inner scroll containers. */}
          {(row.body || !(row.attachments?.length || row.meta?.exec_attachments?.length)) && (
          <div
            className={cn(
              "max-w-full rounded-md px-4 py-2.5 leading-relaxed break-words select-text",
              own
                ? "rounded-br-sm bg-chat-user text-chat-user-text"
                : "rounded-bl-sm bg-chat-bubble text-text-primary",
            )}
          >
            {failed && (
              <div className="mb-1 rounded bg-red-50 px-2 py-0.5 text-10 text-red-500 dark:bg-red-900/20">
                {row.meta?.status}
              </div>
            )}
            {row.body ? <ChatMarkdown content={row.body} /> : (
              <span className="text-xs text-text-tertiary italic">{t("groupChat.emptyTurn")}</span>
            )}
          </div>
          )}
          <div className="mt-1 flex items-center gap-2">
            {/* Drill-into-exec affordance — the group twin of the 1:1 chat
                surface's button: a discrete control under the bubble, not a
                whole-bubble click, so selecting the reply text never fires a
                jump. Present on every local agent reply with a resolved exec
                session, empty body included. Same brain glyph as the exec
                tabs (design 27 §6.1) — the button and its destination are
                the same thing. */}
            {!own && who.isAgent && row.kind === "agent_message" && execSid && (
              <ViewRunButton
                runName={execRunName?.trim() || truncateTaskName(triggerBody) || undefined}
                onJump={() => onJumpToExec(row.from, execSid)}
              />
            )}
            <span className="text-10 text-text-tertiary opacity-0 transition-opacity group-hover:opacity-100">
              {formatBubbleTime(row.ts * 1000)}
            </span>
          </div>
        </div>
      </div>
      {own && avatar}
    </div>
  );
}

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
  const win = useChatSessionStore(
    (s) => (activeGroupId ? s.windows[activeGroupId] : undefined),
  );
  const sending = useChatSessionStore((s) => (activeGroupId ? s.sending[activeGroupId] : false));
  const pendingLocateSeq = useChatSessionStore((s) =>
    activeGroupId ? (s.windows[activeGroupId]?.pendingLocateSeq ?? null) : null,
  );

  const [historyOpen, setHistoryOpen] = useState(false);
  // Scroll viewport, captured alongside `useStickToBottom`'s callback ref so
  // the locate effect can query `[data-seq]` rows within it.
  const containerRef = useRef<HTMLDivElement | null>(null);

  const [draft, setDraft] = useState("");
  const [directory, setDirectory] = useState<DirectoryUser[]>([]);
  const nameOf = useParticipantNamer(directory);
  const agentsMap = useAgentStore((s) => s.agents);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  // Composer attachments — uploaded into the group's `files/` dir (§4.2.1)
  // before send; the send carries their ids so the durable row and each
  // mentioned agent's execution turn both see them (mirrors AgentChatView).
  const [pendingAttachments, setPendingAttachments] = useState<ChatAttachmentMeta[]>([]);
  const [uploading, setUploading] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);

  // @-mention autocomplete: the active token under the caret + which popup
  // row is highlighted (keyboard nav). `null` = caret is not inside a mention.
  const [mention, setMention] = useState<{ query: string; start: number } | null>(null);
  const [mentionActive, setMentionActive] = useState(0);
  const pickerRef = useRef<HTMLDivElement>(null);

  const { ref: scrollRef, stickNow, atBottom } = useStickToBottom(
    [win?.tail.length ?? 0, activeGroupId],
    activeGroupId,
    () => {},
    win?.older.length ?? 0,
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

  // Locate-and-flash for history jumps (ADR-097) — declared AFTER the follow
  // hook so the center-scroll wins over follow-to-bottom on the jump commit.
  const highlightSeq = useLocateMessage(
    () => containerRef.current,
    pendingLocateSeq,
    () => activeGroupId && useChatSessionStore.getState().clearLocate(activeGroupId),
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
  const rows = useMemo(() => [...(win?.older ?? []), ...(win?.tail ?? [])], [win]);
  // message_id → body index over the raw window, so a reply (whose trigger is
  // only an `in_reply_to` id) can name the task in its drill-down tooltip when
  // the run has no title yet.
  const bodyById = useMemo(() => {
    const m = new Map<string, string>();
    for (const r of [...(win?.older ?? []), ...(win?.tail ?? [])]) {
      if (r.meta?.message_id) m.set(r.meta.message_id, r.body);
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
    if (hasMoreNewer && !win.loadingNewer && el.scrollHeight - el.scrollTop - el.clientHeight <= 48) {
      void useChatSessionStore.getState().loadNewer(activeGroupId);
    }
  };
  const closed = useMemo(() => (win?.tail ? closedCardIds(win.tail) : new Set<string>()), [win]);
  const answered = useMemo(
    () => (win?.tail ? humanAnsweredCardIds(win.tail) : new Set<string>()),
    [win],
  );
  const agentParticipants = useMemo(() => {
    if (!group?.participants) return [];
    return group.participants
      .filter((p) => p.id !== me && (p.identity === "agent" || !p.identity))
      .map((p) => ({ id: p.id, name: nameOf(p.id).name }))
      .filter((p) => nameOf(p.id).isAgent);
  }, [group, me, nameOf]);

  // Is the newest message still waiting for an answering agent? Derived, never
  // stored (`chatTurnState`). Suppressed while a locate has the window parked
  // mid-history (no newest message is rendered there).
  //
  // `agentParticipants.length > 0` is the chat-side fact that makes the line
  // meaningful at all: a `user_user` DM has no agent, so it must never claim
  // somebody is working — not even while the send is in flight. Rows are
  // filtered the same way inside `pendingTurn`, so an un-@'d group remark
  // opens no wait either (it woke nobody), while a turn an agent is still
  // chewing on keeps its line even if a plain remark landed after it.
  const waiting =
    !hasMoreNewer && pendingTurn(rows, sending, closed, agentParticipants.length > 0);

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
        return { id: p.id, name: who.name, isAgent: who.isAgent };
      })
      .filter((p) => p.name);
  }, [group, me, nameOf]);

  // Candidates for the open popup: mentionable participants narrowed by the
  // typed query.
  const mentionCandidates = useMemo(
    () => (mention ? filterMentionCandidates(mentionParticipants, mention.query) : []),
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

  const syncMention = useCallback((value: string, caret: number) => {
    setMention(detectMention(value, caret));
    setMentionActive(0);
  }, []);

  const pickMention = useCallback(
    (name: string) => {
      const ta = textareaRef.current;
      if (!ta || !mention) return;
      const caret = ta.selectionStart ?? draft.length;
      const { value, caret: nextCaret } = applyMention(draft, caret, mention.start, name);
      setDraft(value);
      setMention(null);
      requestAnimationFrame(() => {
        ta.focus();
        ta.setSelectionRange(nextCaret, nextCaret);
      });
    },
    [draft, mention],
  );

  const submit = useCallback(async () => {
    const body = draft.trim();
    const attachmentIds = pendingAttachments.map((a) => a.id);
    if ((!body && attachmentIds.length === 0) || !activeGroupId || uploading) return;
    setDraft("");
    const sentAttachments = pendingAttachments;
    setPendingAttachments([]);
    stickNow();
    const targets = resolveMentions(body, agentParticipants);
    const ok = await useChatSessionStore
      .getState()
      .sendGroupText(activeGroupId, body, targets, undefined, attachmentIds);
    // Refund the composer on failure so nothing typed or attached is lost.
    if (!ok) {
      setDraft(body);
      setPendingAttachments(sentAttachments);
    }
  }, [draft, activeGroupId, agentParticipants, stickNow, pendingAttachments, uploading]);

  // Upload each chosen file into the group's `files/` dir and stack a chip.
  // Shared by the paperclip picker and the paste handler (§4.2.1).
  const uploadFiles = useCallback(async (files: File[]) => {
    if (files.length === 0 || !activeGroupId) return;
    setUploading(true);
    try {
      for (const f of files) {
        const meta = await useChatSessionStore.getState().upload(activeGroupId, f);
        setPendingAttachments((prev) => [...prev, meta]);
      }
    } catch (err) {
      console.warn("[GroupChatPanel] attachment upload failed:", err);
    } finally {
      setUploading(false);
    }
  }, [activeGroupId]);

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
            files.push(new File([blob], name, { type: blob.type || "application/octet-stream" }));
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

  const onAnswer = useCallback(
    (row: ChatMessageRow, text: string, _immediate: boolean) => {
      if (!activeGroupId) return;
      stickNow();
      void useChatSessionStore
        .getState()
        .sendGroupText(activeGroupId, text, [], row.meta?.message_id);
      // Optimistically dismiss the card in the execute session view
      const execSid = row.meta?.agent_session_id;
      const reqId = row.meta?.interaction?.request_id;
      if (execSid && reqId && row.from) {
        useChatStore.getState().resolveQuestion(row.from, reqId, execSid);
      }
    },
    [activeGroupId, stickNow],
  );

  // Jump to the exec session behind a group agent reply: select that agent
  // (closes the group in the middle pane, same as picking it in the sidebar),
  // cold-load the exec tab, flip the surface to exec. Mirrors AgentChatView.
  // A deleted exec session is answered with a toast and no navigation — the
  // same probe AgentChatView.jumpToExec runs (the message's exec pointer
  // outlives the session it names, §5.5.5).
  const jumpToExec = useCallback(async (agentId: string, execSid: string) => {
    if (!agentId || !execSid) return;
    if (!(await sessionExists(agentId, execSid))) {
      showToast({ type: "info", message: t("chatView.execSessionGone") });
      return;
    }
    useAgentStore.getState().selectAgent(agentId);
    await useChatStore.getState().openSession(agentId, execSid);
    useChatSessionStore.getState().setMode(agentId, "exec");
  }, [t]);

  if (!activeGroupId) return null;

  const participants = group?.participants ?? [];
  // A `user_user` pair is the smallest group — two people. Render it as a DM:
  // the header shows the peer's name (not the empty session title) and drops
  // the "N members" chip, which is noise for a conversation of exactly two.
  const isDm = group?.kind === "user_user";
  const peerId = isDm ? participants.find((p) => p.id !== me)?.id : undefined;
  const title = isDm
    ? (peerId ? nameOf(peerId).name : activeGroupId.slice(0, 8))
    : group?.title || activeGroupId.slice(0, 8);
  // Admin cross-account view: the panel lists groups the admin never joined.
  // Reading is allowed (server-side admin bypass), posting is not — hide the
  // composer and card actions for anyone who isn't a participant.
  const canPost = participants.some((p) => p.id === me);

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
        {rows.length === 0 && (
          <div className="flex h-full items-center justify-center text-xs text-text-tertiary">
            {t("groupChat.empty")}
          </div>
        )}
        {rows.map((row) => (
          <div
            key={`${row.seq}-${row.meta?.message_id ?? ""}`}
            data-seq={row.seq}
            className={cn(
              "scroll-mt-2 transition-colors",
              highlightSeq === row.seq && "bg-[var(--color-accent)]/10",
            )}
          >
            <GroupRowView
              row={row}
              me={me}
              chatSessionId={activeGroupId}
              nameOf={nameOf}
              closed={closed}
              answered={answered}
              onAnswer={onAnswer}
              onJumpToExec={(aid, sid) => void jumpToExec(aid, sid)}
              triggerBody={
                row.meta?.in_reply_to ? bodyById.get(row.meta.in_reply_to) : undefined
              }
              readOnly={!canPost}
            />
          </div>
        ))}
        {/* One aggregated "an agent has your message" line, derived from the
            newest unanswered row — the group twin of the 1:1 waiting line. No
            agent is named: a fan-out can wake more than one, and the reply
            bubbles say who answered. */}
        {waiting && (
          <ChatTurnStatusLine className="px-4" live text={t("chatView.notice.pendingTurnGeneric")} />
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
                onClick={() => activeGroupId && void useChatSessionStore.getState().loadNewer(activeGroupId)}
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
            {/* @-mention picker — floats above the composer card (standard IM
                behavior). `onMouseDown preventDefault` keeps the textarea
                focused so selecting a row does not blur it first. */}
            {mention && mentionCandidates.length > 0 && (
              <div
                onMouseDown={(e) => e.preventDefault()}
                className="absolute bottom-full left-3 z-20 mb-2 w-64 overflow-hidden rounded-lg border border-border-outer bg-modal-surface shadow-xl"
              >
                <OverlayScrollbar
                  ref={pickerRef}
                  role="listbox"
                  aria-label={t("groupChat.mentionLabel")}
                  className="py-1"
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
                      avatarUrl={null}
                      builtinAvatarId={null}
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
                      onClick={() => pickMention(c.name)}
                      onMouseEnter={() => setMentionActive(i)}
                      className={cn(
                        "flex w-full items-center gap-2 px-2 py-1.5 text-left text-xs",
                        i === mentionActive
                          ? "bg-nav-item-hover text-text"
                          : "text-text-secondary",
                      )}
                    >
                      {avatar}
                      <span className="min-w-0 flex-1 truncate">{c.name}</span>
                    </button>
                  );
                })}
                </OverlayScrollbar>
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
                  syncMention(ta.value, ta.selectionStart ?? ta.value.length);
                }}
                onKeyDown={(e) => {
                  // While the picker is open it owns Enter / arrows / Tab / Esc
                  // — Enter picks the highlighted member instead of sending.
                  if (mention && mentionCandidates.length > 0) {
                    if (e.key === "ArrowDown") {
                      e.preventDefault();
                      setMentionActive((i) => (i + 1) % mentionCandidates.length);
                      return;
                    }
                    if (e.key === "ArrowUp") {
                      e.preventDefault();
                      setMentionActive(
                        (i) => (i - 1 + mentionCandidates.length) % mentionCandidates.length,
                      );
                      return;
                    }
                    if (e.key === "Enter" || e.key === "Tab") {
                      e.preventDefault();
                      const pick = mentionCandidates[mentionActive] ?? mentionCandidates[0];
                      if (pick) pickMention(pick.name);
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
                disabled={sending}
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
              <div data-toolbar-cluster="right" className="flex shrink-0 items-center gap-1">
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
                  disabled={(!draft.trim() && pendingAttachments.length === 0) || sending || uploading}
                  aria-label={t("groupChat.send")}
                  title={t("groupChat.send")}
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
    </div>
  );
}
