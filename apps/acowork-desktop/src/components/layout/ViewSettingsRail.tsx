/**
 * ViewSettingsRail — the generic 40px right rail carrying a single
 * settings gear that toggles a context-settings panel. The pattern comes
 * from `GroupRightNavBar` (the group-chat members toggle) and is shared by
 * the pm / docs views, whose right panel hosts project-/document-scoped
 * settings instead of the agent-config tabs.
 *
 * The active state mirrors `RightNavBar`: the gear reads filled (with the
 * center hole preserved via the shared SVG mask) while the panel is open,
 * and collapses to the outline when the panel is hidden. "Open" now has two
 * forms — pinned into the layout (`expanded`) or floating over it as the
 * hover drawer (`drawerOpen`) — and the gear reads filled for both, because
 * from the user's side the panel is on screen either way.
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
  /** `true` while the panel is floating over the view as the hover drawer.
   *  Mirrors `expanded` for the gear's fill state; the rail's owner keeps the
   *  two mutually exclusive (the drawer only exists while collapsed). */
  drawerOpen?: boolean;
  /** Pointer rested onto the gear. Optional: a rail whose view has nothing to
   *  preview simply does not wire it (layout/useRightPanelDrawer.ts). */
  onHoverStart?: () => void;
  /** Pointer left the rail as a whole. */
  onHoverEnd?: () => void;
}

export function ViewSettingsRail({ expanded, onToggle, tooltipKey, drawerOpen = false, onHoverStart, onHoverEnd }: ViewSettingsRailProps) {
  const { t } = useTranslation();
  return (
    <nav className="flex w-10 shrink-0 flex-col items-center gap-2 py-2 dark:border-zinc-800" onMouseLeave={onHoverEnd}>
      <NavButton
        active={expanded || drawerOpen}
        onClick={onToggle}
        // The drawer covers the bubble's usual spot, so it is dropped while
        // the drawer is up (see RightNavBar for the same rule).
        tooltip={drawerOpen ? "" : t(tooltipKey)}
        tooltipPosition="left"
        onMouseEnter={onHoverStart}
        // Aligns the tile's top edge with the chat / panel header divider,
        // matching the first `RightNavBar` button's offset.
        className="mt-[25px]"
      >
        {expanded || drawerOpen ? (
          <FilledSettingsIcon className="h-5 w-5" />
        ) : (
          <OutlineSettingsIcon className="h-5 w-5" />
        )}
      </NavButton>
    </nav>
  );
}
