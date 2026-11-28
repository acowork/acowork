/**
 * GroupTasksPanel — 项目群聊右侧槽位里的任务面板。
 *
 * 面板外壳（pane 头 + 宽度 / 拖拽把手）在这里，四张状态卡是共用的
 * [`TasksColumnFlow`]（bot 单聊右侧的任务 tab 用的是同一份，见
 * `AgentTasksPanel`）。本文件只负责：把群背后的项目 id 解析出来、按项目
 * 作用域把任务装进 `usePmBoardStore`、判断数据是否已经属于本项目。
 */

import { useEffect } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { CAPSULE_PANE_CN } from "../common/capsule";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { projectScope, sameScope, usePmBoardStore } from "../../stores/pm/boardStore";
import { TasksColumnFlow } from "../../views/pm/TasksColumnFlow";
import { parseBoardPageSize } from "../../lib/pm-board-settings";

interface GroupTasksPanelProps {
  width: number;
  onResizeStart?: (e: React.MouseEvent) => void;
  /** Rendered inside the hover drawer: the drawer owns the geometry (see
   *  `GroupMembersPanel` for the same contract). */
  overlay?: boolean;
}

export function GroupTasksPanel({ width, onResizeStart, overlay = false }: GroupTasksPanelProps) {
  const { t } = useTranslation();
  const projectId = useProjectGroupStore(
    (s) => s.groups.find((g) => g.session_id === s.activeGroupId)?.project_id,
  );
  const pageSize = usePmProjectStore(
    (s) => parseBoardPageSize(s.projects.find((p) => p.id === projectId)?.metadata),
  );
  const loadProjects = usePmProjectStore((s) => s.loadProjects);

  const boardScope = usePmBoardStore((s) => s.scope);
  const loadTasks = usePmBoardStore((s) => s.loadTasks);

  // 群聊视图不加载 PM 项目列表，这里补一次（30s 缓存，重复挂载不折腾）。
  // 列表只用来读每页数量，读不到就按默认值走。
  useEffect(() => {
    if (projectId) void loadProjects({ silent: true });
  }, [projectId, loadProjects]);

  useEffect(() => {
    if (projectId) void loadTasks(projectId);
  }, [projectId, loadTasks]);

  // 只看本项目的数据：store 里同一时刻的那份 `tasks` 可能属于别的项目或某个
  // agent。换作用域时 store 会先把列表清空，所以「作用域相同」就足以保证
  // 下面画出来的每一张卡片都是本项目的。
  const ready = !!projectId && sameScope(boardScope, projectScope(projectId));

  return (
    <div
      className={cn("relative flex shrink-0 flex-col", overlay ? "h-full w-full" : "ml-0.5")}
      style={overlay ? undefined : { width }}
    >
      {!overlay && onResizeStart && (
        <div
          className="group absolute bottom-0 -left-[3px] top-0 z-10 w-1 cursor-col-resize"
          onMouseDown={onResizeStart}
        >
          <div className="absolute inset-y-0 left-0 w-1 rounded-full transition-colors group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60" />
        </div>
      )}

      <div className={cn(CAPSULE_PANE_CN, "flex-1 bg-right-panel")}>
        {/* 面板头 —— 与群成员面板 / agent 右面板同一个 pane 头语法 */}
        <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center border-b border-border-divider px-[var(--tab-px)] text-[length:var(--tab-font-size)] font-medium leading-[var(--tab-line-height)] text-text-tertiary">
          <span className="min-w-0 flex-1 truncate">{t("groupTasks.title")}</span>
        </div>

        <TasksColumnFlow ready={ready} pageSize={pageSize} scopeKey={projectId ?? ""} />
      </div>
    </div>
  );
}