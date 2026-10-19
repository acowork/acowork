/**
 * GroupRightNavBar — the 40px rail shown for a project group chat, the
 * group twin of the agent session's `RightNavBar`. It carries a single
 * entry for now: a settings tile that toggles the group members panel
 * (WeChat's "聊天信息" gear). Further group-info sections (announcements,
 * file history, …) can join this rail as they land.
 *
 * The active state mirrors `RightNavBar`: the gear reads filled (with the
 * center hole preserved via the shared SVG mask) while the panel is open,
 * and collapses to the outline when the panel is hidden.
 */

import { NavButton } from "../common/NavButton";
import { OutlineSettingsIcon, FilledSettingsIcon } from "../common/SettingsIcon";
import { useTranslation } from "../../i18n/useTranslation";

interface GroupRightNavBarProps {
  /** `true` = the members panel is expanded (gear reads active). */
  expanded: boolean;
  onToggle: () => void;
}

export function GroupRightNavBar({ expanded, onToggle }: GroupRightNavBarProps) {
  const { t } = useTranslation();
  return (
    <nav className="flex w-10 shrink-0 flex-col items-center gap-2 py-2 dark:border-zinc-800">
      <NavButton
        active={expanded}
        onClick={onToggle}
        tooltip={t("groupMembers.title")}
        tooltipPosition="left"
        // Aligns the tile's top edge with the chat / panel header divider,
        // matching the first `RightNavBar` button's offset.
        className="mt-[25px]"
      >
        {expanded ? (
          <FilledSettingsIcon className="h-5 w-5" />
        ) : (
          <OutlineSettingsIcon className="h-5 w-5" />
        )}
      </NavButton>
    </nav>
  );
}
