/**
 * GroupRightNavBar — the 40px rail shown for a project group chat, the
 * group twin of the agent session's `RightNavBar`. Two tiles:
 *
 * - 齿轮 → 群成员面板（WeChat's "聊天信息"）
 * - 清单 → 项目任务面板（四列状态卡片，见 `GroupTasksPanel`）
 *
 * 两个面板共用同一个槽位（宽度 / 拖拽把手 / 折叠状态），所以 rail 表达的是
 * "哪个面板在该槽位里"，不是一个开关：`activeTab` 为 `null` 表示槽位收起。
 *
 * 齿轮那一片是共享的 `ViewSettingsRail`（pm / docs 视图同一套）；任务 tile 从
 * 它的 `children` 槽进来，共用同一条 rail 的 hover 离开语义。
 */

import { ListChecks } from "lucide-react";
import { NavButton } from "../common/NavButton";
import { ViewSettingsRail } from "./ViewSettingsRail";
import { useTranslation } from "../../i18n/useTranslation";

/** Which panel the group's right slot holds. */
export type GroupPanelTab = "members" | "tasks";

interface GroupRightNavBarProps {
  /** Panel currently pinned into the slot; `null` = collapsed. */
  activeTab: GroupPanelTab | null;
  /** Tile click. Same tile twice = collapse (the rail's toggle semantics). */
  onSelect: (tab: GroupPanelTab) => void;
  /** Panel currently floating over the chat as the hover drawer. */
  drawerTab?: GroupPanelTab | null;
  /** Pointer rested onto a tile (see `useRightPanelDrawer`). */
  onHoverStart?: (tab: GroupPanelTab) => void;
  /** Pointer left the rail. */
  onHoverEnd?: () => void;
}

export function GroupRightNavBar({
  activeTab,
  onSelect,
  drawerTab = null,
  onHoverStart,
  onHoverEnd,
}: GroupRightNavBarProps) {
  const { t } = useTranslation();
  const tasksOpen = activeTab === "tasks" || drawerTab === "tasks";

  return (
    <ViewSettingsRail
      expanded={activeTab === "members"}
      onToggle={() => onSelect("members")}
      tooltipKey="groupMembers.title"
      drawerOpen={drawerTab === "members"}
      onHoverStart={() => onHoverStart?.("members")}
      onHoverEnd={onHoverEnd}
    >
      <NavButton
        active={tasksOpen}
        onClick={() => onSelect("tasks")}
        // The drawer covers the bubble's usual spot (same rule as the gear).
        tooltip={drawerTab === "tasks" ? "" : t("groupTasks.title")}
        tooltipPosition="left"
        onMouseEnter={() => onHoverStart?.("tasks")}
      >
        <ListChecks className="h-5 w-5" />
      </NavButton>
    </ViewSettingsRail>
  );
}
