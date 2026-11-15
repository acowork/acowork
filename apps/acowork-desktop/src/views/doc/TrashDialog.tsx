/**
 * TrashDialog — 回收站列表（设计 §3.3：30 天自动清理）。
 *
 * 展示 TrashEntry（标题/原路径/删除时间/大小）；支持恢复（restore 到原目录，
 * 服务端重新生成 doc_id）与永久删除（purge，二次确认）。
 */

import { useCallback, useEffect, useState } from "react";
import { ArchiveRestore, Loader2, Trash2, X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import * as docApi from "../../lib/doc-api";
import { useToast } from "../../components/common/ToastProvider";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { useDocTreeStore } from "../../stores/doc/treeStore";
import { useDocEditorStore } from "../../stores/doc/editorStore";
import type { TrashEntry } from "../../lib/doc-types";
import { listRowHover } from "../../lib/ui-styles";

interface TrashDialogProps {
  open: boolean;
  onClose: () => void;
  disabled?: boolean;
}

export function TrashDialog({ open, onClose, disabled }: TrashDialogProps) {
  const { t } = useTranslation();
  const toast = useToast();
  const [entries, setEntries] = useState<TrashEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [purgeTarget, setPurgeTarget] = useState<TrashEntry | null>(null);

  const load = useCallback(async () => {
    if (!open || disabled) return;
    setLoading(true);
    try {
      const list = await docApi.listTrash();
      setEntries(list);
    } catch (e) {
      toast.addToast({ type: "error", message: e instanceof Error ? e.message : String(e) });
    } finally {
      setLoading(false);
    }
  }, [open, disabled, toast]);

  useEffect(() => {
    if (open) void load();
  }, [open, load]);

  // Esc 关闭
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [open, onClose]);

  if (!open) return null;

  const handleRestore = async (entry: TrashEntry) => {
    try {
      const meta = await docApi.restoreTrash(entry.trash_id);
      // 恢复会落到原目录（服务端保留 original_dir_id）→ 刷新恢复目录并打开
      await useDocTreeStore.getState().refreshDir(entry.original_dir_id);
      await useDocEditorStore.getState().requestOpen(meta.doc_id);
      toast.addToast({ type: "success", message: t("doc.restored", { name: entry.original_name }) });
      await load();
    } catch (e) {
      toast.addToast({ type: "error", message: e instanceof Error ? e.message : String(e) });
    }
  };

  const handlePurge = async () => {
    if (!purgeTarget) return;
    try {
      await docApi.purgeTrash(purgeTarget.trash_id);
      toast.addToast({ type: "success", message: t("doc.purged", { name: purgeTarget.original_name }) });
      setPurgeTarget(null);
      await load();
    } catch (e) {
      toast.addToast({ type: "error", message: e instanceof Error ? e.message : String(e) });
    }
  };

  const list = entries ?? [];
  const isEmpty = !loading && list.length === 0;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div
        className="absolute inset-0 bg-black/40"
        onClick={onClose}
        aria-hidden
      />
      <div
        role="dialog"
        aria-label={t("doc.trash")}
        className="relative z-10 flex max-h-[70vh] w-[440px] flex-col overflow-hidden rounded-lg border border-border-outer bg-surface shadow-xl dark:bg-zinc-900"
      >
        <div className="flex items-center gap-2 border-b border-border-divider px-3 py-2">
          <Trash2 className="h-4 w-4 text-text-tertiary" aria-hidden />
          <span className="flex-1 text-sm font-medium text-text-secondary ">
            {t("doc.trash")}
          </span>
          <button
            type="button"
            aria-label={t("common.close")}
            onClick={onClose}
            className="rounded p-1 text-text-tertiary hover:bg-[var(--color-accent)]/10 hover:text-text-secondary dark:hover:bg-[var(--color-accent)]/15"
          >
            <X className="h-4 w-4" />
          </button>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto p-2">
          {loading && (
            <div className="flex items-center justify-center gap-2 py-6 text-text-tertiary">
              <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
            </div>
          )}
          {isEmpty && (
            <div className="py-8 text-center text-xs text-text-tertiary">{t("doc.trashEmpty")}</div>
          )}
          {!loading &&
            list.map((entry) => (
              <div
                key={entry.trash_id}
                className={`group flex items-center gap-2 rounded-md px-2 py-1.5 ${listRowHover}`}
              >
                <div className="min-w-0 flex-1">
                  <div className="truncate text-xs text-text-secondary ">
                    {entry.original_name}
                  </div>
                  <div className="truncate text-10 text-text-tertiary">
                    {new Date(entry.deleted_at).toLocaleString()} · {fmtSize(entry.file_size_bytes)}
                  </div>
                </div>
                <div className="hidden shrink-0 items-center gap-1 group-hover:flex group-focus-within:flex">
                  {entry.doc_id && (
                    <button
                      type="button"
                      title={t("doc.restore")}
                      aria-label={t("doc.restore")}
                      onClick={() => void handleRestore(entry)}
                      className="rounded p-1 text-text-tertiary hover:bg-[var(--color-accent)]/10 hover:text-ok dark:hover:bg-[var(--color-accent)]/15"
                    >
                      <ArchiveRestore className="h-3.5 w-3.5" />
                    </button>
                  )}
                  <button
                    type="button"
                    title={t("doc.purge")}
                    aria-label={t("doc.purge")}
                    onClick={() => setPurgeTarget(entry)}
                    className="rounded p-1 text-text-tertiary hover:bg-[var(--color-accent)]/10 hover:text-danger-icon dark:hover:bg-[var(--color-accent)]/15"
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                  </button>
                </div>
              </div>
            ))}
        </div>

        {!isEmpty && (
          <div className="border-t border-border-divider px-3 py-1.5 text-10 text-text-tertiary">
            {t("doc.trashNote")}
          </div>
        )}
      </div>

      <ConfirmDialog
        open={purgeTarget !== null}
        title={t("doc.purgeTitle")}
        message={
          purgeTarget ? t("doc.purgeMsg", { name: purgeTarget.original_name }) : ""
        }
        confirmLabel={t("doc.purge")}
        destructive
        onCancel={() => setPurgeTarget(null)}
        onConfirm={() => void handlePurge()}
      />
    </div>
  );
}

function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
