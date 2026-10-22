/**
 * ViewSettingsRail — the generic 40px right rail carrying a single
 * settings gear that toggles a context-settings panel. The pattern comes
 * from `GroupRightNavBar` (the group-chat members toggle) and is shared by
 * the pm / docs views, whose right panel hosts project-/document-scoped
 * settings instead of the agent-config tabs.
 *
 * The active state mirrors `RightNavBar`: the gear reads filled (with the
 * center hole preserved via the shared SVG mask) while the panel is open,
 * and collapses to the outline when the panel is hidden.
 */

import { NavButton } from "../common/NavButton";
import { OutlineSettingsIcon, FilledSettingsIcon } from "../common/SettingsIcon";
import { useTranslation } from "../../i18n/useTranslation";

interface ViewSettingsRailProps {
  /** `true` = the settings panel is expanded (gear reads active). */
  expanded: boolean;
  onToggle: () => void;
  /** i18n key for the gear tooltip. */
  tooltipKey: string;
}

export function ViewSettingsRail({ expanded, onToggle, tooltipKey }: ViewSettingsRailProps) {
  const { t } = useTranslation();
  return (
    <nav className="flex w-10 shrink-0 flex-col items-center gap-2 py-2 dark:border-zinc-800">
      <NavButton
        active={expanded}
        onClick={onToggle}
        tooltip={t(tooltipKey)}
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
