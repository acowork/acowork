/**
 * ChatHistoryDialog — WeChat-style in-conversation history browser
 * (ADR-097). A single Chat session's records browsed by 文本 / 图片 / 文件,
 * each click locating the source message in the conversation (via
 * `chatSessionStore.jumpToMessage`, which the panels scroll to + flash).
 *
 * Chrome mirrors `GlobalSearchDialog`'s overlay shell (header + pill tab strip
 * + scrollable result zone + Close-only footer), but the data is a single scoped
 * endpoint:
 * `GET /api/chat-sessions/{sid}/search` reads only the disposable
 * `search.sqlite` sidecar. The frontend never caches records — it requests
 * a page, renders it, and pages toward older rows on scroll.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { FileText, Loader2, Search } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { formatBubbleTime } from "../../lib/formatTime";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import {
  searchChatHistory,
  type HistoryCategory,
  type HistoryHit,
} from "../../lib/chat-history-api";
import {
  useChatSessionStore,
  chatAttachmentObjectUrl,
  type ChatAttachmentMeta,
} from "../../stores/chatSessionStore";
import { useAgentStore } from "../../stores/agentStore";
import { useAuthStore } from "../../stores/authStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { AgentAvatar } from "../common/AgentAvatar";
import { UserAvatar } from "../common/UserAvatar";

interface ChatHistoryDialogProps {
  open: boolean;
  onClose: () => void;
  chatSessionId: string;
  /** Optional sender-name override. Without it the dialog names rows from
   *  the local agent registry (keyed by `instance_id`) and the signed-in
   *  account — a raw id is only shown when nothing resolves. Group surfaces
   *  pass their own resolver because they also know the human directory. */
  resolveName?: (id: string) => string;
}

const PAGE_SIZE = 50;
/** Distance from the list bottom (px) that triggers the next older page. */
const LOAD_MORE_BOTTOM_PX = 80;

/** Case-insensitive keyword highlight (same shape as GlobalSearchDialog). */
function highlightParts(text: string, query: string): { text: string; hit: boolean }[] {
  const q = query.trim();
  if (!q) return [{ text, hit: false }];
  const lower = text.toLowerCase();
  const needle = q.toLowerCase();
  const parts: { text: string; hit: boolean }[] = [];
  let i = 0;
  let idx = lower.indexOf(needle, i);
  while (idx !== -1 && parts.length < 40) {
    if (idx > i) parts.push({ text: text.slice(i, idx), hit: false });
    parts.push({ text: text.slice(idx, idx + q.length), hit: true });
    i = idx + q.length;
    idx = lower.indexOf(needle, i);
  }
  if (i < text.length) parts.push({ text: text.slice(i), hit: false });
  return parts.length ? parts : [{ text, hit: false }];
}

function Highlight({ text, query }: { text: string; query: string }) {
  return (
    <>
      {highlightParts(text, query).map((p, i) =>
        p.hit ? (
          <mark
            key={i}
            className="rounded-sm bg-[var(--color-accent)]/25 px-0.5 text-[var(--color-accent)]"
          >
            {p.text}
          </mark>
        ) : (
          <span key={i}>{p.text}</span>
        ),
      )}
    </>
  );
}

/** One image attachment: object-URL thumb (full-image fallback on 404). */
function HistoryThumb({
  chatSessionId,
  attachment,
}: {
  chatSessionId: string;
  attachment: ChatAttachmentMeta;
}) {
  const [url, setUrl] = useState<string | null>(null);
  const [triedFull, setTriedFull] = useState(false);
  useEffect(() => {
    let cancelled = false;
    const load = async (thumb: boolean) => {
      try {
        const u = await chatAttachmentObjectUrl(chatSessionId, attachment.id, { thumb });
        if (!cancelled) setUrl(u);
      } catch {
        // No thumb sidecar → retry once against the full blob.
        if (thumb && !cancelled) {
          setTriedFull(true);
          try {
            const u = await chatAttachmentObjectUrl(chatSessionId, attachment.id);
            if (!cancelled) setUrl(u);
          } catch {
            /* leave blank */
          }
        }
      }
    };
    if (!triedFull) void load(true);
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [chatSessionId, attachment.id]);
  if (!url) {
    return <div className="aspect-square w-full animate-pulse rounded bg-chat-bubble" />;
  }
  return (
    <img
      src={url}
      alt={attachment.filename}
      loading="lazy"
      className="aspect-square w-full rounded object-cover"
    />
  );
}

const isImage = (a: ChatAttachmentMeta) => a.mime.startsWith("image/");

export function ChatHistoryDialog({
  open,
  onClose,
  chatSessionId,
  resolveName,
}: ChatHistoryDialogProps) {
  const { t } = useTranslation();
  const [category, setCategory] = useState<HistoryCategory>("text");
  const [input, setInput] = useState("");
  // The query actually searched — an unsent edit does not re-request. Text
  // browses on empty (whole category); typing + Enter searches; image ignores
  // the box; file filters by filename.
  const [ranQuery, setRanQuery] = useState("");
  const [hits, setHits] = useState<HistoryHit[]>([]);
  const [cursor, setCursor] = useState<number | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const reqSeq = useRef(0);

  // Escape closes.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  const runFirstPage = useCallback(
    async (cat: HistoryCategory, q: string) => {
      const seq = ++reqSeq.current;
      setLoading(true);
      setError(null);
      try {
        const page = await searchChatHistory(chatSessionId, cat, q, null, PAGE_SIZE);
        if (seq !== reqSeq.current) return; // a newer run superseded this one
        setHits(page.hits);
        setCursor(page.next_before_line);
        setHasMore(page.next_before_line != null);
      } catch (e) {
        if (seq !== reqSeq.current) return;
        setError(e instanceof Error ? e.message : String(e));
        setHits([]);
        setHasMore(false);
      } finally {
        if (seq === reqSeq.current) setLoading(false);
      }
    },
    [chatSessionId],
  );

  // (Re)load whenever the dialog opens or the tab / committed query changes.
  useEffect(() => {
    if (!open) return;
    void runFirstPage(category, category === "image" ? "" : ranQuery);
  }, [open, category, ranQuery, runFirstPage]);

  const loadMore = useCallback(async () => {
    if (loadingMore || !hasMore || cursor == null) return;
    const seq = reqSeq.current;
    setLoadingMore(true);
    try {
      const page = await searchChatHistory(
        chatSessionId,
        category,
        category === "image" ? "" : ranQuery,
        cursor,
        PAGE_SIZE,
      );
      if (seq !== reqSeq.current) return; // the tab/query changed mid-scroll
      setHits((h) => [...h, ...page.hits]);
      setCursor(page.next_before_line);
      setHasMore(page.next_before_line != null);
    } catch (e) {
      if (seq !== reqSeq.current) return;
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoadingMore(false);
    }
  }, [chatSessionId, category, ranQuery, cursor, hasMore, loadingMore]);

  const handleScroll = (e: React.UIEvent<HTMLDivElement>) => {
    const el = e.currentTarget;
    if (el.scrollHeight - el.scrollTop - el.clientHeight <= LOAD_MORE_BOTTOM_PX) void loadMore();
  };

  const submit = () => setRanQuery(input.trim());

  const jump = useCallback(
    (hit: HistoryHit) => {
      void useChatSessionStore.getState().jumpToMessage(chatSessionId, hit.line_pos, hit.seq);
      onClose();
    },
    [chatSessionId, onClose],
  );

  // Sender naming. A session's participant ids are agent INSTANCE ids or
  // account ids — both opaque. Resolve them through the local agent registry
  // (`agents` is keyed by instance_id) and the signed-in account; a caller
  // with a richer directory (group chat) passes `resolveName` and wins.
  const agentsMap = useAgentStore((s) => s.agents);
  const account = useAuthStore((s) => s.account);
  const registryName = useCallback(
    (id: string) => {
      const agent = agentsMap[id]?.meta;
      if (agent) return agent.display_name ?? agent.name ?? id;
      if (account && id === account.user_id) return account.display_name || account.username;
      return id;
    },
    [agentsMap, account],
  );
  const nameOf = resolveName ?? registryName;

  const tabs: Array<{ key: HistoryCategory; label: string }> = useMemo(
    () => [
      { key: "text", label: t("chatHistory.tabText") },
      { key: "image", label: t("chatHistory.tabImage") },
      { key: "file", label: t("chatHistory.tabFile") },
    ],
    [t],
  );

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay"
      onMouseDown={onClose}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={t("chatHistory.title")}
        onMouseDown={(e) => e.stopPropagation()}
        className="flex h-[min(560px,80vh)] w-full max-w-2xl flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface text-text shadow-xl"
      >
        {/* Header — same zone height / type scale as GlobalSearchDialog, so the
            two dialogs line up when one replaces the other. No close affordance
            here: dismissal lives in the footer, like the search dialog. */}
        <div className="flex shrink-0 items-center gap-2 border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <Search className="h-5 w-5 shrink-0 text-text-tertiary" />
          <h2 className="min-w-0 flex-1 truncate text-sm font-semibold">
            {t("chatHistory.title")}
          </h2>
        </div>

        {/* Tab strip — centered pills, mirroring GlobalSearchDialog's result
            tabs (rounded-full; the active one takes the zinc wash and full-
            strength text rather than accent-colored text). */}
        <div className="flex shrink-0 items-center justify-center gap-1 border-b border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          {tabs.map((tab) => (
            <button
              key={tab.key}
              type="button"
              onClick={() => setCategory(tab.key)}
              className={`rounded-full px-3 py-1.5 text-xs font-medium transition-colors ${
                category === tab.key
                  ? "bg-zinc-200 text-text dark:bg-zinc-700"
                  : "text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700/50"
              }`}
            >
              {tab.label}
            </button>
          ))}
        </div>

        {/* Query row — a divider-separated zone like the search dialog's
            input band: magnifier on the left, borderless input inside. */}
        {category !== "image" && (
          <div className="flex shrink-0 items-center gap-1.5 border-b border-border-divider px-5 py-1.5 min-h-[var(--ui-dialog-zone-h)]">
            <Search className="h-3 w-3 shrink-0 text-text-tertiary" />
            <input
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  submit();
                }
              }}
              placeholder={
                category === "file"
                  ? t("chatHistory.searchFilePlaceholder")
                  : t("chatHistory.searchTextPlaceholder")
              }
              className="flex-1 bg-transparent text-xs text-text-secondary outline-none placeholder:text-text-tertiary"
            />
          </div>
        )}

        {/* Result zone
            The zone must be a flex column: `OverlayScrollbar`'s outer box is
            `min-h-0 flex-1`, so only a flex parent gives it a definite
            height (`fill="flex"` then sizes the viewport). Without this the
            viewport grows with the hits, `maxScroll` stays 0, and neither
            the scrollbar nor `loadMore()` on scroll ever fires. */}
        <div className="flex min-h-0 flex-1 flex-col">
          {loading ? (
            <div className="flex h-full items-center justify-center gap-2 text-xs text-text-tertiary">
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              {t("chatHistory.loading")}
            </div>
          ) : error && hits.length === 0 ? (
            <div className="flex h-full items-center justify-center px-6 text-center text-xs text-red-500">
              {t("chatHistory.error")}
            </div>
          ) : hits.length === 0 ? (
            <div className="flex h-full items-center justify-center text-xs text-text-tertiary">
              {t("chatHistory.empty")}
            </div>
          ) : (
            <OverlayScrollbar fill="flex" onViewportScroll={handleScroll} className="h-full">
              {category === "image" ? (
                <ImageGrid hits={hits} chatSessionId={chatSessionId} onJump={jump} />
              ) : (
                <div className="py-1">
                  {hits.map((hit) =>
                    category === "file" ? (
                      <FileRow
                        key={`${hit.line_pos}-${hit.seq}`}
                        hit={hit}
                        nameOf={nameOf}
                        onJump={() => jump(hit)}
                      />
                    ) : (
                      <TextRow
                        key={`${hit.line_pos}-${hit.seq}`}
                        hit={hit}
                        query={ranQuery}
                        nameOf={nameOf}
                        onJump={() => jump(hit)}
                      />
                    ),
                  )}
                </div>
              )}
              {loadingMore && (
                <div className="flex items-center justify-center gap-1.5 py-3 text-10 text-text-tertiary">
                  <Loader2 className="h-3 w-3 animate-spin" />
                  {t("chatHistory.loading")}
                </div>
              )}
            </OverlayScrollbar>
          )}
        </div>

        {/* Footer — dismissal lives here (GlobalSearchDialog's footer shape):
            a divider-separated zone with one right-aligned Close button, so the
            header carries no competing X. */}
        <div className="flex shrink-0 items-center justify-end border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 dark:hover:bg-zinc-700"
          >
            {t("chatHistory.close")}
          </button>
        </div>
      </div>
    </div>
  );
}

// ─── Rows ──────────────────────────────────────────────────────────────

/** Sender avatar for one history row. Same resolution order (and the same
 *  primitives) the chat surfaces use, so a participant looks identical here
 *  and in the conversation: registry agent -> `AgentAvatar`, the signed-in
 *  account -> the local profile's avatar, anyone else -> a `UserAvatar`
 *  seeded by name (the fallback GroupChatPanel draws for peers too). */
function SenderAvatar({
  id,
  name,
  size = 28,
  className,
}: {
  id: string;
  name: string;
  size?: number;
  className?: string;
}) {
  const agent = useAgentStore((s) => s.agents[id]?.meta);
  const me = useAuthStore((s) => s.account?.user_id);
  const profile = useUserProfileStore((s) => s.profile);

  if (agent) {
    return (
      <AgentAvatar
        agentId={id}
        displayName={name}
        avatarUrl={agent.avatar}
        version={agent.version}
        builtinAvatarId={agent.builtin_avatar ?? null}
        size={size}
        className={className}
      />
    );
  }

  const own = !!me && id === me;
  return (
    <UserAvatar
      displayName={own ? profile.displayName : name}
      avatarUrl={own ? (profile.backendAvatarUrl ?? null) : null}
      builtinAvatarId={own ? (profile.backendBuiltinAvatarId ?? undefined) : null}
      size={size}
      className={className}
    />
  );
}

function TextRow({
  hit,
  query,
  nameOf,
  onJump,
}: {
  hit: HistoryHit;
  query: string;
  nameOf: (id: string) => string;
  onJump: () => void;
}) {
  const name = nameOf(hit.from);
  return (
    <button
      type="button"
      onClick={onJump}
      className={cn("flex w-full items-start gap-2.5 px-4 py-2 text-left", listRowHover)}
    >
      <SenderAvatar id={hit.from} name={name} className="mt-0.5 shrink-0" />
      <div className="flex min-w-0 flex-1 flex-col gap-0.5">
        {/* Sender line is the scan anchor of the row: 12px semibold in the
            high-contrast text token, with the timestamp demoted to a 10px
            tertiary hint beside it. */}
        <div className="flex min-w-0 items-center gap-2">
          <span className="min-w-0 truncate text-xs font-semibold text-text">{name}</span>
          <span className="shrink-0 text-10 text-text-tertiary">
            {formatBubbleTime(hit.ts * 1000)}
          </span>
        </div>
        <span className="line-clamp-2 text-xs text-text-primary">
          <Highlight text={hit.body} query={query} />
        </span>
      </div>
    </button>
  );
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function FileRow({
  hit,
  nameOf,
  onJump,
}: {
  hit: HistoryHit;
  nameOf: (id: string) => string;
  onJump: () => void;
}) {
  const files = hit.attachments.filter((a) => !isImage(a));
  return (
    <button
      type="button"
      onClick={onJump}
      className={cn("flex w-full items-center gap-3 px-4 py-2 text-left", listRowHover)}
    >
      <FileText size={18} className="shrink-0 text-text-tertiary" />
      <div className="min-w-0 flex-1">
        <div className="truncate text-xs font-medium text-text-primary">
          {files[0]?.filename ?? hit.body ?? nameOf(hit.from)}
        </div>
        <div className="flex items-center gap-2 text-10 text-text-tertiary">
          <span className="min-w-0 truncate">{nameOf(hit.from)}</span>
          <span className="shrink-0">{formatBubbleTime(hit.ts * 1000)}</span>
        </div>
      </div>
      {files.length > 0 && (
        <span className="shrink-0 text-10 text-text-tertiary">{formatSize(files[0]!.size)}</span>
      )}
    </button>
  );
}

function ImageGrid({
  hits,
  chatSessionId,
  onJump,
}: {
  hits: HistoryHit[];
  chatSessionId: string;
  onJump: (hit: HistoryHit) => void;
}) {
  // Flatten: one cell per image attachment, tagged with its owning hit so a
  // click locates that message; the day label groups visually by message date.
  const cells = hits.flatMap((hit) =>
    hit.attachments.filter(isImage).map((a) => ({ hit, a })),
  );
  if (cells.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-xs text-text-tertiary">
        —
      </div>
    );
  }
  return (
    <div className="grid grid-cols-4 gap-1.5 p-3">
      {cells.map(({ hit, a }) => (
        <button
          key={`${hit.line_pos}-${a.id}`}
          type="button"
          onClick={() => onJump(hit)}
          title={formatBubbleTime(hit.ts * 1000)}
          className="group relative overflow-hidden rounded"
        >
          <HistoryThumb chatSessionId={chatSessionId} attachment={a} />
          <span className="pointer-events-none absolute inset-x-0 bottom-0 truncate bg-black/45 px-1 py-0.5 text-left text-9 text-white opacity-0 transition-opacity group-hover:opacity-100">
            {formatBubbleTime(hit.ts * 1000)}
          </span>
        </button>
      ))}
    </div>
  );
}
