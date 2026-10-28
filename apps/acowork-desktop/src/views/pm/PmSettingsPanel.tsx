/**
 * PmSettingsPanel — pm 视图右侧设置面板（40px 齿轮轨唤出）。
 *
 * 布局镜像群聊的 `GroupMembersPanel`：capsule 外壳 + 左侧拖拽把手，
 * 宽度与 chat 右面板共享（AppLayout 的 rightWidth）。内容按功能分区
 * 卡片（`ListBox` + `ExpandableRow`，与 chat 右侧面板同一卡片语法），
 * 滚动条统一 `OverlayScrollbar`。全局设置仍走 SettingsPage nav 视图：
 * - 项目信息：常驻输入框（名字 / 备注，失焦自动保存）+ 项目图标选择器
 * - 成员管理：内嵌 `MemberManagerSection`（与群聊 dialog 同一份主体）
 * - 项目操作：删除项目（从 ProjectHeader 的 ⋮ 菜单迁入）
 * - 服务状态：pm 健康指示 + 重试（顶部 ServiceOfflineBanner 保留）
 *
 * 离线（healthy === false）时禁用全部写操作，与视图其余部分一致。
 */

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { CAPSULE_PANE_CN } from "../../components/common/capsule";
import { ExpandableRow, ListBox } from "../../components/common/list";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { ProjectIconPicker } from "../../components/common/ProjectIconPicker";
import { parseProjectIcon, type ProjectIconRef } from "../../lib/projectIcon";
import { showToast } from "../../components/common/ToastProvider";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { usePmHealthStore } from "../../stores/pm/healthStore";
import { MemberManagerSection } from "./MemberManagerDialog";
import { canGovernProject, useLocalMode, useSelfAccount } from "./pmMembers";
import type { PmProject } from "../../lib/pm-types";

interface PmSettingsPanelProps {
  width: number;
  onResizeStart?: (e: React.MouseEvent) => void;
  onClose: () => void;
}

export function PmSettingsPanel({ width, onResizeStart, onClose }: PmSettingsPanelProps) {
  const { t } = useTranslation();
  const project = usePmProjectStore((s) => s.selected);
  // The backend gates every project mutation on owner-or-admin (PM's
  // `require_governance`); mirror it so a plain member gets no affordance the
  // server would refuse. The roster half is gated inside `MemberManagerSection`
  // (shared with the group-chat dialog), so it is not repeated here.
  const self = useSelfAccount();
  const localMode = useLocalMode();
  const canGovern = canGovernProject(project, self, localMode);

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
          <span>{t("pm.settingsPanel.title")}</span>
          <button
            type="button"
            onClick={onClose}
            aria-label={t("common.close")}
            className="rounded px-1.5 text-sm text-text-tertiary hover:bg-zinc-100 hover:text-zinc-700 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
          >
            ×
          </button>
        </div>

        {!project ? (
          <div className="flex h-full flex-col items-center justify-center gap-2 p-6 text-center">
            <p className="text-xs text-text-tertiary">{t("pm.settingsPanel.noProject")}</p>
          </div>
        ) : (
          <OverlayScrollbar fill="flex" className="p-3">
            <div className="space-y-2">
              <SettingsCard title={t("pm.settingsPanel.info")}>
                <ProjectInfoSection project={project} canGovern={canGovern} />
              </SettingsCard>
              <SettingsCard title={t("pm.settingsPanel.members")}>
                <MemberManagerSection project={project} />
              </SettingsCard>
              {canGovern && (
                <SettingsCard title={t("pm.settingsPanel.actions")}>
                  <ProjectActionsSection project={project} />
                </SettingsCard>
              )}
              <SettingsCard title={t("pm.settingsPanel.service")}>
                <ServiceStatusSection />
              </SettingsCard>
            </div>
          </OverlayScrollbar>
        )}
      </div>
    </div>
  );
}

/** 统一卡片外壳：ListBox + 默认展开的 ExpandableRow（与 DocSettingsPanel 同一语法）。 */
function SettingsCard({
  title,
  children,
}: {
  title: string;
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
        bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-3"
      >
        {children}
      </ExpandableRow>
    </ListBox>
  );
}

/** 项目信息 — 常驻输入框（名字 / 备注，失焦自动保存）+ 项目图标选择器。 */
function ProjectInfoSection({ project, canGovern }: { project: PmProject; canGovern: boolean }) {
  const { t } = useTranslation();
  const updateProjectMeta = usePmProjectStore((s) => s.updateProjectMeta);
  const healthy = usePmHealthStore((s) => s.healthy);
  // 非 owner/admin 走同一套禁用：后端 `require_governance` 会 403，输入框只读
  // 展示（项目名/备注本身是可见信息）。
  const blocked = healthy === false || !canGovern;

  // 输入框为本地草稿（受控）；失焦 / Enter 时与非空旧值比较后回写，避免
  // 无关 keystroke 打到 store。project 变化（切换选中）时用最新值重置草稿。
  const [titleDraft, setTitleDraft] = useState(project.title);
  const [descDraft, setDescDraft] = useState(project.description);
  useEffect(() => {
    setTitleDraft(project.title);
    setDescDraft(project.description);
  }, [project.id, project.title, project.description]);

  const saveTitle = useCallback(async () => {
    const trimmed = titleDraft.trim();
    if (!trimmed || trimmed === project.title) {
      setTitleDraft(project.title); // 空 / 未变 → 回退，不发请求
      return;
    }
    // 名字只发 { title }，绝不触碰 metadata（后端对 metadata 整体替换）。
    const ok = await updateProjectMeta(project.id, { title: trimmed });
    if (ok) showToast({ type: "success", message: t("pm.projectUpdated") });
    else setTitleDraft(project.title);
  }, [titleDraft, project, updateProjectMeta, t]);

  const saveDesc = useCallback(async () => {
    if (descDraft === project.description) return;
    const ok = await updateProjectMeta(project.id, { description: descDraft });
    if (ok) showToast({ type: "success", message: t("pm.projectUpdated") });
    else setDescDraft(project.description);
  }, [descDraft, project, updateProjectMeta, t]);

  // 图标：后端对 metadata 整体替换，故必须 {...metadata, icon} 合并再回写。
  const currentIcon = parseProjectIcon(project.metadata);
  const saveIcon = useCallback(
    async (ref: ProjectIconRef | null) => {
      const ok = await updateProjectMeta(project.id, {
        metadata: { ...project.metadata, icon: ref },
      });
      if (ok) showToast({ type: "success", message: t("pm.iconUpdated") });
    },
    [project, updateProjectMeta, t],
  );

  return (
    <div className="space-y-2.5 text-xs">
      {/* 图标行（对标头像设置）：点击图标弹出选择器；旁边显示项目名 + ID */}
      <div className="flex items-center gap-4">
        <ProjectIconPicker
          project={project}
          value={currentIcon}
          disabled={blocked}
          onChange={(ref) => void saveIcon(ref)}
        />
        <div className="min-w-0">
          <p className="truncate text-sm font-medium text-text">{project.title}</p>
          <p className="mt-0.5 truncate text-10 text-text-tertiary" title={project.id}>
            ID: {project.id}
          </p>
        </div>
      </div>

      {/* 名字：常驻单行输入，失焦 / Enter 保存 */}
      <div className="space-y-1">
        <label className="block text-xs font-medium text-text-secondary">{t("pm.projectName")}</label>
        <input
          value={titleDraft}
          disabled={blocked}
          onChange={(e) => setTitleDraft(e.target.value)}
          onBlur={saveTitle}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              (e.target as HTMLInputElement).blur();
            }
          }}
          className="w-full rounded border border-input-border bg-input-bg px-2 py-1 text-sm font-semibold text-text outline-none focus:border-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-50"
          aria-label={t("pm.projectName")}
        />
      </div>

      {/* 备注：常驻多行输入，失焦保存 */}
      <div className="space-y-1">
        <label className="block text-xs font-medium text-text-secondary">{t("pm.projectRemark")}</label>
        <textarea
          value={descDraft}
          disabled={blocked}
          onChange={(e) => setDescDraft(e.target.value)}
          onBlur={saveDesc}
          className="w-full resize-y rounded border border-input-border bg-input-bg px-2 py-1 text-xs text-text-tertiary outline-none focus:border-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-50"
          rows={3}
          placeholder={t("pm.noDescription")}
          aria-label={t("pm.projectRemark")}
        />
      </div>
    </div>
  );
}

/** 项目操作 — 删除项目（级联语义确认，从 ProjectHeader ⋮ 菜单迁入）。 */
function ProjectActionsSection({ project }: { project: PmProject }) {
  const { t } = useTranslation();
  const deleteProject = usePmProjectStore((s) => s.deleteProject);
  const healthy = usePmHealthStore((s) => s.healthy);
  const offline = healthy === false;
  const [confirmDelete, setConfirmDelete] = useState(false);

  const handleDelete = useCallback(async () => {
    setConfirmDelete(false);
    const ok = await deleteProject(project.id);
    if (ok) showToast({ type: "success", message: t("pm.projectDeleted") });
  }, [project.id, deleteProject, t]);

  return (
    <>
      <button
        type="button"
        disabled={offline}
        onClick={() => setConfirmDelete(true)}
        className="w-full rounded-md border border-red-300/70 px-3 py-1.5 text-xs font-medium text-red-600 transition-colors hover:bg-red-50 disabled:cursor-not-allowed disabled:opacity-40 dark:border-red-800/70 dark:text-red-400 dark:hover:bg-red-950/40"
      >
        {t("common.deleteProject")}
      </button>

      <ConfirmDialog
        open={confirmDelete}
        title={t("pm.deleteProjectTitle")}
        message={t("pm.deleteProjectCascadeDesc", { name: project.title })}
        confirmLabel={t("common.delete")}
        destructive
        onConfirm={handleDelete}
        onCancel={() => setConfirmDelete(false)}
      />
    </>
  );
}

/** 服务状态 — pm 健康指示 + 手动重试（ProjectsView 已有 30s 轮询）。 */
function ServiceStatusSection() {
  const { t } = useTranslation();
  const healthy = usePmHealthStore((s) => s.healthy);
  const checking = usePmHealthStore((s) => s.checking);
  const check = usePmHealthStore((s) => s.check);

  const state =
    healthy === true
      ? { label: t("pm.settingsPanel.serviceOnline"), cls: "text-emerald-600 dark:text-emerald-400" }
      : healthy === false
        ? { label: t("pm.settingsPanel.serviceOffline"), cls: "text-red-600 dark:text-red-400" }
        : { label: t("pm.settingsPanel.serviceUnknown"), cls: "text-text-tertiary" };

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
