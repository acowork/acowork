/**
 * GroupRightNavBar — the 40px rail shown for a project group chat, the
 * group twin of the agent session's `RightNavBar`. It carries a single
 * entry for now: a settings tile that toggles the group members panel
 * (WeChat's "聊天信息" gear). Further group-info sections (announcements,
 * file history, …) can join this rail as they land.
 *
 * The tile itself is the shared `ViewSettingsRail` (filled/outline gear via
 * the shared SVG mask, active while the panel is open) — the same rail the
 * pm / docs views use for their context-settings panels.
 */

import { ViewSettingsRail } from "./ViewSettingsRail";

interface GroupRightNavBarProps {
  /** `true` = the members panel is expanded (gear reads active). */
  expanded: boolean;
  onToggle: () => void;
}

export function GroupRightNavBar({ expanded, onToggle }: GroupRightNavBarProps) {
  return (
    <ViewSettingsRail
      expanded={expanded}
      onToggle={onToggle}
      tooltipKey="groupMembers.title"
    />
  );
}
