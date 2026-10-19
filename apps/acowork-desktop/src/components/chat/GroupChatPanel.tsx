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
 * no attachments, no scroll-anchored pagination (a "load earlier" button
 * instead). Upgrade path: fold in the seed chips once the mentions
 * channel carries `session_config`.
 */

import { Children, isValidElement, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Send } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { formatBubbleTime } from "../../lib/formatTime";
import { cn } from "../../lib/utils";
import { getGatewayUrl } from "../../lib/config";
import { fetchDirectory } from "../../lib/auth-api";
import type { DirectoryUser } from "../../lib/types";
import { useStickToBottom } from "../../lib/stickToBottom";
import { AgentAvatar } from "../common/AgentAvatar";
import { UserAvatar } from "../common/UserAvatar";
import { CodeBlock } from "./CodeBlock";
import { handleChatMarkdownLinkClick, markdownUrlTransform } from "../editor/markdownLinkResolver";
import { useAuthStore } from "../../stores/authStore";
import { useAgentStore } from "../../stores/agentStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import {
  useChatSessionStore,
  answeredCardIds,
  type ChatMessageRow,
} from "../../stores/chatSessionStore";

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
    <code className="rounded bg-black/10 px-1 py-0.5 text-[13px] dark:bg-white/10">
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
};

function ChatMarkdown({ content }: { content: string }) {
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm]}
      components={chatMarkdownComponents}
      urlTransform={markdownUrlTransform}
    >
      {content}
    </ReactMarkdown>
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
  nameOf: (id: string) => { name: string; isAgent: boolean };
  answered: Set<string>;
  onAnswer: (row: ChatMessageRow, text: string) => void;
}

function GroupRowView({ row, me, nameOf, answered, onAnswer }: RowProps) {
  const { t } = useTranslation();
  const profile = useUserProfileStore((s) => s.profile);
  const agentsMap = useAgentStore((s) => s.agents);

  if (row.kind === "system_notice") {
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
    const asker = nameOf(row.from);
    if (answered.has(row.meta?.message_id ?? "")) {
      return (
        <div className="mx-6 my-1 rounded-md border border-border-divider bg-chat-bubble px-3 py-2 text-xs text-text-tertiary">
          {t("chatView.cardAnswered")}: {question}
        </div>
      );
    }
    return (
      <div className="mx-6 my-2 rounded-lg border border-[var(--color-accent)]/30 bg-chat-bubble p-3">
        <div className="mb-1 text-10 text-text-tertiary">{asker.name}</div>
        <div className="text-xs font-medium text-text-primary">{question}</div>
        {it?.options && it.options.length > 0 ? (
          <div className="mt-2 flex flex-wrap gap-2">
            {it.options.map((opt) => (
              <button
                key={opt}
                onClick={() => onAnswer(row, opt)}
                className="rounded btn-accent px-3 py-1.5 text-xs"
              >
                {opt}
              </button>
            ))}
            <button
              onClick={() => onAnswer(row, "")}
              className="rounded btn-solid px-3 py-1.5 text-xs"
            >
              {t("chatView.freeAnswer")}
            </button>
          </div>
        ) : (
          <div className="mt-2">
            <button
              onClick={() => onAnswer(row, "")}
              className="rounded btn-accent px-3 py-1.5 text-xs"
            >
              {t("chatView.answer")}
            </button>
          </div>
        )}
      </div>
    );
  }

  const own = row.kind === "user_message" && row.from === me;
  const who = own
    ? { name: profile.displayName, isAgent: false }
    : nameOf(row.from);
  const failed = !!row.meta?.status && row.meta.status !== "completed";
  const agentMeta = who.isAgent ? agentsMap[row.from]?.meta : undefined;

  return (
    <div className={cn("px-4 py-1", own && "flex flex-col items-end")}>
      <div className="mb-[2px] flex items-center gap-1.5 text-10 text-text-tertiary">
        {!own && (
          who.isAgent ? (
            <AgentAvatar
              agentId={row.from}
              displayName={who.name}
              avatarUrl={agentMeta?.avatar}
              version={agentMeta?.version}
              builtinAvatarId={agentMeta?.builtin_avatar ?? null}
              size={20}
              className="shrink-0"
            />
          ) : (
            <UserAvatar
              displayName={who.name}
              avatarUrl={null}
              builtinAvatarId={null}
              size={20}
              className="shrink-0"
            />
          )
        )}
        <span>{who.name}</span>
        <span>{formatBubbleTime(row.ts * 1000)}</span>
      </div>
      <div
        className={cn(
          "max-w-[85%] rounded-md px-3 py-2 text-sm leading-relaxed break-words select-text",
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

// ── View ──────────────────────────────────────────────────────────────

export function GroupChatPanel() {
  const { t } = useTranslation();
  const activeGroupId = useProjectGroupStore((s) => s.activeGroupId);
  const group = useProjectGroupStore(
    (s) => s.groups.find((g) => g.session_id === s.activeGroupId) ?? null,
  );
  const me = useAuthStore((s) => s.account?.user_id ?? "");
  const win = useChatSessionStore(
    (s) => (activeGroupId ? s.windows[activeGroupId] : undefined),
  );
  const inFlight = useChatSessionStore((s) => (activeGroupId ? s.inFlight[activeGroupId] : null));
  const sending = useChatSessionStore((s) => (activeGroupId ? s.sending[activeGroupId] : false));

  const [draft, setDraft] = useState("");
  const [directory, setDirectory] = useState<DirectoryUser[]>([]);
  const nameOf = useParticipantNamer(directory);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  const { ref: scrollRef, stickNow } = useStickToBottom(
    [win?.tail.length ?? 0, activeGroupId],
    activeGroupId,
    () => {},
  );

  // Directory for human sender names — best-effort, once per mount.
  useEffect(() => {
    const token = useAuthStore.getState().accessToken;
    if (!token) return;
    void fetchDirectory(getGatewayUrl(), token)
      .then(setDirectory)
      .catch(() => {});
  }, []);

  const rows = useMemo(() => {
    const all = [...(win?.older ?? []), ...(win?.tail ?? [])];
    return all.filter((r) => !(r.kind === "system_notice" && r.meta?.notice_type === "turn_started"));
  }, [win]);
  const answered = useMemo(() => (win?.tail ? answeredCardIds(win.tail) : new Set<string>()), [win]);

  const agentParticipants = useMemo(() => {
    if (!group?.participants) return [];
    return group.participants
      .filter((p) => p.id !== me && (p.identity === "agent" || !p.identity))
      .map((p) => ({ id: p.id, name: nameOf(p.id).name }))
      .filter((p) => nameOf(p.id).isAgent);
  }, [group, me, nameOf]);

  const submit = useCallback(async () => {
    const body = draft.trim();
    if (!body || !activeGroupId) return;
    setDraft("");
    stickNow();
    const targets = resolveMentions(body, agentParticipants);
    const ok = await useChatSessionStore.getState().sendGroupText(activeGroupId, body, targets);
    if (!ok) setDraft(body);
  }, [draft, activeGroupId, agentParticipants, stickNow]);

  const onAnswer = useCallback(
    (row: ChatMessageRow, text: string) => {
      if (!activeGroupId) return;
      stickNow();
      void useChatSessionStore
        .getState()
        .sendGroupText(activeGroupId, text, [], row.meta?.message_id);
    },
    [activeGroupId, stickNow],
  );

  if (!activeGroupId) return null;

  const title = group?.title || activeGroupId.slice(0, 8);
  const participants = group?.participants ?? [];

  return (
    <div className="flex min-w-0 flex-1 flex-col overflow-hidden rounded-xl border border-border-outer bg-chat-body">
      <header className="flex h-9 shrink-0 items-center gap-2 border-b border-border-divider px-4 text-xs font-semibold text-text-secondary">
        <span className="truncate">{title}</span>
        <span className="shrink-0 font-normal text-text-tertiary">
          {t("groupChat.members", { count: participants.length })}
        </span>
      </header>

      <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto py-2">
        {(win?.total ?? 0) > rows.length && (
          <button
            onClick={() => void useChatSessionStore.getState().loadOlder(activeGroupId)}
            className="mx-auto mb-2 block rounded px-3 py-1 text-10 text-text-tertiary hover:bg-nav-item-hover"
          >
            {t("groupChat.loadEarlier")}
          </button>
        )}
        {rows.length === 0 && !inFlight && (
          <div className="flex h-full items-center justify-center text-xs text-text-tertiary">
            {t("groupChat.empty")}
          </div>
        )}
        {rows.map((row) => (
          <GroupRowView
            key={`${row.seq}-${row.meta?.message_id ?? ""}`}
            row={row}
            me={me}
            nameOf={nameOf}
            answered={answered}
            onAnswer={onAnswer}
          />
        ))}
        {inFlight && (
          <div className="px-6 py-1 text-10 text-text-tertiary">{t("groupChat.working")}</div>
        )}
      </div>

      <div className="shrink-0 border-t border-border-divider p-2">
        <div className="flex items-end gap-2">
          <textarea
            ref={textareaRef}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                void submit();
              }
            }}
            rows={2}
            placeholder={
              agentParticipants.length
                ? t("groupChat.placeholderWithMentions")
                : t("groupChat.placeholder")
            }
            className="min-h-[40px] flex-1 resize-none rounded-md border border-border-outer bg-transparent px-3 py-2 text-sm text-text-primary outline-none focus:border-[var(--color-accent)]/50"
          />
          <button
            onClick={() => void submit()}
            disabled={!draft.trim() || sending}
            className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-[var(--color-accent)] text-white disabled:opacity-40"
            aria-label={t("groupChat.send")}
          >
            <Send className="h-4 w-4" />
          </button>
        </div>
      </div>
    </div>
  );
}
