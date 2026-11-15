/**
 * ReviewQueue — doc 视图顶部「待审核更新请求」条（设计 §5 PR 式审核流）。
 *
 * - 常驻顶条显示 pending 计数 badge；点击展开面板列出请求。
 * - 请求行：目标文档路径 + 提交者 + base_version + 时间；点击可打开目标文档
 *   对照内容；inline [批准]；[拒绝] → NoteDialog（可选原因）。
 * - approve 合并入库（doc version+1，服务端完成）；reject 保留原因。
 * - 面板底部「刷新」重新拉 pending（Agent 新提交实时可见）。
 */

import { useEffect, useState } from "react";
import { Check, ChevronDown, ChevronUp, Inbox, Loader2, RefreshCw, X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useDocRequestStore } from "../../stores/doc/requestStore";
import { useDocEditorStore } from "../../stores/doc/editorStore";
import { useDocHealthStore } from "../../stores/doc/healthStore";
import { useAgentStore } from "../../stores/agentStore";
import { useToast } from "../../components/common/ToastProvider";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";
import type { UpdateRequest } from "../../lib/doc-types";

/** 解析 instance_id → 显示名（meta.display_name ?? meta.name ?? id）。
 *  agentStore 按 instance_id（UUID, ADR-073）索引，而 `submitted_by`
 *  服务端就存的是 instance_id —— 键即查表 key。 */
function resolveAgentName(
  agents: Record<string, { meta?: { display_name?: string; name?: string } }>,
  id: string | null,
): string | null {
  if (!id) return null;
  const a = agents[id];
  if (!a?.meta) return id;
  return a.meta.display_name || a.meta.name || id;
}

export function ReviewQueue() {
  const { t } = useTranslation();
  const toast = useToast();
  const healthy = useDocHealthStore((s) => s.healthy);
  const requests = useDocRequestStore((s) => s.requests);
  const loading = useDocRequestStore((s) => s.loading);
  const error = useDocRequestStore((s) => s.error);
  const loadPending = useDocRequestStore((s) => s.loadPending);
  const approve = useDocRequestStore((s) => s.approve);
  const reject = useDocRequestStore((s) => s.reject);
  // ADR-073: `submitted_by` 在服务端存的是 instance_id（不是包 id），
  // UI 用 agentStore 解析为人类可读名。Store 未加载时 fallback 到原始
  // instance_id —— 至少保留可追溯性，不会显示空字符串。
  const agents = useAgentStore((s) => s.agents);
  const [open, setOpen] = useState(false);
  const [noteTarget, setNoteTarget] = useState<UpdateRequest | null>(null);
  const [note, setNote] = useState("");
  const [busyId, setBusyId] = useState<string | null>(null);

  // 首次挂载 + 30s 轮询 pending（Agent 提交后无需手动刷新即出现）
  useEffect(() => {
    if (healthy === false) return;
    void loadPending();
    const timer = setInterval(() => void loadPending(), 30_000);
    return () => clearInterval(timer);
  }, [healthy, loadPending]);

  const handleApprove = async (req: UpdateRequest) => {
    setBusyId(req.request_id);
    const ok = await approve(req);
    setBusyId(null);
    if (ok) {
      // 若该文档正在编辑，把合并后的内容推到编辑器（无 dirty 时直接替换）
      const ed = useDocEditorStore.getState();
      if (ed.doc?.meta.doc_id === req.doc_id) {
        ed.applyMergedUpdate(req.doc_id, req.content, ed.doc.meta.version + 1);
      }
      toast.addToast({ type: "success", message: t("doc.reviewApproved", { name: req.path }) });
    } else {
      toast.addToast({ type: "error", message: error ?? t("doc.reviewFailed") });
    }
  };

  const handleRejectConfirm = async () => {
    if (!noteTarget) return;
    const req = noteTarget;
    setBusyId(req.request_id);
    const ok = await reject(req, note.trim() || undefined);
    setBusyId(null);
    setNoteTarget(null);
    setNote("");
    if (ok) {
      toast.addToast({ type: "success", message: t("doc.reviewRejected", { name: req.path }) });
    } else {
      toast.addToast({ type: "error", message: error ?? t("doc.reviewFailed") });
    }
  };

  const openTarget = (docId: string) => {
    void useDocEditorStore.getState().requestOpen(docId);
  };

  const pendingCount = requests.length;

  return (
    <div className="shrink-0 border-b border-border-divider bg-page-bg">
      {/* ── 顶条 ──────────────────────────────────────────── */}
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className={cn(
          "flex w-full items-center gap-2 px-3 py-1 text-left text-11 transition-colors",
          pendingCount > 0
            ? "bg-warning-surface text-warning-quiet hover:bg-warning-surface-hover"
            : "text-text-tertiary hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15",
        )}
      >
        <Inbox className="h-3.5 w-3.5" aria-hidden />
        <span className="flex-1">
          {pendingCount > 0 ? t("doc.reviewPendingN", { count: pendingCount }) : t("doc.reviewEmpty")}
        </span>
        {open ? <ChevronUp className="h-3.5 w-3.5" /> : <ChevronDown className="h-3.5 w-3.5" />}
      </button>

      {/* ── 面板 ──────────────────────────────────────────── */}
      {open && (
        <div className="max-h-64 overflow-y-auto border-t border-border-divider px-1 py-1">
          {loading && (
            <div className="flex items-center justify-center gap-2 py-3 text-text-tertiary">
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
            </div>
          )}
          {!loading && pendingCount === 0 && (
            <div className="flex items-center justify-between px-2 py-2 text-11 text-text-tertiary">
              <span>{t("doc.reviewQueueEmpty")}</span>
              <button
                type="button"
                onClick={() => void loadPending()}
                className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-text-tertiary hover:bg-[var(--color-accent)]/10 hover:text-text-tertiary dark:hover:bg-[var(--color-accent)]/15"
              >
                <RefreshCw className="h-3 w-3" />
                {t("common.refresh")}
              </button>
            </div>
          )}
          {!loading &&
            requests.map((req) => (
              <div
                key={req.request_id}
                className={cn("group flex items-center gap-2 rounded-md px-2 py-1.5", listRowHover)}
              >
                <button
                  type="button"
                  onClick={() => openTarget(req.doc_id)}
                  className="min-w-0 flex-1 text-left"
                  title={t("doc.reviewOpenDoc")}
                >
                  <div className="flex items-center gap-1.5 truncate text-xs text-text-secondary ">
                    <span className="truncate font-medium">{req.path}</span>
                    <span className="shrink-0 rounded bg-surface px-1 text-10 text-text-tertiary ">
                      base v{req.base_version}
                    </span>
                  </div>
                  <div className="truncate text-10 text-text-tertiary">
                    {resolveAgentName(agents, req.submitted_by) ?? req.submitted_by} ·{" "}
                    {new Date(req.created_at).toLocaleString()}
                  </div>
                </button>
                <div className="flex shrink-0 items-center gap-1">
                  <button
                    type="button"
                    disabled={busyId === req.request_id}
                    onClick={() => void handleApprove(req)}
                    className="inline-flex items-center gap-0.5 rounded bg-ok-solid px-1.5 py-0.5 text-10 font-medium text-white hover:bg-ok-solid/90 disabled:opacity-40"
                  >
                    <Check className="h-2.5 w-2.5" aria-hidden />
                    {t("doc.approve")}
                  </button>
                  <button
                    type="button"
                    disabled={busyId === req.request_id}
                    onClick={() => {
                      setNoteTarget(req);
                      setNote("");
                    }}
                    className="inline-flex items-center gap-0.5 rounded border border-border-outer px-1.5 py-0.5 text-10 text-text-tertiary hover:bg-[var(--color-accent)]/10 disabled:opacity-40  dark:hover:bg-[var(--color-accent)]/15"
                  >
                    <X className="h-2.5 w-2.5" aria-hidden />
                    {t("doc.reject")}
                  </button>
                </div>
              </div>
            ))}
        </div>
      )}

      {/* ── 拒绝原因 dialog ───────────────────────────────── */}
      {noteTarget && (
        <div className="fixed inset-0 z-50 flex items-center justify-center">
          <div className="absolute inset-0 bg-black/40" onClick={() => setNoteTarget(null)} aria-hidden />
          <div
            role="dialog"
            aria-label={t("doc.reject")}
            className="relative z-10 w-[400px] rounded-lg border border-border-outer bg-surface p-3 shadow-xl dark:bg-zinc-900"
          >
            <div className="mb-2 text-sm font-medium text-text-secondary ">
              {t("doc.rejectTitle", { name: noteTarget.path })}
            </div>
            <textarea
              autoFocus
              value={note}
              onChange={(e) => setNote(e.target.value)}
              placeholder={t("doc.rejectNotePlaceholder")}
              rows={3}
              className="w-full resize-none rounded-md border border-input-border bg-input-bg px-2 py-1.5 text-xs outline-none focus:border-[var(--color-accent)]"
            />
            <div className="mt-2 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setNoteTarget(null)}
                className="rounded-md border border-border-outer px-2 py-1 text-xs text-text-secondary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
              >
                {t("common.cancel")}
              </button>
              <button
                type="button"
                disabled={busyId === noteTarget.request_id}
                onClick={() => void handleRejectConfirm()}
                className="rounded-md bg-danger-solid px-2 py-1 text-xs font-medium text-white hover:bg-danger-solid disabled:opacity-40"
              >
                {t("doc.rejectConfirm")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
