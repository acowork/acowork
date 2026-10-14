/**
 * DocsView — doc 视图壳（plan D2-1，替换 AppLayout docs TODO）。
 *
 * 布局：左侧 DocTreeSidebar（260px）+ 右侧列（ReviewQueue 顶条 + DocEditor）。
 * 职责：
 * - 进入视图：健康检查 + 30s 轮询；离线 → 右侧整体离线面板（重试按钮），
 *   sidebar 保留（可浏览已缓存树）；恢复后自动回到编辑器。
 * - 首次健康检查成功后加载根树（sidebar 内部做，这里只负责 health 编排）。
 */

import { useEffect } from "react";
import { BookOpen, Loader2, RefreshCw } from "lucide-react";
import { useTranslation } from "../i18n/useTranslation";
import { useDocHealthStore } from "../stores/doc/healthStore";
import { useDocTreeStore } from "../stores/doc/treeStore";
import { useDocRequestStore } from "../stores/doc/requestStore";
import { DocTreeSidebar } from "./doc/DocTreeSidebar";
import { CAPSULE_PANE_CN } from "../components/common/capsule";
import { cn } from "../lib/utils";
import { ReviewQueue } from "./doc/ReviewQueue";
import { DocEditor } from "./doc/DocEditor";
import { DOC_ROOT_DIR_ID } from "../lib/doc-types";
import { useDragResize } from "../hooks/useDragResize";
import { SplitHandle } from "../components/common/SplitHandle";

export function DocsView() {
  const { t } = useTranslation();
  const healthy = useDocHealthStore((s) => s.healthy);
  const checking = useDocHealthStore((s) => s.checking);
  const check = useDocHealthStore((s) => s.check);
  const loadDir = useDocTreeStore((s) => s.loadDir);
  const refreshVisible = useDocTreeStore((s) => s.refreshVisible);
  const loadPending = useDocRequestStore((s) => s.loadPending);

  // 进入视图：健康检查；检查成功（或已成功）时确保根树加载
  useEffect(() => {
    void check().then((ok) => {
      if (ok) {
        void loadDir(DOC_ROOT_DIR_ID);
        void loadPending();
      }
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 30s 健康轮询；离线 → 恢复在线时重载已展开目录树 + pending。
  // 用 `refreshVisible` 而非 `refreshDir(root)`：mcp 工具可能在已展开的子
  // 目录里加文档，仅刷根目录看不到。`refreshVisible` 强制刷所有已展开层。
  useEffect(() => {
    const timer = setInterval(() => {
      void check().then((ok) => {
        if (ok) {
          void refreshVisible();
          void loadPending();
        }
      });
    }, 30_000);
    return () => clearInterval(timer);
  }, [check, refreshVisible, loadPending]);

  // 与聊天 AgentList 一致的左侧分栏宽度（可拖动 + localStorage 持久化）
  const sidebar = useDragResize({
    storageKey: "acowork-doc-list-width",
    defaultWidth: 240,
    minWidth: 160,
    maxWidth: 400,
  });

  const offline = healthy === false;

  return (
    // `w-full` is load-bearing: this root is a flex item of AppLayout's
    // wrapper, and without it the default `flex: 0 1 auto` sizes the view
    // to its CONTENT — with no doc open that collapsed the whole layout to
    // the tree's width and left a wide strip of bare vibrancy on the right.
    // It was invisible while the wrapper painted the same `bg-page-bg`;
    // transparent wrappers expose it. ProjectsView already had w-full.
    <div className="flex h-full w-full overflow-hidden">
      {/* 左侧目录树（离线也可浏览缓存） */}
      <DocTreeSidebar width={sidebar.width} />
      <SplitHandle
        onMouseDown={sidebar.onHandleMouseDown}
        ariaLabel={t("appLayout.ariaLabelResizeSidebar")}
      />

      {/* 右侧列 */}
      <div className={cn(CAPSULE_PANE_CN, "h-full min-w-0 flex-1 bg-page-bg")}>
        {offline ? (
          <OfflinePanel
            checking={checking}
            onRetry={() => void check()}
            message={t("doc.offlineTitle")}
            hint={t("doc.offlineHint")}
          />
        ) : (
          <>
            <ReviewQueue />
            <div className="min-h-0 flex-1">
              <DocEditor />
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function OfflinePanel({
  checking,
  onRetry,
  message,
  hint,
}: {
  checking: boolean;
  onRetry: () => void;
  message: string;
  hint: string;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 p-8 text-center">
      <BookOpen className="h-10 w-10 text-text-secondary " aria-hidden />
      <div>
        <p className="text-sm font-medium text-text-secondary ">{message}</p>
        <p className="mt-1 max-w-sm text-xs leading-relaxed text-text-tertiary">{hint}</p>
      </div>
      <button
        type="button"
        onClick={onRetry}
        disabled={checking}
        className="inline-flex items-center gap-1.5 rounded-md bg-accent-strong px-3 py-1.5 text-xs font-medium text-white hover:opacity-90 disabled:opacity-60"
      >
        {checking ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <RefreshCw className="h-3.5 w-3.5" aria-hidden />}
        {t("common.retry")}
      </button>
    </div>
  );
}
