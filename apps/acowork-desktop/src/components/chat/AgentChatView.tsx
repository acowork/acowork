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
// - No Paperclip yet: `POST /api/intents/chat-message` carries no
//   attachment ids (the session-scoped write does, but that channel never
//   starts a turn). `ponytail:` ceiling — attachment send needs the
//   gateway `ChatMessageRequest` field + runtime ingestion first; upgrade
//   path is one backend wave, then this composer adds the button.
// - No @-mention UI: a 1:1 user_agent session has exactly one other
//   participant (design 27 §5.5.4 "单聊无需 @").
//
// The status card is the drill-down INTO the execution view (plan 5.3):
// click → `chatStore.openSession(agentId, execSid)` + mode flip.

import { Children, isValidElement, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Send, ExternalLink } from "lucide-react";
import { formatBubbleTime } from "../../lib/formatTime";
import { cn } from "../../lib/utils";
import { toolbarIconButton } from "../../lib/ui-styles";
import { Tooltip } from "../common/Tooltip";
import { UserAvatar } from "../common/UserAvatar";
import { AgentAvatar } from "../common/AgentAvatar";
import { CodeBlock } from "./CodeBlock";
import { handleChatMarkdownLinkClick, markdownUrlTransform } from "../editor/markdownLinkResolver";
import { ModelMenu, ReasoningEffortMenu } from "./ChatPanel";
import { WorkspaceSelector } from "../workspace/WorkspaceSelector";
import { ExecSessionStatusCard } from "./ExecSessionStatusCard";
import { useChatSessionStore, answeredCardIds, type ChatMessageRow, type ChatSessionConfig } from "../../stores/chatSessionStore";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { useAuthStore } from "../../stores/authStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { getGatewayUrl } from "../../lib/config";

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
  showHeader: boolean;
  answered: boolean;
  onAnswer: (card: ChatMessageRow, text: string, immediate: boolean) => void;
  /** Drill into the execution session that produced this agent message.
   *  Every `agent_message` with a resolved exec session id is clickable —
   *  finished, failed or empty-bodied alike (§5.5.5: the pointer survives
   *  rotation, the JSONL stays in Agent home). */
  onJumpToExec: (execSid: string) => void;
}

function RowView({ row, agentId, showHeader, answered, onAnswer, onJumpToExec }: RowProps) {
  const { t } = useTranslation();
  const agent = useAgentStore((s) => s.agents[agentId]?.meta);
  const profile = useUserProfileStore((s) => s.profile);
  const agentName = agent?.display_name ?? agent?.name ?? "";

  if (row.kind === "system_notice") {
    // `turn_started` is rendered as the status card, never as a row; other
    // notices (receipts) are a quiet centered line.
    if (row.meta?.notice_type === "turn_started") return null;
    return (
      <div className="py-1 text-center text-10 text-text-tertiary">
        {row.body || row.meta?.status || ""}
      </div>
    );
  }

  if (row.kind === "interaction_request") {
    const it = row.meta?.interaction;
    const question = it?.question || row.body;
    if (answered) {
      return (
        <div className="mx-6 my-1 rounded-md border border-border-divider bg-chat-bubble px-3 py-2 text-xs text-text-tertiary">
          {t("chatView.cardAnswered")}: {question}
        </div>
      );
    }
    return (
      <div className="mx-6 my-2 rounded-lg border border-[var(--color-accent)]/30 bg-chat-bubble p-3">
        <div className="text-xs font-medium text-text-primary">{question}</div>
        {it?.options && it.options.length > 0 && (
          <div className="mt-2 flex flex-wrap gap-2">
            {it.options.map((opt) => (
              <button
                key={opt}
                onClick={() => onAnswer(row, opt, true)}
                className="rounded btn-accent px-3 py-1.5 text-xs"
              >
                {opt}
              </button>
            ))}
            <button
              onClick={() => onAnswer(row, "", false)}
              className="rounded btn-solid px-3 py-1.5 text-xs"
            >
              {t("chatView.freeAnswer")}
            </button>
          </div>
        )}
        {!it?.options?.length && (
          <div className="mt-2">
            <button
              onClick={() => onAnswer(row, "", false)}
              className="rounded btn-accent px-3 py-1.5 text-xs"
            >
              {t("chatView.answer")}
            </button>
          </div>
        )}
      </div>
    );
  }

  if (row.kind === "agent_message") {
    const failed = !!row.meta?.status && row.meta.status !== "completed";
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
          <div className="group min-w-0">
            <div className="max-w-[var(--content-max-width)] rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5  select-text break-words">
              {failed && (
                <div className="mb-1 rounded bg-red-50 px-2 py-0.5 text-10 text-red-500 dark:bg-red-900/20">
                  {row.meta?.status}
                </div>
              )}
              {row.body && <ChatMarkdown content={row.body} />}
              {/* A turn can end with an empty response body — a stop after the
                  intent already routed back writes an `agent_message` with no
                  text (§5.5.5). Render a placeholder instead of a blank bubble
                  so the user sees the run ended with nothing, not a glitch. */}
              {!row.body && !row.attachments?.length && (
                <span className="italic text-text-tertiary">{t("chatView.emptyReply")}</span>
              )}
              {row.attachments && row.attachments.length > 0 && (
                <div className="mt-1 text-10 text-text-tertiary">
                  {row.attachments.map((a) => a.filename).join(", ")}
                </div>
              )}
            </div>
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
                  session, empty body included. */}
              {execSid && (
                <button
                  type="button"
                  onClick={() => onJumpToExec(execSid)}
                  title={t("chatView.viewRun")}
                  className="inline-flex shrink-0 items-center gap-1 text-10 text-text-tertiary transition-colors hover:text-[var(--color-accent)]"
                >
                  <ExternalLink size={11} />
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
            <div className="w-full rounded-md rounded-br-sm bg-chat-user text-chat-user-text select-text">
              <div className="px-4 py-2.5 whitespace-pre-wrap break-words">
                {row.body}
                {row.attachments && row.attachments.length > 0 && (
                  <div className="mt-1 text-10 opacity-70">
                    {row.attachments.map((a) => a.filename).join(", ")}
                  </div>
                )}
              </div>
            </div>
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

export function AgentChatView({ agentId, sessionId }: AgentChatViewProps) {
  const { t } = useTranslation();
  const win = useChatSessionStore((s) => s.windows[sessionId]);
  const inFlight = useChatSessionStore((s) => s.inFlight[sessionId] ?? null);
  const sending = !!useChatSessionStore((s) => s.sending[sessionId]);
  // The seed draft is the CACHE of the session row's `session_config`
  // (design 27 §4.5, backend-persisted): hydrated by `openAgent`, updated
  // by `saveDraft`'s PATCH response. Stable ref so the selector never
  // re-renders on an absent draft.
  const draft = useChatSessionStore((s) => s.draft[sessionId] ?? EMPTY_DRAFT);
  const { sendText, loadOlder, setMode, saveDraft } = useChatSessionStore.getState();

  const [input, setInput] = useState("");
  const [answerTarget, setAnswerTarget] = useState<ChatMessageRow | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const lastCompositionEndRef = useRef(0);

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
  const answered = useMemo(() => answeredCardIds(rows), [rows]);

  // Stick to the bottom when new rows land (same simple rule the IM view
  // uses; the execution view's scroll controller is session-stream shaped
  // and does not apply to HTTP windows).
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [rows.length, inFlight]);

  const handleAnswer = (card: ChatMessageRow, text: string, immediate: boolean) => {
    if (immediate && text) {
      void sendText(sessionId, text, card.meta?.message_id);
      return;
    }
    setAnswerTarget(card);
    textareaRef.current?.focus();
  };

  const handleSend = () => {
    const text = input.trim();
    if (!text || sending) return;
    const replyTo = answerTarget?.meta?.message_id;
    // A card answer continues the ORIGINAL turn — no new execution session
    // is created, so a config draft would be meaningless there (§4.5 seed
    // applies at session creation only). Plain messages carry the draft.
    const sessionConfig = replyTo ? undefined : (Object.keys(draft).length > 0 ? draft : undefined);
    setInput("");
    setAnswerTarget(null);
    void sendText(sessionId, text, replyTo, sessionConfig);
  };

  const drillIntoExec = async () => {
    if (!inFlight?.execSid) return;
    await useChatStore.getState().openSession(agentId, inFlight.execSid);
    setMode(agentId, "exec");
  };

  // Jump to the execution session behind a specific agent message — the same
  // drill the in-flight status card does, but for a resolved turn (finished,
  // failed or empty). openSession cold-loads the exec tab; the pointer stays
  // valid after rotation because the old JSONL remains in Agent home (§5.5.5).
  const jumpToExec = async (execSid: string) => {
    if (!execSid) return;
    await useChatStore.getState().openSession(agentId, execSid);
    setMode(agentId, "exec");
  };

  const hasMore = win ? win.tailLoaded + win.older.length < win.total : false;
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

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {/* Message stream */}
      <div ref={scrollRef} className="flex-1 overflow-y-auto py-2">
        {hasMore && (
          <div className="py-2 text-center">
            <button
              onClick={() => void loadOlder(sessionId)}
              disabled={win?.loadingOlder}
              className="rounded btn-solid px-3 py-1 text-xs disabled:opacity-50"
            >
              {t("chatHistory.loadEarlier")}
            </button>
          </div>
        )}
        {rows.length === 0 && (
          <div className="py-10 text-center text-sm text-text-tertiary">{t("chatView.empty")}</div>
        )}
        {rows.map((r, i) => (
          <RowView
            key={`${r.seq}`}
            row={r}
            agentId={agentId}
            showHeader={
              r.kind === "agent_message" && rows[i - 1]?.kind !== "agent_message"
            }
            answered={!!(r.meta?.message_id && answered.has(r.meta.message_id))}
            onAnswer={handleAnswer}
            onJumpToExec={(sid) => void jumpToExec(sid)}
          />
        ))}
        {/* Status card — the one live row the Chat surface owns (§5.5.5:
            pointer durable, state projected). The card mirrors the exec
            session's live event stream (status / step / model / workspace /
            error / approval / pause) and forwards controls to the session-
            scoped HTTP endpoints — no independent frontend state. */}
        {inFlight && (
          <ExecSessionStatusCard
            agentId={agentId}
            execSid={inFlight.execSid ?? null}
            onDrill={inFlight.execSid ? () => void drillIntoExec() : undefined}
          />
        )}
      </div>

      {/* Composer — same chrome as the execution view (plan 5.4). */}
      <div className="mx-6 mb-3 rounded-xl border border-chat-input-border bg-chat-input-bg">
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
