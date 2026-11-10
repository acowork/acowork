/**
 * ProjectHeader — 项目头部（T2-4）。
 *
 * 对齐 UX 设计 §3.2：
 * - 标题 inline-edit（点击 → input，blur/Enter 保存）
 * - 描述单行展示，hover 编辑（点击展开为 textarea）
 * - 统计（总任务 / 进行中 / 待审核 / 已完成）
 * - 卡片 / 列表视图切换（segmented，紧邻「新建任务」）
 * - [+ 新建任务] 按钮
 *
 * 视图切换刻意**不受离线门控**：列表视图是纯读的，PM 服务不可用时看数据
 * 反而更有用。这里只负责渲染与上抛意图，metadata 落盘与失败回滚都在
 * `ProjectBoard`（它同时持有看板与列表两种视图的挂载点）。写操作（新建
 * 任务）仍按 `healthy === false` 置灰，与看板一致。
 *
 * 成员管理与删除项目已从头部退出：它们迁入右侧 40px 齿轮轨唤出的
 * `PmSettingsPanel`，与 chat 视图的右面板风格统一。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmHealthStore } from "../../stores/pm/healthStore";
import { showToast } from "../../components/common/ToastProvider";
import { ProjectAvatar } from "../../components/common/ProjectAvatar";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import type { PmViewMode } from "../../lib/pm-view-settings";
import type { PmProject } from "../../lib/pm-types";

/**
 * 两个视图选项。图标与 `docs/prototypes/pm-list-view.html` 的
 * `.viewswitch` 一致（13px 描线），文案/提示走 i18n。
 */
const VIEW_OPTIONS: {
  key: PmViewMode;
  labelKey: string;
  hintKey: string;
  icon: React.ReactNode;
}[] = [
  {
    key: "board",
    labelKey: "pm.view.board",
    hintKey: "pm.view.boardHint",
    icon: (
      <svg viewBox="0 0 16 16" className="h-3.5 w-3.5 shrink-0" fill="none" stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
        <rect x="1.8" y="2.4" width="3.4" height="11.2" rx="1" />
        <rect x="6.3" y="2.4" width="3.4" height="8" rx="1" />
        <rect x="10.8" y="2.4" width="3.4" height="5.4" rx="1" />
      </svg>
    ),
  },
  {
    key: "list",
    labelKey: "pm.view.list",
    hintKey: "pm.view.listHint",
    icon: (
      <svg viewBox="0 0 16 16" className="h-3.5 w-3.5 shrink-0" fill="none" stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
        <path d="M2 4h12M2 8h12M2 12h8" />
      </svg>
    ),
  },
];

interface ProjectHeaderProps {
  project: PmProject;
  onNewTask: () => void;
  /** 当前任务视图（`metadata.pmView` 解析而来），由 `ProjectBoard` 持有。 */
  view: PmViewMode;
  /** 用户点击切换 → 上抛意图；写 metadata 与回滚在 `ProjectBoard`。 */
  onViewChange: (view: PmViewMode) => void;
}

export function ProjectHeader({
  project,
  onNewTask,
  view,
  onViewChange,
}: ProjectHeaderProps) {
  const { t } = useTranslation();
  const updateProjectMeta = usePmProjectStore((s) => s.updateProjectMeta);
  const tasks = usePmBoardStore((s) => s.tasks);
  const healthy = usePmHealthStore((s) => s.healthy);
  const offline = healthy === false;

  const [editingTitle, setEditingTitle] = useState(false);
  const [titleDraft, setTitleDraft] = useState(project.title);
  const [editingDesc, setEditingDesc] = useState(false);
  const [descDraft, setDescDraft] = useState(project.description);
  const titleInputRef = useRef<HTMLInputElement>(null);

  // 统计：从看板任务计算
  const total = tasks.length;
  const inProgress = tasks.filter((t) => t.status === "in_progress").length;
  const submitted = tasks.filter((t) => t.status === "submitted").length;
  const done = tasks.filter((t) => t.status === "done").length;

  useEffect(() => {
    if (editingTitle) titleInputRef.current?.focus();
  }, [editingTitle]);

  const saveTitle = useCallback(async () => {
    const trimmed = titleDraft.trim();
    setEditingTitle(false);
    if (!trimmed || trimmed === project.title) return;
    const ok = await updateProjectMeta(project.id, { title: trimmed });
    if (ok) {
      showToast({ type: "success", message: t("pm.projectUpdated") });
    }
  }, [titleDraft, project, updateProjectMeta, t]);

  const saveDesc = useCallback(async () => {
    setEditingDesc(false);
    if (descDraft === project.description) return;
    const ok = await updateProjectMeta(project.id, { description: descDraft });
    if (ok) {
      showToast({ type: "success", message: t("pm.projectUpdated") });
    }
  }, [descDraft, project, updateProjectMeta, t]);

  return (
    <header className="shrink-0 border-b border-border-divider px-4 py-3">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          {/* 标题 inline-edit（前置项目图标） */}
          <div className="flex items-center gap-2">
            <ProjectAvatar project={project} size={22} className="shrink-0" />
            {editingTitle ? (
            <input
              ref={titleInputRef}
              value={titleDraft}
              onChange={(e) => setTitleDraft(e.target.value)}
              onBlur={saveTitle}
              onKeyDown={(e) => {
                if (e.key === "Enter") saveTitle();
                if (e.key === "Escape") {
                  setTitleDraft(project.title);
                  setEditingTitle(false);
                }
              }}
              className="w-full rounded border border-input-border bg-input-bg px-2 py-0.5 text-base font-semibold text-text outline-none focus:border-[var(--color-accent)]"
              aria-label={t("pm.projectTitleEdit")}
            />
          ) : (
            <h2
              className="cursor-text text-base font-semibold text-text hover:text-zinc-600  dark:hover:text-zinc-300"
              onClick={() => {
                setTitleDraft(project.title);
                setEditingTitle(true);
              }}
              title={t("pm.clickToEdit")}
              tabIndex={0}
              role="button"
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  setTitleDraft(project.title);
                  setEditingTitle(true);
                }
              }}
            >
              {project.title}
            </h2>
          )}
          </div>

          {/* 描述 hover 编辑 */}
          {editingDesc ? (
            <textarea
              value={descDraft}
              onChange={(e) => setDescDraft(e.target.value)}
              onBlur={saveDesc}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  setDescDraft(project.description);
                  setEditingDesc(false);
                }
              }}
              className="mt-1 w-full resize-y rounded border border-input-border bg-input-bg px-2 py-1 text-xs text-text-tertiary outline-none focus:border-[var(--color-accent)]"
              rows={2}
              aria-label={t("pm.projectDescEdit")}
            />
          ) : (
            <p
              className="mt-1 line-clamp-1 cursor-text text-xs text-text-tertiary hover:text-zinc-400  dark:hover:text-zinc-300"
              onClick={() => {
                setDescDraft(project.description);
                setEditingDesc(true);
              }}
              title={t("pm.clickToEdit")}
            >
              {project.description || <span className="italic opacity-60">{t("pm.noDescription")}</span>}
            </p>
          )}

          {/* 统计 */}
          <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-11 text-text-tertiary ">
            <span>
              {t("pm.statTotal")}: <strong className="tabular-nums">{total}</strong>
            </span>
            <span>
              {t("pm.board.inProgress")}: <strong className="tabular-nums">{inProgress}</strong>
            </span>
            {submitted > 0 && (
              <span className="text-amber-600 dark:text-amber-400">
                {t("pm.board.submitted")}: <strong className="tabular-nums">{submitted}</strong>
              </span>
            )}
            <span>
              {t("pm.board.done")}: <strong className="tabular-nums">{done}</strong>
            </span>
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-1">
          {/* 卡片 / 列表视图切换 —— 离线不禁用（见文件头注释） */}
          <div
            role="group"
            aria-label={t("pm.view.ariaLabel")}
            className="mr-1 flex items-center gap-0.5 rounded-md border border-border-outer bg-panel-block p-0.5"
          >
            {VIEW_OPTIONS.map((opt) => {
              const active = view === opt.key;
              return (
                <button
                  key={opt.key}
                  type="button"
                  aria-pressed={active}
                  onClick={() => onViewChange(opt.key)}
                  title={t(opt.hintKey)}
                  className={cn(
                    "flex h-6 items-center gap-1 rounded px-2 text-11 font-medium transition-colors",
                    active
                      ? "bg-page-bg text-text shadow-sm"
                      : "text-text-tertiary hover:text-text",
                  )}
                >
                  {opt.icon}
                  {t(opt.labelKey)}
                </button>
              );
            })}
          </div>
          <button
            type="button"
            onClick={onNewTask}
            disabled={offline}
            className="rounded-md bg-zinc-800 px-2.5 py-1 text-xs font-medium text-white hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-40 dark:bg-zinc-700 dark:hover:bg-zinc-600"
          >
            + {t("pm.newTask")}
          </button>
        </div>
      </div>
    </header>
  );
}
