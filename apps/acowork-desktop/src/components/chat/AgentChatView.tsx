// Design 27 Wave 5: the Chat session surface — the Agent page's primary
// view. One Chat session: durable rows (messages / decision cards /
// system notices) from the user service, the in-flight turn projected at
// read time (§6.2), and a composer that is the SAME visual language as the
// execution view (same container, same textarea, same toolbar components —
// plan 5.4: "输入框能力与执行视图一致").
//
// Deliberate deltas from the execution composer, all of them semantic:
// - ModelMenu / ReasoningEffortMenu / WorkspaceSelector are WRITABLE, but a
//   pick is a `session_config` DRAFT held on the Chat session ROW (design 27
//   §4.5) — the store only caches what the server returned: `setCurrentModel`
//   needs an active execution session, and a Chat turn's session is created
//   server-side when the intent lands. The draft rides `sendText` and seeds
//   that session at creation (or rotates it via T3 if one is already running
//   under a different config) — so the choice is fixed BEFORE the turn, not
//   after, which is the whole point (pick model/workspace/reasoning first).
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
// The status card is the drill-down INTO the execution view (plan 5.3):
// click → `chatStore.openSession(agentId, execSid)` + mode flip.

import { Children, isValidElement, useEffect, useMemo, useRef, useState, type UIEvent } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { useTranslation } from "../../i18n/useTranslation";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Send, Paperclip, Loader2, ChevronDown, ChevronsDown } from "lucide-react";
import { formatBubbleTime } from "../../lib/formatTime";
import { cn } from "../../lib/utils";
import { toolbarIconButton } from "../../lib/ui-styles";
import { useStickToBottom } from "../../lib/stickToBottom";
import { Tooltip } from "../common/Tooltip";
import { UserAvatar } from "../common/UserAvatar";
import { AgentAvatar } from "../common/AgentAvatar";
import { OutlineBrainIcon } from "../common/BrainIcon";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { CodeBlock } from "./CodeBlock";
import { ChatAttachmentChip } from "./ChatAttachmentChip";
import { detectPaste } from "./pasteDetect";
import { handleChatMarkdownLinkClick, markdownUrlTransform } from "../editor/markdownLinkResolver";
import { ModelMenu, ReasoningEffortMenu } from "./ChatPanel";
import { WorkspaceSelector } from "../workspace/WorkspaceSelector";
import { ExecSessionStatusCard } from "./ExecSessionStatusCard";
import { useLocateMessage } from "../../lib/useLocateMessage";
import { useChatSessionStore, projectInFlight, projectLiveTurn, type ChatMessageRow, type ChatSessionConfig, type ChatAttachmentMeta } from "../../stores/chatSessionStore";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { sessionExists } from "../../lib/session-control";
import { showToast } from "../common/ToastProvider";
import { useAuthStore } from "../../stores/authStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { getGatewayUrl } from "../../lib/config";

/** i18n key for an empty agent bubble, read off the turn's §7.1 outcome.
 *
 *  A stopped run (`aborted`, `timeout`) genuinely produced no output; a run
 *  that answered `completed` with an empty body said nothing, which is a
 *  different fact and used to be labelled the same way. `failed` keeps the
 *  interrupted wording — its body is the error summary, so this path is only
 *  a fallback. Rows predating the outcome field carry no status and keep the
 *  historical guess. */
export function emptyBubbleKey(status?: string): string {
  return status === "completed" || status === "waiting"
    ? "chatView.emptyAnswer"
    : "chatView.emptyReply";
}

/** Strip common leading whitespace from multi-line strings (same helper
 *  the execution bubbles use for indented code blocks). */
function dedent(code: string): string {
  const lines = code.split("\n");
  const nonEmpty = lines.filter((l) => l.trim().length > 0);
  if (nonEmpty.length === 0) return code.trim();
  const minIndent = Math.min(...nonEmpty.map((l) => l.match(/^ */)?.[0].length ?? 0));
  return lines.map((l) => l.slice(minIndent)).join("\n").trim();
}

// ── Markdown in chat bubbles — same overrides as the execution view,
//    minimal set (code + safe links). ──────────────────────────────────
const chatMarkdownComponents = {
  pre: ({ children }: { children?: React.ReactNode }) => {
    const childArray = Children.toArray(children);
    const codeEl = childArray.find(
      (child): child is React.ReactElement<{ className?: string; children?: React.ReactNode }> =>
        isValidElement(child) && child.type === "code",
    );
    if (codeEl) {
      const { className, children: codeContent } = codeEl.props;
      const language = className?.replace(/^language-/, "") || "";
      const code = dedent(Children.toArray(codeContent).join(""));
      return <CodeBlock language={language} code={code} />;
    }
    return <pre>{children}</pre>;
  },
  a: ({ href, children, node: _node, ...rest }: React.AnchorHTMLAttributes<HTMLAnchorElement> & { node?: unknown }) => {
    if (!href) return <span>{children}</span>;
    return (
      <a
        {...rest}
        href={href}
        onClick={(e) => {
          e.preventDefault();
          handleChatMarkdownLinkClick(href);
        }}
      >
        {children}
      </a>
    );
  },
};

function ChatMarkdown({ content }: { content: string }) {
  return (
    <div className="prose prose-sm prose-zinc max-w-none prose-h1:text-lg prose-h2:text-base prose-h3:text-sm prose-h4:text-sm prose-headings:font-semibold select-text break-words [&_th]:bg-chat-title [&_td]:bg-chat-body [&_tbody_tr]:!bg-transparent">
      <ReactMarkdown remarkPlugins={[remarkGfm]} urlTransform={markdownUrlTransform} components={chatMarkdownComponents}>
        {content}
      </ReactMarkdown>
    </div>
  );
}

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
   *  finished, failed or empty-bodied alike (§5.5.5: the pointer survives
   *  rotation, the JSONL stays in Agent home). */
  onJumpToExec: (execSid: string) => void;
  /** For a `router_fallback` receipt: the exec session its trigger message was
   *  routed into (resolved through the window's `turn_started` pointer). When
   *  present the receipt is a clickable anchor, not just a line of text. */
  routerExecSid?: string;
}

function RowView({ row, agentId, chatSessionId, showHeader, onJumpToExec, routerExecSid }: RowProps) {
  const { t } = useTranslation();
  const agent = useAgentStore((s) => s.agents[agentId]?.meta);
  const profile = useUserProfileStore((s) => s.profile);
  const agentName = agent?.display_name ?? agent?.name ?? "";

  if (row.kind === "system_notice") {
    // `turn_started` is rendered as the status card, never as a row; other
    // notices (receipts) are a quiet centered line.
    if (row.meta?.notice_type === "turn_started") return null;
    // ADR-099: the router degraded to the deterministic route. The message
    // itself still went through — a receipt, not an error. Once routing
    // landed the receipt is an anchor: clicking drills into the execution
    // session the message was assigned to.
    if (row.meta?.notice_type === "router_fallback") {
      const text = t("chatView.routerFallback");
      if (!routerExecSid) {
        return <div className="py-1 text-center text-10 text-text-tertiary">{text}</div>;
      }
      return (
        <div className="py-1 text-center">
          <button
            type="button"
            onClick={() => onJumpToExec(routerExecSid)}
            className="text-10 text-text-tertiary underline-offset-2 transition-colors hover:text-text-secondary hover:underline"
          >
            {text}
          </button>
        </div>
      );
    }
    const text = row.body || row.meta?.status || "";
    return (
      <div className="py-1 text-center text-10 text-text-tertiary">{text}</div>
    );
  }

  if (row.kind === "interaction_request") {
    // Every execute-session projection (ask_question / approval / pause) is
    // rendered through the unified ExecSessionStatusCard, not as a standalone
    // row — so the durable card row never paints here. An unanswered card is
    // surfaced live by the status card (bound to its exec session); an
    // answered one is intentionally left with no orphan "已回答" line (§ 5.5.4,
    // answer-then-done).
    return null;
  }

  if (row.kind === "agent_message") {
    // §7.1 outcomes: `failed` / `timeout` are errors and wear the error
    // badge; `aborted` is the user stopping a run — a fact of the transcript,
    // not an error, so it must not paint red (it used to arrive as an empty
    // `completed`, which is why stopping looked like a blank reply).
    const failed = row.meta?.status === "failed" || row.meta?.status === "timeout";
    // The execution session that produced this reply: a 1:1 agent message
    // carries it as `sender_session_id` (the responder's own session); a
    // group / routed reply carries `agent_session_id` (routing-resolved).
    // Either way every agent message is drillable, finished or empty alike.
    const execSid = row.meta?.agent_session_id || row.meta?.sender_session_id || "";
    return (
      <div className="px-6 py-1">
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
              <span className="text-xs font-medium text-text-tertiary ">{agentName}</span>
              {agent?.role && (
                <span className="text-10 leading-tight text-text-tertiary ">{agent.role}</span>
              )}
            </div>
          </div>
        )}
        <div className="flex items-start gap-2">
          <div className="w-10 shrink-0" />
          {/* The percentage line-length clamp belongs on this shrink-wrap
              group (resolved against the wide row), exactly like the user
              row's `max-w-[85%]`. On the bubble itself it double-clamps:
              the group hugs the text, then 70%-of-that cuts the bubble
              narrower than its own content and the text overflows right. */}
          <div className="group min-w-0 max-w-[var(--content-max-width)]">
            {/* `inline-block`: the bubble hugs its text (symmetric padding,
                no width jump when the hover timestamp widens the group). */}
            {/* Attachment chips above the bubble, left-aligned — the mirror
                of the user row; the bubble never wraps them. An attachments-
                only reply paints chips alone, no empty bubble. */}
            {row.attachments && row.attachments.length > 0 && (
              <div className="mb-1 flex flex-wrap gap-1.5">
                {row.attachments.map((a) => (
                  <ChatAttachmentChip key={a.id} chatSessionId={chatSessionId} attachment={a} />
                ))}
              </div>
            )}
            {(row.body || failed || !row.attachments?.length) && (
              <div className="inline-block rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5  select-text break-words">
                {failed && (
                  <div className="mb-1 rounded bg-red-50 px-2 py-0.5 text-10 text-red-500 dark:bg-red-900/20">
                    {row.meta?.status}
                  </div>
                )}
                {row.body && <ChatMarkdown content={row.body} />}
                {/* The write-back records which outcome ended the turn (§7.1),
                    so the placeholder no longer has to guess from an empty
                    body: a stopped run says "interrupted", an answered one that
                    had nothing to say says "empty reply". Rows written before
                    the outcome existed carry no status and keep the old
                    wording. */}
                {!row.body && !row.attachments?.length && (
                  <span className="italic text-text-tertiary whitespace-nowrap">
                    {t(emptyBubbleKey(row.meta?.status))}
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
                <button
                  type="button"
                  onClick={() => onJumpToExec(execSid)}
                  title={t("chatView.viewRun")}
                  className="inline-flex shrink-0 items-center gap-1 text-10 text-text-tertiary transition-colors hover:text-[var(--color-accent)]"
                >
                  <OutlineBrainIcon className="h-[11px] w-[11px] shrink-0" />
                  {t("chatView.viewRun")}
                </button>
              )}
              <span className="w-0 shrink overflow-hidden whitespace-nowrap text-10 text-text-tertiary opacity-0 transition-opacity group-hover:w-auto group-hover:opacity-100">
                {formatBubbleTime(row.ts * 1000)}
              </span>
            </div>
          </div>
        </div>
      </div>
    );
  }

  // user_message (and anything else) — the human side, right-aligned.
  return (
    <div className="px-6 py-1">
      <div className="flex items-start justify-end gap-2">
        <div className="min-w-0 flex-1 flex flex-col items-end">
          {profile.displayName && (
            <span className="mt-[2px] text-xs text-text-tertiary ">{profile.displayName}</span>
          )}
          <div className="group mt-[6px] max-w-[85%] flex flex-col items-start">
            {/* Attachment chips — independent units above the text bubble,
                right-aligned: same grammar as the execution session's
                `UserWithAttachmentsBubble`. The bubble never wraps them. */}
            {row.attachments && row.attachments.length > 0 && (
              <div className="mb-1 flex flex-wrap justify-end gap-1.5 self-end">
                {row.attachments.map((a) => (
                  <ChatAttachmentChip key={a.id} chatSessionId={chatSessionId} attachment={a} />
                ))}
              </div>
            )}
            {row.body && (
              <div className="rounded-md rounded-br-sm bg-chat-user text-chat-user-text select-text">
                <div className="px-4 py-2.5 whitespace-pre-wrap break-words">{row.body}</div>
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
}

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
  const { sendText, loadOlder, loadNewer, clearLocate, setMode, saveDraft, upload } =
    useChatSessionStore.getState();

  const [input, setInput] = useState("");
  const [answerTarget, setAnswerTarget] = useState<ChatMessageRow | null>(null);
  // Composer attachments — uploaded into the Chat session's `files/` dir
  // (§4.2.1) before send; the send carries their ids so the durable row and
  // the execution turn both see them.
  const [pendingAttachments, setPendingAttachments] = useState<ChatAttachmentMeta[]>([]);
  const [uploading, setUploading] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const lastCompositionEndRef = useRef(0);
  // The scroll viewport, captured alongside `useStickToBottom`'s callback ref
  // so the locate effect can query `[data-seq]` rows within it.
  const containerRef = useRef<HTMLDivElement | null>(null);

  // The agent's current model, read-only here (see header comment).
  const [agentModel, setAgentModel] = useState<{ model: string; provider: string } | null>(null);
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const token = useAuthStore.getState().accessToken;
        const resp = await fetch(`${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/model`, {
          headers: token ? { Authorization: `Bearer ${token}` } : {},
        });
        if (resp.ok && !cancelled) {
          const data = (await resp.json()) as { model: string; provider: string };
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

  const rows = useMemo(
    () => (win ? [...win.older, ...win.tail] : []),
    [win],
  );
  // `in_reply_to → agent_session_id` over the window's `turn_started`
  // pointer rows — the anchor map receipts and other rows drill through
  // (§5.5.5: the pointer is written at routing time, so it exists while the
  // turn is still queued on a busy session).
  const pointerSid = useMemo(() => {
    const m = new Map<string, string>();
    for (const r of rows) {
      if (
        r.kind === "system_notice" &&
        r.meta?.notice_type === "turn_started" &&
        r.meta?.in_reply_to &&
        r.meta?.agent_session_id
      ) {
        m.set(r.meta.in_reply_to, r.meta.agent_session_id);
      }
    }
    return m;
  }, [rows]);
  // The live round — one projection for the status card, drilled from the
  // rows themselves so the card reacts to the window the user is looking at
  // rather than to the last refresh tick (§5.5.5).
  const live = useMemo(() => projectLiveTurn(rows), [rows]);
  // ADR-099: this is a 1:1 surface — every message reaches the agent, so a
  // trailing `user_message` whose pointer has not landed yet is the router's
  // assigning window, not a dead row. The card paints "assigning…" for it and
  // switches to the live mirror the moment `turn_started` arrives.
  const inFlight = useMemo(
    () => projectInFlight(rows, { awaitPointer: false }) ?? live.inFlight,
    [rows, live.inFlight],
  );
  // The latest `interaction_request` card, if it is still answerable — this
  // is the execute-session projection the unified status card mirrors live
  // (ask_question / approval / pause). A closed card (or none) yields `null`,
  // so nothing lingers in the stream (§ 5.5.4: answer-then-done).
  const pendingCard = live.pendingCard;

  // ADR-099: optimistic title for a Chat-Router exec session. That session has
  // no list row and usually no open tab, so its title would otherwise read
  // "Untitled" until the backend's generated title arrives over MQTT. Seed a
  // provisional title from the triggering user message the moment
  // `turn_started` resolves the exec session; `seedOptimisticTitle` no-ops
  // once any title exists, so the backend's authoritative (possibly
  // LLM-refined) push always wins afterwards. Guarded per execSid so it
  // fires at most once per session.
  const seededTitleRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    const execSid = inFlight?.execSid;
    if (!execSid || seededTitleRef.current.has(execSid)) return;
    const triggerId = inFlight?.triggerId;
    const body = triggerId
      ? rows.find((r) => r.meta?.message_id === triggerId)?.body
      : undefined;
    // Only pin the execSid once a body is available — the pointer can land
    // a window before the trigger row is readable, and marking it early would
    // permanently skip the seed. `seedOptimisticTitle` itself is idempotent
    // (no-ops once a title exists), so a re-run on a later `rows` tick is safe.
    if (!body) return;
    seededTitleRef.current.add(execSid);
    useChatStore.getState().seedOptimisticTitle(agentId, execSid, body);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [inFlight?.execSid, inFlight?.triggerId, agentId, rows]);

  // Follow rule shared by every chat surface (AgentChatView / GroupChatPanel /
  // InboxPanel): stick to the newest row only while pinned, and never let a
  // refresh (poll tick, re-render with a fresh object) yank an upward-scrolling
  // reader back down. Deps key on `tail.length` — the appended newest page —
  // NOT the total rows, so `loadOlder` (which prepends into `older`) leaves the
  // trigger unchanged and does not jump to the bottom; browser scroll
  // anchoring (`overflow-anchor: auto` on the viewport) holds the visible rows
  // put as older content appears above them, recomputing only the thumb.
  const { ref: scrollRef, stickNow, atBottom } = useStickToBottom(
    [win?.tail.length ?? 0, sessionId],
    sessionId,
    undefined,
    win?.older.length ?? 0,
  );

  // Locate-and-flash for history jumps (ADR-097): declared AFTER the follow
  // hook so the center-scroll wins over follow-to-bottom on the jump commit.
  const highlightSeq = useLocateMessage(
    () => containerRef.current,
    pendingLocateSeq,
    () => clearLocate(sessionId),
  );

  const handleAnswer = (card: ChatMessageRow, text: string, immediate: boolean) => {
    if (immediate && text) {
      stickNow();
      void sendText(sessionId, text, card.meta?.message_id);
      return;
    }
    setAnswerTarget(card);
    textareaRef.current?.focus();
  };

  const handleSend = () => {
    const text = input.trim();
    const attachmentIds = pendingAttachments.map((a) => a.id);
    if ((!text && attachmentIds.length === 0) || sending || uploading) return;
    const replyTo = answerTarget?.meta?.message_id;
    // A card answer continues the ORIGINAL turn deterministically (it does
    // not go through the Chat Router), so no seed is applied — carrying a
    // draft there would be meaningless. Plain messages carry the draft;
    // per ADR-099 the router-side seed lands on session creation and, for
    // model/reasoning, on reuse of an existing session (workspace is only
    // applied to a newly created one).
    const sessionConfig = replyTo ? undefined : (Object.keys(draft).length > 0 ? draft : undefined);
    setInput("");
    setAnswerTarget(null);
    setPendingAttachments([]);
    stickNow();
    void sendText(sessionId, text, replyTo, sessionConfig, attachmentIds);
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
            files.push(new File([blob], name, { type: blob.type || "application/octet-stream" }));
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

  const drillIntoExec = async () => {
    if (!inFlight?.execSid) return;
    await useChatStore.getState().openSession(agentId, inFlight.execSid);
    setMode(agentId, "exec");
  };

  // Jump to the execution session behind a specific agent message — the same
  // drill the in-flight status card does, but for a resolved turn (finished,
  // failed or empty). openSession cold-loads the exec tab; the pointer stays
  // valid after rotation because the old JSONL remains in Agent home (§5.5.5).
  // A DELETED session would otherwise cold-open as an empty exec tab whose
  // every load fails into the retry banner, so probe existence first: a
  // definitive 404 is answered with a toast and no navigation.
  const jumpToExec = async (execSid: string) => {
    if (!execSid) return;
    if (!(await sessionExists(agentId, execSid))) {
      showToast({ type: "info", message: t("chatView.execSessionGone") });
      return;
    }
    await useChatStore.getState().openSession(agentId, execSid);
    setMode(agentId, "exec");
  };

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
  // has no session to read `reasoningEffort` off): show the effort control
  // only when the selected model supports reasoning, defaulting the label to
  // that model's own default until the user picks a level.
  const currentModelEntry =
    availableModels.find(
      (m) => m.name === currentModelId && m.provider === currentProviderId,
    ) ??
    availableModels.find((m) => m.name === currentModelId) ??
    null;
  const reasoningSupported = !!currentModelEntry?.reasoning;
  const reasoningEffort =
    draft.reasoning_effort ?? currentModelEntry?.default_reasoning_effort ?? null;

  // Auto-pagination: reaching the top band triggers the next older page — no
  // click. Guarded by `hasMore` and the in-flight `loadingOlder` flag so it
  // can neither run past the oldest row nor overlap a request. The
  // prepend-anchoring in `useStickToBottom` then holds the reading position
  // as the new page lands above.
  const handleStreamScroll = (event: UIEvent<HTMLDivElement>) => {
    const el = event.currentTarget;
    if (hasMoreOlder && !win?.loadingOlder && el.scrollTop <= AUTO_LOAD_TOP_PX) {
      void loadOlder(sessionId);
    }
    if (hasMoreNewer && !win?.loadingNewer && el.scrollHeight - el.scrollTop - el.clientHeight <= AUTO_LOAD_TOP_PX) {
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
        ref={(el) => {
          containerRef.current = el;
          // `useStickToBottom` hands back a callback ref in production, but a
          // plain object ref is a valid React ref too — forward only the former
          // so a merged container capture never calls a non-function.
          if (typeof scrollRef === "function") scrollRef(el);
        }}
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
          <div className="py-10 text-center text-sm text-text-tertiary">{t("chatView.empty")}</div>
        )}
        {rows.map((r, i) => (
          <div
            key={`${r.seq}`}
            data-seq={r.seq}
            className={cn(
              "scroll-mt-2 transition-colors",
              highlightSeq === r.seq && "bg-[var(--color-accent)]/10",
            )}
          >
            <RowView
              row={r}
              agentId={agentId}
              chatSessionId={sessionId}
              showHeader={
                r.kind === "agent_message" && rows[i - 1]?.kind !== "agent_message"
              }
              onJumpToExec={(sid) => void jumpToExec(sid)}
              routerExecSid={
                r.meta?.in_reply_to ? pointerSid.get(r.meta.in_reply_to) : undefined
              }
            />
          </div>
        ))}
        {/* Status card — the one live row the Chat surface owns (§5.5.5:
            pointer durable, state projected). It renders both the in-flight
            turn AND an unanswered execute-session projection card (ask_question
            / approval) bound to that card's exec session, so every projection
            shares one unified card grammar. */}
        {(inFlight || pendingCard) &&
          (() => {
            const cardExecSid =
              inFlight?.execSid ?? pendingCard?.meta?.agent_session_id ?? null;
            const interaction = pendingCard?.meta?.interaction;
            return (
              <ExecSessionStatusCard
                agentId={agentId}
                execSid={cardExecSid}
                assigning={inFlight?.assigning}
                onDrill={
                  cardExecSid
                    ? () =>
                        void (inFlight ? drillIntoExec() : jumpToExec(cardExecSid))
                    : undefined
                }
                card={
                  pendingCard && interaction
                    ? {
                        kind: interaction.kind,
                        messageId: pendingCard.meta?.message_id ?? "",
                      }
                    : undefined
                }
                onAnswerCard={(messageId, text, immediate) => {
                  const cardRow =
                    rows.find((r) => r.meta?.message_id === messageId) ?? pendingCard;
                  if (cardRow) handleAnswer(cardRow, text, immediate);
                }}
              />
            );
          })()}
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
            <span className="inline-flex items-center gap-1 rounded bg-[var(--color-accent)]/10 px-1.5 py-0.5 text-xs font-medium border border-[var(--color-accent)]/20" style={{ color: "var(--color-accent)" }}>
              {t("chatView.replyingTo")} {answerTarget.meta?.interaction?.question?.slice(0, 30) || ""}
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
                  void saveDraft(agentId, { ...draft, model: m, provider: p, reasoning_effort: undefined })
                }
                btnId="model"
              />
            )}
            {reasoningSupported && reasoningEffort != null && (
              <ReasoningEffortMenu
                textHidden={false}
                effort={reasoningEffort}
                onChange={(e) => void saveDraft(agentId, { ...draft, reasoning_effort: e })}
                btnId="effort"
              />
            )}
            <div className="min-w-0">
              <WorkspaceSelector
                textHidden={false}
                value={draft.workspace}
                onChange={(id) => void saveDraft(agentId, { ...draft, workspace: id })}
              />
            </div>
          </div>
          <div data-toolbar-cluster="right" className="flex shrink-0 items-center gap-1">
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
