/**
 * ProjectSidebar — 左侧项目列表（T2-2）。
 *
 * 对齐 UX 设计 §3.1 + AgentList 风格（搜索框 / 列表 / 底部 +按钮 三段式）：
 * - 顶部搜索框（StyledInput + Search icon）
 * - 项目列表 + 选中高亮（与 AgentList 同款：accent/90 + text-white）
 * - 任务计数徽章（待审核数字色高亮）
 * - 底部独立 + 按钮（与 AgentList 同款：p-1.5 + hover:bg-nav-control）
 * - 新建项目（对话框）/ 删除项目（ConfirmDialog 二次确认）
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Plus, Search } from "lucide-react";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { usePmHealthStore } from "../../stores/pm/healthStore";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { StyledInput } from "../../components/common/StyledInput";
import { showToast } from "../../components/common/ToastProvider";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";
import { CAPSULE_PANE_CN } from "../../components/common/capsule";
import { useTranslation } from "../../i18n/useTranslation";
import type { PmProject } from "../../lib/pm-types";

export function ProjectSidebar({ width }: { width?: number }) {
  const { t } = useTranslation();
  const projects = usePmProjectStore((s) => s.projects);
  const selected = usePmProjectStore((s) => s.selected);
  const counts = usePmProjectStore((s) => s.counts);
  const loadingProjects = usePmProjectStore((s) => s.loading);
  const selectProject = usePmProjectStore((s) => s.selectProject);
  const createProject = usePmProjectStore((s) => s.createProject);
  const deleteProject = usePmProjectStore((s) => s.deleteProject);
  const creating = usePmProjectStore((s) => s.creating);
  const openCreate = usePmProjectStore((s) => s.openCreate);
  const closeCreate = usePmProjectStore((s) => s.closeCreate);
  const healthy = usePmHealthStore((s) => s.healthy);

  const [title, setTitle] = useState("");
  const [saving, setSaving] = useState(false);
  const [deleting, setDeleting] = useState<PmProject | null>(null);
  const [searchQuery, setSearchQuery] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  // 新建对话框打开时聚焦输入框（creating 现在是全局 store 状态）
  useEffect(() => {
    if (creating) inputRef.current?.focus();
  }, [creating]);

  const handleCreate = useCallback(async () => {
    const trimmed = title.trim();
    if (!trimmed) {
      showToast({ type: "warning", message: t("pm.newProjectTitleRequired") });
      return;
    }
    setSaving(true);
    const project = await createProject(trimmed);
    setSaving(false);
    if (project) {
      setTitle("");
      closeCreate();
      showToast({ type: "success", message: t("pm.projectCreated") });
    }
  }, [title, createProject, closeCreate, t]);

  const handleDelete = useCallback(async () => {
    if (!deleting) return;
    const ok = await deleteProject(deleting.id);
    setDeleting(null);
    if (ok) {
      showToast({ type: "success", message: t("pm.projectDeleted") });
    }
  }, [deleting, deleteProject, t]);

  // 搜索过滤：与 AgentList 一致的小写包含匹配；空串返回全部
  const filteredProjects = useMemo(() => {
    const q = searchQuery.trim().toLowerCase();
    if (!q) return projects;
    return projects.filter((p) => p.title.toLowerCase().includes(q));
  }, [projects, searchQuery]);

  return (
    <aside
      className={cn(CAPSULE_PANE_CN, "h-full shrink-0 bg-nav-surface text-xs")}
      style={{ width: width ?? 240 }}
    >
      {/* Header — search input (与 AgentList 同款)。高度绑定
          --ui-list-header-h，与 harness / settings 的标题栏同高，
          三个列表胶囊的分割线才能横向对齐。 */}
      {/* `border-b` matches the other sidebar headers (SectionPane's title
          row, DocTreeSidebar's title row) so the search band is separated
          from the project list the same way everywhere. The search box
          itself already has a border, which is what made the missing rule
          read as "intentional" here. */}
      <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center border-b border-border-divider px-3">
        <div className="relative min-w-0 flex-1">
          <Search className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-text-tertiary " />
          <StyledInput
            type="text"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            placeholder={t("pm.searchPlaceholder")}
            aria-label={t("pm.searchPlaceholder")}
            className="rounded-md bg-input-bg pl-7 py-1.5 pr-2"
          />
        </div>
      </div>

      {/* 项目列表 */}
      <nav
        // `py-1` matches SectionPane's list container (and harness/settings,
        // which renders through it) — without it the pm rows sit flush
        // against the header hairline while every other capsule list has
        // the same 4px breathing room top and bottom.
        className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden px-1.5 py-1"
        aria-label={t("pm.projectListAria")}
      >
        {filteredProjects.map((p, index) => {
          const active = selected?.id === p.id;
          const c = counts[p.id] ?? { total: 0, submitted: 0 };
          const isLast = index === filteredProjects.length - 1;
          return (
            <button
              key={p.id}
              type="button"
              onClick={() => selectProject(p.id)}
              className={cn(
                "relative flex w-full items-center gap-2 rounded-md px-3 py-2.5 text-left transition-colors duration-150",
                active
                  ? "bg-accent-strong/90 text-white"
                  : cn("text-text-secondary ", listRowHover),
                !isLast &&
                  "row-divider-b",
              )}
              aria-current={active ? "page" : undefined}
            >
              {/* `font-medium` 与 chat 侧边栏的 agent 名一致，见 SectionPane
                  同一处的说明：`text-xs` 下 `semibold` 明显过重。 */}
              <span
                className={cn(
                  "min-w-0 flex-1 truncate font-medium",
                  active ? "text-white" : "",
                )}
              >
                {p.title}
              </span>
              {/* 计数徽章：待审核数字色高亮（active 时用半透明白底以维持对比度） */}
              {c.total > 0 && (
                <span className="flex shrink-0 items-center gap-1">
                  {c.submitted > 0 && (
                    <span
                      className={cn(
                        "rounded-full px-1.5 text-10 font-medium",
                        active
                          ? "bg-white/20 text-white"
                          : "bg-amber-100 text-amber-700 dark:bg-amber-900/50 dark:text-amber-300",
                      )}
                    >
                      {c.submitted}
                    </span>
                  )}
                  <span
                    className={cn(
                      "rounded-full px-1.5 text-10",
                      active
                        ? "bg-white/20 text-white"
                        : "bg-zinc-100 text-text-tertiary group-hover:bg-zinc-200 dark:bg-zinc-800 ",
                    )}
                  >
                    {c.total}
                  </span>
                </span>
              )}
            </button>
          );
        })}

        {/* 空态：与 AgentList 一致的两条文案（无项目 / 无匹配） */}
        {!loadingProjects &&
          projects.length > 0 &&
          filteredProjects.length === 0 && (
            <div className="px-3 py-8 text-center text-xs text-text-tertiary ">
              {t("pm.noMatchingProjects")}
            </div>
          )}
      </nav>

      {/* 离线时禁用写操作 — 紧贴列表底部，但仍在 + 按钮上方 */}
      {healthy === false && (
        <div className="border-t border-border-divider px-3 py-2 text-10 text-text-tertiary">
          {t("pm.offlineReadonlyHint")}
        </div>
      )}

      {/* 底部新建按钮（与 AgentList 同款：p-1.5 包住，hover:bg-nav-control） */}
      <div className="p-1.5">
        <button
          type="button"
          onClick={() => openCreate()}
          disabled={healthy === false}
          className="flex w-full items-center justify-center rounded-md px-0 py-[var(--ui-btn-py)] text-xs font-medium text-text-secondary transition-colors hover:bg-nav-control focus-visible:bg-nav-control disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent "
          aria-label={t("pm.newProject")}
          title={t("pm.newProject")}
        >
          <Plus className="h-3.5 w-3.5" />
        </button>
      </div>

      {/* 新建项目对话框 */}
      {creating && (
        <div className="fixed inset-0 z-50 flex items-center justify-center">
          <div
            className="absolute inset-0 bg-modal-overlay"
            onClick={() => closeCreate()}
          />
          <div
            className="relative z-10 w-full max-w-sm rounded-md border border-border-outer bg-modal-surface p-5 shadow-xl"
            role="dialog"
            aria-modal="true"
            aria-labelledby="pm-new-project-title"
          >
            <h3 id="pm-new-project-title" className="text-sm font-semibold">
              {t("pm.newProject")}
            </h3>
            <div className="mt-3">
              <StyledInput
                ref={inputRef}
                value={title}
                onChange={(e) => setTitle(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleCreate();
                  if (e.key === "Escape") closeCreate();
                }}
                placeholder={t("pm.newProjectPlaceholder")}
                disabled={saving}
              />
            </div>
            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => closeCreate()}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-700"
                disabled={saving}
              >
                {t("common.cancel")}
              </button>
              <button
                type="button"
                onClick={handleCreate}
                className="rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
                disabled={saving}
              >
                {saving ? t("common.saving") : t("common.create")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* 删除项目确认 */}
      <ConfirmDialog
        open={!!deleting}
        title={t("pm.deleteProjectTitle")}
        message={`${t("pm.deleteProjectDesc")} "${deleting?.title ?? ""}"?`}
        confirmLabel={t("common.delete")}
        destructive
        onConfirm={handleDelete}
        onCancel={() => setDeleting(null)}
      />
    </aside>
  );
}
