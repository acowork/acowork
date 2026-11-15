//! Compression History card (ADR-061 §15-16).
//!
//! DevMode diagnostics: lists every LLM-driven context compaction event
//! of the active session (level / before → after tokens / ratio / model).
//!
//! Pure frontend — no runtime changes: compaction events are persisted
//! as `kind="compaction"` JSONL entries (with `CompactionEventMeta` in
//! `metadata`) and served by the existing
//! `GET /api/agents/{id}/sessions/{sid}/messages` API (the same source
//! the chat stream's `CompactionCard` renders from).
//!
//! Renders on the unified list grammar (common/list): a ListBox card
//! containing one ExpandableRow per compaction event; the expanded body
//! shows the details as a KvList. The per-level colour pill (cnLevel)
//! intentionally keeps its own amber/orange coding — it is data-viz
//! (deep-compaction severity), not a generic status badge.
import { useCallback, useEffect, useState } from "react";
import { Loader, RefreshCw } from "lucide-react";

import { getGatewayUrl } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import type { CompactionEventMeta, ConversationEntry } from "../../lib/types";
import { ListBox, ExpandableRow, KvList } from "../common/list";

interface CompactionRow {
  ts: number;
  meta: CompactionEventMeta;
}

const HISTORY_LIMIT = 500;

function formatTokens(n: number): string {
  return n.toLocaleString("en-US");
}

function formatRatio(before: number, after: number): string {
  if (before <= 0) return "—%";
  const ratio = 1 - after / before;
  return `${(ratio * 100).toFixed(1)}%`;
}

function shortId(id?: string): string {
  if (!id) return "—";
  return id.length > 12 ? `${id.slice(0, 6)}…${id.slice(-4)}` : id;
}

function formatTime(ts: number): string {
  return new Date(ts).toLocaleTimeString("en-US", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function CompressionHistoryCard({
  agentId,
  sessionId,
}: {
  agentId: string | null;
  sessionId: string | null;
}) {
  const { t } = useTranslation();
  const [rows, setRows] = useState<CompactionRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Indices of expanded rows. Use a Set so multiple rows can be open at
  // once (mirrors SnapshotNode, which keeps each snapshot independent).
  const [expanded, setExpanded] = useState<Set<number>>(() => new Set());
  // Level-1 collapse state — the whole history card body toggles from
  // the header row (same interaction as the PROMPT card). Default open.
  const [open, setOpen] = useState(true);

  const load = useCallback(async () => {
    if (!agentId || !sessionId) {
      setRows([]);
      setError(null);
      return;
    }
    setRows(null);
    setError(null);
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/messages?limit=${HISTORY_LIMIT}&tail=true`,
      );
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as { messages: ConversationEntry[] };
      const compactions = (data.messages ?? [])
        .filter((e) => e.kind === "compaction")
        .map((e) => ({
          ts: new Date(e.ts).getTime(),
          meta: (e.metadata ?? {}) as unknown as CompactionEventMeta,
        }))
        .sort((a, b) => a.ts - b.ts);
      setRows(compactions);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [agentId, sessionId]);

  useEffect(() => {
    void load();
  }, [load]);

  const toggleExpanded = useCallback((idx: number) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(idx)) next.delete(idx);
      else next.add(idx);
      return next;
    });
  }, []);

  return (
    <ListBox dividers={false}>
      {/* Level-1 collapsible header — matches the PROMPT card header:
          clicking the row toggles the whole history list; the refresh
          button stops propagation. */}
      <ExpandableRow
        open={open}
        onToggle={() => setOpen((v) => !v)}
        title={t("rightPanel.compressionHistory", { count: rows?.length ?? 0 })}
        trailing={
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              void load();
            }}
            title={t("rightPanel.buttonRefresh")}
            className="rounded p-1 text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/10 hover:text-text-secondary  dark:hover:bg-[var(--color-accent)]/15"
          >
            <RefreshCw className="h-3.5 w-3.5" />
          </button>
        }
        bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
      >

      {rows === null && !error && (
        <div className="flex items-center justify-center gap-2 py-3 text-xs text-text-tertiary">
          <Loader className="h-3.5 w-3.5 animate-spin" />
          {t("rightPanel.loadingCompressionHistory")}
        </div>
      )}

      {error && (
        <div className="py-3 text-center text-xs text-danger-icon">
          {t("rightPanel.compressionHistoryError")}: {error}
        </div>
      )}

      {rows !== null && !error && rows.length === 0 && (
        <div className="py-3 text-center text-xs text-text-tertiary">
          {t("rightPanel.noCompressionEvents")}
        </div>
      )}

      {rows !== null && !error && rows.length > 0 && (
        <ListBox variant="plain">
          {rows.map((row, i) => {
            const m = row.meta;
            const time = formatTime(row.ts);
            const before = m.before_tokens ?? 0;
            const after = m.after_tokens ?? 0;
            const ratio = formatRatio(before, after);
            return (
              <ExpandableRow
                key={i}
                open={expanded.has(i)}
                onToggle={() => toggleExpanded(i)}
                surface="inset"
                title={
                  <span className="font-mono text-11 text-text-tertiary ">
                    {time}
                  </span>
                }
                meta={<span className={cnLevel(m.level)}>Lv{m.level}</span>}
                trailing={
                  <span className="font-mono text-11 text-text-secondary ">
                    {ratio}
                  </span>
                }
                bodyClassName="mx-2 mb-2 mt-1"
              >
                <KvList
                  rows={[
                    { k: t("rightPanel.compTime"), v: time },
                    { k: t("rightPanel.compLevel"), v: m.level },
                    { k: t("rightPanel.compTokens"), v: `${formatTokens(before)} → ${formatTokens(after)}` },
                    { k: t("rightPanel.compRatio"), v: ratio },
                    ...(m.model ? [{ k: t("rightPanel.compModel"), v: m.model }] : []),
                    ...(m.compacted_from_id || m.compacted_to_id
                      ? [{
                          k: t("rightPanel.compRange"),
                          v: `${shortId(m.compacted_from_id)} → ${shortId(m.compacted_to_id)}`,
                        }]
                      : []),
                  ]}
                />
              </ExpandableRow>
            );
          })}
        </ListBox>
      )}
      </ExpandableRow>
    </ListBox>
  );
}

/** Level badge: 8 (minimal form) is the most aggressive level — tint it
 *  amber so an unexpected deep compaction stands out at a glance. */
function cnLevel(level: number): string {
  if (level >= 8) {
    return "rounded bg-warning-surface-hover px-1 py-0.5 text-10 font-medium text-warning-quiet";
  }
  if (level >= 5) {
    return "rounded bg-warning-surface px-1 py-0.5 text-10 font-medium text-warning-quiet";
  }
  return "rounded bg-surface px-1 py-0.5 text-10 font-medium text-text-secondary ";
}
