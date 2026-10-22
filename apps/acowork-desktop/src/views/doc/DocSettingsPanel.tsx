/**
 * DocSettingsPanel — docs 视图右侧设置面板（40px 齿轮轨唤出）。
 *
 * 布局镜像群聊的 `GroupMembersPanel`：capsule 外壳 + 左侧拖拽把手，
 * 宽度与 chat 右面板共享（AppLayout 的 rightWidth）。内容按功能分区
 * 卡片（`ListBox` + `ExpandableRow`，与 chat 右侧面板同一卡片语法），
 * 滚动条统一 `OverlayScrollbar`。全局设置仍走 SettingsPage nav 视图：
 * - 权限：左侧树选中节点（目录/文档）的访问权限编辑（设计 §9，仅
 *   `multi_user` 挂载；local 无账号体系，服务端恒放行）
 * - 回收站：打开现有居中 `TrashDialog`
 * - 服务状态：doc 健康指示 + 重试（视图其余部分已有 30s 轮询）
 */

import { useState } from "react";
import { Trash2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { CAPSULE_PANE_CN } from "../../components/common/capsule";
import { ExpandableRow, ListBox } from "../../components/common/list";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { useAuthStore } from "../../stores/authStore";
import { useDocHealthStore } from "../../stores/doc/healthStore";
import { useDocTreeStore } from "../../stores/doc/treeStore";
import { DocPermissionsCard } from "./DocPermissionsCard";
import { TrashDialog } from "./TrashDialog";

interface DocSettingsPanelProps {
  width: number;
  onResizeStart?: (e: React.MouseEvent) => void;
  onClose: () => void;
}

export function DocSettingsPanel({ width, onResizeStart, onClose }: DocSettingsPanelProps) {
  const { t } = useTranslation();
  const [trashOpen, setTrashOpen] = useState(false);
  const healthy = useDocHealthStore((s) => s.healthy);
  const mode = useAuthStore((s) => s.mode);
  const selectedNode = useDocTreeStore((s) => s.selectedNode);

  return (
    <div className="relative ml-1 flex shrink-0 flex-col" style={{ width }}>
      {onResizeStart && (
        <div
          className="group absolute -left-1 top-0 bottom-0 z-10 w-1 cursor-col-resize"
          onMouseDown={onResizeStart}
        >
          <div className="absolute inset-y-0 left-0 w-1 rounded-full transition-colors group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60" />
        </div>
      )}

      <div className={cn(CAPSULE_PANE_CN, "flex-1 bg-right-panel")}>
        {/* Header — 与 GroupMembersPanel 同一 line-box 高度契约，发丝线
            与中间内容列的头部横线对齐。 */}
        <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center justify-between border-b border-border-divider px-[var(--tab-px)] text-[length:var(--tab-font-size)] font-medium leading-[var(--tab-line-height)] text-text-tertiary">
          <span>{t("doc.settingsPanel.title")}</span>
          <button
            type="button"
            onClick={onClose}
            aria-label={t("common.close")}
            className="rounded px-1.5 text-sm text-text-tertiary hover:bg-zinc-100 hover:text-zinc-700 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
          >
            ×
          </button>
        </div>

        <OverlayScrollbar fill="flex" className="p-3">
          <div className="space-y-2">
            {/* 权限卡片 — 作用对象 = 树中选中的目录/文档（设计 §9）。
                local 模式无账号体系，不展示（服务端也恒放行）。 */}
            {mode === "multi_user" && selectedNode && (
              <SettingsCard
                title={t("doc.settingsPanel.permissions")}
                bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-3"
              >
                <DocPermissionsCard
                  kind={selectedNode.kind}
                  id={selectedNode.id}
                  name={selectedNode.name}
                />
              </SettingsCard>
            )}

            <SettingsCard
              title={t("doc.trash")}
              bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-3"
            >
              <button
                type="button"
                disabled={healthy === false}
                onClick={() => setTrashOpen(true)}
                className="flex w-full items-center gap-2 rounded-md border border-border-outer px-3 py-1.5 text-xs font-medium text-text-secondary transition-colors hover:bg-zinc-100 disabled:cursor-not-allowed disabled:opacity-40 dark:hover:bg-zinc-700"
              >
                <Trash2 className="h-3.5 w-3.5" aria-hidden />
                {t("doc.settingsPanel.openTrash")}
              </button>
            </SettingsCard>

            <SettingsCard
              title={t("doc.settingsPanel.service")}
              bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-3"
            >
              <ServiceStatusSection />
            </SettingsCard>
          </div>
        </OverlayScrollbar>
      </div>

      <TrashDialog open={trashOpen} onClose={() => setTrashOpen(false)} disabled={healthy === false} />
    </div>
  );
}

/** 统一卡片外壳：ListBox + 默认展开的 ExpandableRow。 */
function SettingsCard({
  title,
  bodyClassName,
  children,
}: {
  title: string;
  bodyClassName: string;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useState(true);
  return (
    <ListBox dividers={false}>
      <ExpandableRow
        open={open}
        onToggle={() => setOpen((v) => !v)}
        title={title}
        ariaLabel={title}
        bodyClassName={bodyClassName}
      >
        {children}
      </ExpandableRow>
    </ListBox>
  );
}

/** 服务状态 — doc 健康指示 + 手动重试。 */
function ServiceStatusSection() {
  const { t } = useTranslation();
  const healthy = useDocHealthStore((s) => s.healthy);
  const checking = useDocHealthStore((s) => s.checking);
  const check = useDocHealthStore((s) => s.check);

  const state =
    healthy === true
      ? { label: t("doc.settingsPanel.serviceOnline"), cls: "text-emerald-600 dark:text-emerald-400" }
      : healthy === false
        ? { label: t("doc.settingsPanel.serviceOffline"), cls: "text-red-600 dark:text-red-400" }
        : { label: t("doc.settingsPanel.serviceUnknown"), cls: "text-text-tertiary" };

  return (
    <div className="flex items-center justify-between gap-2">
      <span className={cn("text-xs font-medium", state.cls)}>{state.label}</span>
      <button
        type="button"
        disabled={checking}
        onClick={() => void check()}
        className="rounded-md px-2 py-1 text-11 font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-40 dark:hover:bg-zinc-700"
      >
        {t("common.retry")}
      </button>
    </div>
  );
}
