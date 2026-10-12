//! Memory node master list — bare scroll container for the rows that the
//! "记忆搜索" (Memory Search) card renders in its body. The chrome
//! (ListBox + ExpandableRow title row) used to live here in v1, but
//! nesting it inside the new "Memory Search" card body produced a
//! card-within-a-card layout — the two chevrons fought each other and
//! the inner title was redundant with the outer one. This component
//! now owns only: the inner ListBox of ListRow, the loading/empty
//! states, and the pager strip at the bottom. The outer card (title,
//! collapse, surface) is owned by MemoryPanel.
//!
//! Clicking a row drills into the MemoryNodeDetail view (master-detail;
//! intentionally NOT an inline expansion — the detail pane carries
//! edit/delete controls).
import type { MemoryNodeResponse } from "../../lib/types";
import { cn } from "../../lib/utils";
import { Loader2, ChevronLeft, ChevronRight } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useNodeTypeLabel, useSubTypeLabel } from "./nodeTypeI18n";
import { ListBox, ListRow, Badge, EmptyState } from "../common/list";
import { OverlayScrollbar } from "../common/OverlayScrollbar";

interface MemoryNodeListProps {
  nodes: MemoryNodeResponse[];
  total: number;
  page: number;
  pageSize: number;
  totalPages: number;
  loading: boolean;
  selectedNodeId: number | null;
  onSelectNode: (id: number | null) => void;
  onPageChange: (page: number) => void;
}

const accentText = "text-[var(--color-accent)]";

function formatDate(ts: number): string {
  if (ts === 0) return "—";
  const d = new Date(ts * 1000);
  return d.toLocaleString();
}

function truncateContent(content: string, maxLen = 80): string {
  if (content.length <= maxLen) return content;
  return content.slice(0, maxLen) + "…";
}

export function MemoryNodeList({
  nodes,
  total,
  page,
  totalPages,
  loading,
  selectedNodeId,
  onSelectNode,
  onPageChange,
}: MemoryNodeListProps) {
  const labelOf = useNodeTypeLabel();
  const subLabelOf = useSubTypeLabel();
  const { t } = useTranslation();

  // `total` is not rendered directly: the surrounding "记忆搜索" card
  // owns the result-count badge (the parent already passes the same
  // total into the top stats strip). It stays in the prop signature so
  // the parent call site does not need to change — we just mark the
  // destructured binding as intentionally consumed.
  void total;

  // Pager strip lives at the bottom of the body — the surrounding
  // "记忆搜索" card now owns the title/collapse chrome, so there is no
  // header trailing slot to drop these into. Layout mirrors the
  // Session-tab pager at SessionTabBar.tsx:215 (justify-between, three
  // slot — prev / "Page X of Y" / next) so the two bottom pagers feel
  // consistent across the right panel. Shown only when there is more
  // than one page; single-page result sets stay clean.
  const pager =
    totalPages > 1 ? (
      <div className="flex shrink-0 items-center justify-between border-t border-border-divider px-1 py-1.5">
        <button
          type="button"
          aria-label="Previous memory page"
          disabled={page <= 1}
          onClick={() => onPageChange(page - 1)}
          className="inline-flex items-center rounded-md px-1.5 py-0.5 text-text-tertiary hover:bg-zinc-100 disabled:opacity-30  dark:hover:bg-zinc-800"
        >
          <ChevronLeft className="h-3.5 w-3.5" />
        </button>
        <span className="text-11 text-text-tertiary ">
          {t("memoryPanel.pagerOf", { current: page, total: totalPages })}
        </span>
        <button
          type="button"
          aria-label="Next memory page"
          disabled={page >= totalPages}
          onClick={() => onPageChange(page + 1)}
          className="inline-flex items-center rounded-md px-1.5 py-0.5 text-text-tertiary hover:bg-zinc-100 disabled:opacity-30  dark:hover:bg-zinc-800"
        >
          <ChevronRight className="h-3.5 w-3.5" />
        </button>
      </div>
    ) : null;

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
      {loading && nodes.length === 0 ? (
        <div className="flex flex-1 items-center justify-center py-10">
          <Loader2 className="h-5 w-5 animate-spin text-text-tertiary " />
        </div>
      ) : !loading && nodes.length === 0 ? (
        <EmptyState
          message={t("memoryPanel.emptyNodes")}
          className="flex-1 items-center justify-center"
        />
      ) : (
        <>
          <OverlayScrollbar dataTabScroll className="min-h-0 flex-1">
            <ListBox variant="plain">
              {nodes.map((node) => {
                  const isSelected = node.node_id === selectedNodeId;

                  return (
                    <ListRow
                      key={node.node_id}
                      selected={isSelected}
                      surface="inset"
                      onClick={() => onSelectNode(node.node_id)}
                    >
                      <div className="flex flex-col gap-1">
                        {/* Top row: type + status, both on `text-11` — the
                            card-title step. The shared Badge default is
                            `text-9`, which reads as too small next to the
                            12px content line, so this row opts up; it must
                            never sit above the card title. No `uppercase` /
                            `tracking-wider` on either badge and no bold on
                            the status — same call as the sidebar node
                            header, which had the same combination. */}
                        <div className="flex items-center gap-2">
                          <Badge
                            tone="accent"
                            className="text-11"
                            data-node-type={node.node_type}
                          >
                            {labelOf(node.node_type)}
                          </Badge>
                          {node.sub_type && (
                            <Badge
                              tone="neutral"
                              className="text-11"
                              data-sub-type={node.sub_type}
                              title={node.sub_type}
                            >
                              {subLabelOf(node.node_type, node.sub_type)}
                            </Badge>
                          )}
                          <span
                            className={cn(
                              "text-11",
                              node.status === "active"
                                ? accentText
                                : "text-text-tertiary ",
                            )}
                          >
                            {node.status}
                          </span>
                        </div>

                        {/* Content summary */}
                        <p className="text-xs text-text-secondary ">
                          {truncateContent(node.content)}
                        </p>

                        {/* Bottom row: score + decay + date.
                            Episodic nodes carry `importance` (重要程度) but no
                            `confidence`; the other three types carry `confidence`
                            (置信度) but no `importance`. Each is shown verbatim —
                            the backend never derives one from the other. */}
                        <div className="flex items-center gap-2 text-11 text-text-tertiary ">
                          {node.node_type === "Episodic" ? (
                            <span>
                              {t("memoryNodeDetail.labelImportance")}:{" "}
                              {(node.importance * 100).toFixed(0)}%
                            </span>
                          ) : (
                            <span>
                              {t("memoryNodeDetail.labelConfidence")}:{" "}
                              {(node.confidence * 100).toFixed(0)}%
                            </span>
                          )}

                          <span>Decay: {node.decay_score.toFixed(2)}</span>

                          <span className="ml-auto">{formatDate(node.created_at)}</span>
                        </div>
                      </div>
                    </ListRow>
                  );
                })}
              </ListBox>
            </OverlayScrollbar>
          {pager}
        </>
      )}
    </div>
  );
}
