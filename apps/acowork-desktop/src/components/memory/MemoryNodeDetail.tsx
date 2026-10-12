import React, { useCallback, useMemo } from "react";
import type { MemoryNodeResponse } from "../../lib/types";
import { cn } from "../../lib/utils";
import { ArrowLeft, Copy, FileJson, FileText, Trash2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useNodeTypeLabel, useSubTypeLabel } from "./nodeTypeI18n";
import {
  ContextMenu,
  useContextMenu,
  type ContextMenuItem,
} from "../common/ContextMenu";
import { copySelectionOrFallback, copyText } from "../../lib/clipboard";
import { Badge } from "../common/list";
import { OverlayScrollbar } from "../common/OverlayScrollbar";

interface MemoryNodeDetailProps {
  node: MemoryNodeResponse;
  onClose: () => void;
  onDelete: (nodeId: number) => void;
}

const accentText = "text-[var(--color-accent)]";

function getDecayColor(_score: number): string {
  return "bg-[var(--color-accent)]";
}

function getDecayTier(score: number): "Stable" | "Decaying" | "Critical" {
  if (score <= 0.3) return "Stable";
  if (score <= 0.7) return "Decaying";
  return "Critical";
}

function formatDate(ts: number): string {
  if (ts === 0) return "—";
  const d = new Date(ts * 1000);
  return d.toLocaleString();
}

export function MemoryNodeDetail({ node, onClose, onDelete }: MemoryNodeDetailProps) {
  const { t } = useTranslation();
  const labelOf = useNodeTypeLabel();
  const subLabelOf = useSubTypeLabel();
  const decayLabel = (() => {
    const tier = getDecayTier(node.decay_score);
    if (tier === "Stable") return t("memoryNodeDetail.statusStable");
    if (tier === "Decaying") return t("memoryNodeDetail.statusDecaying");
    return t("memoryNodeDetail.statusCritical");
  })();

  // ── Right-click context menu ──────────────────────────────────────────
  // Three copy variants mirror the panel's three audiences for the same
  // underlying record:
  //
  //   - Copy                — selection-at-open, falls back to `node.content`.
  //                           Same UX as MessageBubble's Copy: if the user
  //                           drag-selected some text on the panel, that wins;
  //                           otherwise the whole content body is copied.
  //   - Copy Node (JSON)    — the literal "整条记忆存储信息": the entire
  //                           MemoryNodeResponse payload, pretty-printed. This
  //                           is what the backend actually stores — useful for
  //                           filing bugs, piping into a diff tool, etc.
  //   - Copy Formatted      — a human-readable snapshot of every field the
  //                           panel renders on screen (content + metadata +
  //                           decay tier + localisable labels), which is the
  //                           easiest to paste into a chat or a note.
  //
  // All three go through the shared `lib/clipboard` helpers so the
  // WKWebView-aware fallback (navigator.clipboard + execCommand textarea)
  // applies uniformly. The selection snapshot is captured at right-click
  // time inside `useContextMenu.openAt` — reading `window.getSelection()`
  // at button-click time would be unreliable because the menu's own
  // `<button>` focus clears the page selection in WKWebView (the same bug
  // MessageBubble hit before its rewrite).
  const menu = useContextMenu();

  // Resolve the localised type/sub-type strings once per render. Captured
  // here (not in `useMemo`'s deps) so the items array only rebuilds when
  // the resolved strings themselves change — not when the i18n closure
  // identity churns.
  const typeLabelStr = labelOf(node.node_type);
  const subTypeLabelStr = node.sub_type ? subLabelOf(node.node_type, node.sub_type) : null;

  const handleCopySelection = useCallback((selectionAtOpen: string) => {
    // `selectionAtOpen` is the snapshot captured at right-click time inside
    // useContextMenu — it's the source of truth, because the menu's own
    // <button> focus will have cleared window.getSelection() by the time
    // this onClick fires (WKWebView quirk). `copySelectionOrFallback`
    // internally re-reads the (now-empty) selection and falls back to the
    // string we hand it, which is exactly what we want.
    void copySelectionOrFallback(selectionAtOpen || (node.content ?? ""));
  }, [node.content]);

  const handleCopyJson = useCallback(() => {
    void copyText(JSON.stringify(node, null, 2));
  }, [node]);

  const handleCopyFormatted = useCallback(() => {
    const tier = getDecayTier(node.decay_score);
    const tierLabel =
      tier === "Stable" ? t("memoryNodeDetail.statusStable")
      : tier === "Decaying" ? t("memoryNodeDetail.statusDecaying")
      : t("memoryNodeDetail.statusCritical");
    const scoreLine =
      node.node_type === "Episodic"
        ? `${t("memoryNodeDetail.labelImportance")}: ${(node.importance * 100).toFixed(1)}%`
        : `${t("memoryNodeDetail.labelConfidence")}: ${(node.confidence * 100).toFixed(1)}%`;
    const lines = [
      `Memory Node #${node.node_id}`,
      `${t("memoryNodeDetail.labelStatus")}: ${node.status}`,
      scoreLine,
      ...(node.node_type === "Knowledge"
        ? [`${t("memoryNodeDetail.labelImportance")}: ${(node.importance * 100).toFixed(1)}%`]
        : []),
      `Type: ${typeLabelStr}${node.sub_type ? ` (${subTypeLabelStr ?? node.sub_type})` : ""}`,
      `${t("memoryNodeDetail.labelCreated")}: ${formatDate(node.created_at)}`,
      `${t("memoryNodeDetail.labelLastAccessed")}: ${formatDate(node.last_accessed_at)}`,
      `${t("memoryNodeDetail.labelAccessCount")}: ${node.access_count}`,
      `Decay: ${node.decay_score.toFixed(3)} (${tierLabel})`,
      "",
      "---",
      "",
      node.content,
    ];
    void copyText(lines.join("\n"));
  }, [node, t, typeLabelStr, subTypeLabelStr]);

  const items = useMemo<ContextMenuItem[]>(() => {
    return [
      {
        key: "copy",
        icon: <Copy size={14} />,
        label: t("common.copy"),
        onClick: ({ selectionAtOpen }) => handleCopySelection(selectionAtOpen),
      },
      {
        key: "copy-node-json",
        icon: <FileJson size={14} />,
        label: t("memoryNodeDetail.contextMenuCopyNodeJson"),
        onClick: () => handleCopyJson(),
        dividerBefore: true,
      },
      {
        key: "copy-formatted",
        icon: <FileText size={14} />,
        label: t("memoryNodeDetail.contextMenuCopyFormatted"),
        onClick: () => handleCopyFormatted(),
      },
    ];
  }, [t, handleCopySelection, handleCopyJson, handleCopyFormatted]);

  const onContextMenu = useCallback(
    (e: React.MouseEvent) => menu.openAt(e),
    [menu],
  );

  const handleDelete = () => {
    if (confirm(`Delete node #${node.node_id}? This action cannot be undone.`)) {
      onDelete(node.node_id);
      onClose();
    }
  };

  return (
    <div
      className="flex flex-1 flex-col overflow-hidden rounded-md border border-zinc-400/70 bg-panel-inset-2 dark:border-zinc-600"
      onContextMenu={onContextMenu}
    >
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border-divider px-3 py-2">
        <button
          onClick={onClose}
          className="inline-flex items-center gap-1 rounded p-0.5 text-11 text-text-tertiary hover:bg-zinc-100  dark:hover:bg-zinc-800"
          aria-label={t("memoryNodeDetail.ariaLabelBackToList")}
        >
          <ArrowLeft className="h-3.5 w-3.5" />
          {t("common.back")}
        </button>
        <div className="flex items-center gap-1.5">
          {/* Shared Badge (kills the hand-rolled pill + the uppercase /
              tracking-wider), on `text-11` — same step as the list rows,
              i.e. the card-title size and never above it. */}
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
          <span className="text-11 text-text-tertiary ">#{node.node_id}</span>
        </div>
      </div>

      {/* Content */}
      <OverlayScrollbar dataTabScroll className="flex-1 p-3">
        {/* Full content */}
        <div className="mb-3">
          <h3 className="mb-1 text-11 font-medium text-text-tertiary ">{t("memoryNodeDetail.content")}</h3>
          <p className="whitespace-pre-wrap text-xs text-text ">{node.content}</p>
        </div>

        {/* Metadata grid.
            Episodic nodes carry `importance` (重要程度) but no `confidence`;
            the other three types carry `confidence` (置信度). Knowledge is
            the only type that carries BOTH, so it shows both verbatim. */}
        <div className="mb-3 grid grid-cols-2 gap-2">
          <MetaItem label={t("memoryNodeDetail.labelStatus")} value={node.status} />
          {node.node_type === "Episodic" ? (
            <MetaItem
              label={t("memoryNodeDetail.labelImportance")}
              value={`${(node.importance * 100).toFixed(1)}%`}
            />
          ) : (
            <MetaItem
              label={t("memoryNodeDetail.labelConfidence")}
              value={`${(node.confidence * 100).toFixed(1)}%`}
            />
          )}
          {node.node_type === "Knowledge" && (
            <MetaItem
              label={t("memoryNodeDetail.labelImportance")}
              value={`${(node.importance * 100).toFixed(1)}%`}
            />
          )}
          <MetaItem label={t("memoryNodeDetail.labelAccessCount")} value={String(node.access_count)} />
          <MetaItem label={t("memoryNodeDetail.labelCreated")} value={formatDate(node.created_at)} />
          <MetaItem label={t("memoryNodeDetail.labelLastAccessed")} value={formatDate(node.last_accessed_at)} />
        </div>

        {/* Decay score visualization */}
        <div className="mb-3">
          <div className="mb-1 flex items-center justify-between">
            <h3 className="text-11 font-medium text-text-tertiary ">Decay Score</h3>
            <span
              className={cn(
                "text-11 font-medium",
                accentText,
              )}
            >
              {decayLabel}
            </span>
          </div>
          <div className="h-2 w-full overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
            <div
              className={cn("h-full rounded-full transition-all", getDecayColor(node.decay_score))}
              style={{ width: `${node.decay_score * 100}%` }}
            />
          </div>
          <p className="mt-1 text-right text-11 text-text-tertiary ">
            {node.decay_score.toFixed(3)}
          </p>
        </div>
      </OverlayScrollbar>

      {/* Actions footer */}
      <div className="border-t border-border-divider p-3">
        <button
          onClick={handleDelete}
          className="inline-flex w-full items-center justify-center gap-1 rounded border border-red-200 bg-red-50 px-2 py-1.5 text-11 font-medium text-red-700 hover:bg-red-100 dark:border-red-900 dark:bg-red-950 dark:text-red-400 dark:hover:bg-red-900"
        >
          <Trash2 className="h-3 w-3" />
          Delete Node
        </button>
      </div>

      <ContextMenu
        isOpen={menu.isOpen}
        menuProps={menu.menuProps}
        items={items}
        payload={undefined}
        selectionAtOpen={menu.selectionAtOpen}
        onClose={menu.close}
        compact
      />
    </div>
  );
}

function MetaItem({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <p className="text-10 uppercase tracking-wider text-text-tertiary ">{label}</p>
      <p className="mt-0.5 text-11 text-text-secondary ">{value}</p>
    </div>
  );
}
