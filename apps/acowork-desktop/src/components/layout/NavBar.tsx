import { type ComponentType } from "react";
import type { NavView } from "../../lib/types";
import { NavButton } from "../common/NavButton";
import { OutlineSettingsIcon, FilledSettingsIcon } from "../common/SettingsIcon";
import { OutlineChatIcon, FilledChatIcon } from "../common/ChatIcon";
import { AccountMenu } from "../account/AccountMenu";
import { useSettingsStore } from "../../stores/settingsStore";
import { useUserChatStore } from "../../stores/userChatStore";
import { useTranslation } from "../../i18n/useTranslation";

interface NavBarProps {
  currentView: NavView;
  onViewChange: (view: NavView) => void;
  /** Called when user clicks their avatar — navigate to profile settings */
  onAvatarClick: () => void;
}

const topNavItems: { view: NavView; icon: ComponentType<{ className?: string }>; i18nKey: string }[] = [
  { view: "chat", icon: OutlineChatIcon, i18nKey: "navBar.chat" },
  { view: "projects", icon: OutlineProjectsIcon, i18nKey: "navBar.projects" },
  { view: "docs", icon: OutlineDocsIcon, i18nKey: "navBar.docs" },
  { view: "extensions", icon: OutlineExtensionsIcon, i18nKey: "navBar.extensions" },
];

const bottomNavItems: { view: NavView; icon: ComponentType<{ className?: string }>; i18nKey: string }[] = [
  { view: "harness", icon: OutlineHarnessIcon, i18nKey: "navBar.harness" },
  { view: "settings", icon: OutlineSettingsIcon, i18nKey: "navBar.settings" },
];

/** Outline puzzle piece icon — stroke-only for non-selected harness state */
function OutlineHarnessIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
      <g transform="translate(1.2, 1.2) scale(0.9, 0.9)">
        <path d="M19.439 7.85c-.049.322.059.648.289.878l1.568 1.568c.47.47.706 1.087.706 1.704s-.235 1.233-.706 1.704l-1.611 1.611a.98.98 0 0 1-.837.276c-.47-.07-.802-.48-.968-.925a2.501 2.501 0 1 0-3.214 3.214c.446.166.855.497.925.968a.979.979 0 0 1-.276.837l-1.61 1.611a2.404 2.404 0 0 1-1.705.706 2.404 2.404 0 0 1-1.704-.706l-1.568-1.568a1.026 1.026 0 0 0-.877-.29c-.493.074-.84.504-1.02.968a2.5 2.5 0 1 1-3.237-3.237c.464-.18.894-.527.967-1.02a1.026 1.026 0 0 0-.289-.877l-1.568-1.568A2.404 2.404 0 0 1 1.998 12c0-.617.236-1.234.706-1.704L4.315 8.685a.98.98 0 0 1 .837-.276c.47.07.802.48.968.925a2.501 2.501 0 1 0 3.214-3.214c-.446-.166-.855-.497-.925-.968a.979.979 0 0 1 .276-.837l1.611-1.611a2.404 2.404 0 0 1 1.704-.706c.617 0 1.234.236 1.704.706l1.568 1.568c.23.23.556.338.877.29.493-.074.84-.504 1.02-.969a2.5 2.5 0 1 1 3.237 3.237c-.464.18-.894.527-.967 1.02Z" />
      </g>
    </svg>
  );
}

/** Filled puzzle piece icon — solid version for active harness state */
function FilledHarnessIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="currentColor" stroke="currentColor" strokeWidth="2">
      <g transform="translate(1.2, 1.2) scale(0.9, 0.9)">
        <path d="M19.439 7.85c-.049.322.059.648.289.878l1.568 1.568c.47.47.706 1.087.706 1.704s-.235 1.233-.706 1.704l-1.611 1.611a.98.98 0 0 1-.837.276c-.47-.07-.802-.48-.968-.925a2.501 2.501 0 1 0-3.214 3.214c.446.166.855.497.925.968a.979.979 0 0 1-.276.837l-1.61 1.611a2.404 2.404 0 0 1-1.705.706 2.404 2.404 0 0 1-1.704-.706l-1.568-1.568a1.026 1.026 0 0 0-.877-.29c-.493.074-.84.504-1.02.968a2.5 2.5 0 1 1-3.237-3.237c.464-.18.894-.527.967-1.02a1.026 1.026 0 0 0-.289-.877l-1.568-1.568A2.404 2.404 0 0 1 1.998 12c0-.617.236-1.234.706-1.704L4.315 8.685a.98.98 0 0 1 .837-.276c.47.07.802.48.968.925a2.501 2.501 0 1 0 3.214-3.214c-.446-.166-.855-.497-.925-.968a.979.979 0 0 1 .276-.837l1.611-1.611a2.404 2.404 0 0 1 1.704-.706c.617 0 1.234.236 1.704.706l1.568 1.568c.23.23.556.338.877.29.493-.074.84-.504 1.02-.969a2.5 2.5 0 1 1 3.237 3.237c-.464.18-.894.527-.967 1.02Z" />
      </g>
    </svg>
  );
}

/** Outline Extensions icon — VSCode-style 2×2 grid of tiles (extensions panel glyph) */
function OutlineExtensionsIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinejoin="round">
      {/* Top-left small tile */}
      <rect x="3" y="3" width="8" height="8" rx="1.5" />
      {/* Top-right wide tile */}
      <rect x="13" y="3" width="8" height="6" rx="1.5" />
      {/* Bottom-left tall tile */}
      <rect x="3" y="13" width="6" height="8" rx="1.5" />
      {/* Bottom-right square tile */}
      <rect x="11" y="11" width="10" height="10" rx="1.5" />
    </svg>
  );
}

/** Filled Extensions icon — solid version for active state */
function FilledExtensionsIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="currentColor" stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round">
      <rect x="3" y="3" width="8" height="8" rx="1.5" />
      <rect x="13" y="3" width="8" height="6" rx="1.5" />
      <rect x="3" y="13" width="6" height="8" rx="1.5" />
      <rect x="11" y="11" width="10" height="10" rx="1.5" />
    </svg>
  );
}

/** Outline document icon */
function OutlineDocsIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
      {/* Document */}
      <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9.5L14 3z" />
      <polyline points="14,3 14,9 20,9" />
      <line x1="8" y1="12" x2="16" y2="12" />
      <line x1="8" y1="15" x2="12" y2="15" />
    </svg>
  );
}

/** Filled document icon */
function FilledDocsIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="currentColor" stroke="currentColor" strokeWidth="1.75">
      {/* Document */}
      <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9.5L14 3z" />
      <polyline points="14,3 14,9 20,9" fill="none" stroke="white" />
      <line x1="8" y1="12" x2="16" y2="12" stroke="white" />
      <line x1="8" y1="15" x2="12" y2="15" stroke="white" />
    </svg>
  );
}

/**
 * Outline Gantt-style icon — a board frame with three rows of a leading dot
 * plus a horizontal bar of varying length, evoking a task timeline.
 * `isDark` is accepted for API compatibility with the call site but the outline
 * variant looks identical in both themes (stroke uses currentColor).
 */
function OutlineProjectsIcon({ className }: { className?: string; isDark?: boolean }) {
  return (
    <svg
      className={className}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {/* Board frame */}
      <rect x="3" y="4" width="18" height="16" rx="2" />
      {/* Row 1: medium bar */}
      <circle cx="6.5" cy="8.5" r="0.6" />
      <line x1="10.5" y1="8.5" x2="15" y2="8.5" />
      {/* Row 2: longest bar (offset right) */}
      <circle cx="6.5" cy="12" r="0.6" />
      <line x1="10.5" y1="12" x2="18" y2="12" />
      {/* Row 3: shortest bar */}
      <circle cx="6.5" cy="15.5" r="0.6" />
      <line x1="10.5" y1="15.5" x2="13" y2="15.5" />
    </svg>
  );
}

/**
 * Filled Gantt-style icon — solid filled board with white dots + bars "carved" out,
 * matching the FilledDocsIcon style (solid currentColor shape + white inner strokes).
 */
function FilledProjectsIcon({ className }: { className?: string }) {
  return (
    <svg
      className={className}
      viewBox="0 0 24 24"
      fill="currentColor"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {/* Solid board frame — same stroke width as outline version, so the outer
          visual bounds match exactly (the 0.875 stroke half-extends the fill). */}
      <rect x="3" y="4" width="18" height="16" rx="2" />
      {/* Row 1: medium bar carved in white */}
      <circle cx="6.5" cy="8.5" r="0.6" fill="white" stroke="white" />
      <line x1="10.5" y1="8.5" x2="15" y2="8.5" stroke="white" />
      {/* Row 2: longest bar carved in white (offset right) */}
      <circle cx="6.5" cy="12" r="0.6" fill="white" stroke="white" />
      <line x1="10.5" y1="12" x2="18" y2="12" stroke="white" />
      {/* Row 3: shortest bar carved in white */}
      <circle cx="6.5" cy="15.5" r="0.6" fill="white" stroke="white" />
      <line x1="10.5" y1="15.5" x2="13" y2="15.5" stroke="white" />
    </svg>
  );
}

export function NavBar({ currentView, onViewChange, onAvatarClick }: NavBarProps) {
  const { t } = useTranslation();
  const { theme, osTheme } = useSettingsStore();
  const isDark = theme === "dark" || (theme === "system" && osTheme === "dark");
  // ADR-076 §决策 8: the nav dot is driven by the inbox unread total. It can
  // only stay live now that the poll runs from the session instead of from
  // whichever pane happens to be mounted. Selector returns a number, so the
  // re-render is reference-stable.
  const unreadInbox = useUserChatStore(
    (s) => s.chats.reduce((n, c) => n + c.unread_count, 0),
  );

  return (
    <nav
      className="flex w-[var(--spacing-nav)] flex-col items-center gap-2 pt-2"
      role="navigation"
      aria-label={t("navBar.ariaLabelMainNavigation")}
    >
      {/* User avatar — account menu under multi_user (ADR-076 §决策 7),
          otherwise the historical "edit profile" entry. */}
      <AccountMenu onOpenProfile={onAvatarClick} />

      {/* Top navigation items */}
      {topNavItems.map(({ view, icon: Icon, i18nKey }) => (
        <NavButton
          key={view}
          active={currentView === view}
          onClick={() => onViewChange(view)}
          tooltip={t(i18nKey)}
          tooltipPosition="right"
          badge={view === "chat" ? unreadInbox : undefined}
        >
          {currentView === view ? (
            view === "chat" ? (
              <FilledChatIcon className="h-6 w-6" />
            ) : view === "extensions" ? (
              <FilledExtensionsIcon className="h-6 w-6" />
            ) : view === "harness" ? (
              <FilledHarnessIcon className="h-6 w-6" />
            ) : view === "docs" ? (
              <FilledDocsIcon className="h-6 w-6" />
            ) : view === "projects" ? (
              <FilledProjectsIcon className="h-6 w-6" />
            ) : (
              <FilledSettingsIcon className="h-6 w-6" />
            )
          ) : (
            view === "projects" ? (
              <OutlineProjectsIcon className="h-6 w-6" isDark={isDark} />
            ) : (
              <Icon className="h-6 w-6" />
            )
          )}
        </NavButton>
      ))}

      {/* Spacer */}
      <div className="flex-1" />

      {/* Bottom navigation items */}
      {bottomNavItems.map(({ view, icon: Icon, i18nKey }) => (
        <NavButton
          key={view}
          active={currentView === view}
          onClick={() => onViewChange(view)}
          tooltip={t(i18nKey)}
          tooltipPosition="right"
        >
          {currentView === view ? (
            view === "harness" ? (
              <FilledHarnessIcon className="h-6 w-6" />
            ) : (
              <FilledSettingsIcon className="h-6 w-6" />
            )
          ) : (
            <Icon className="h-6 w-6" />
          )}
        </NavButton>
      ))}
    </nav>
  );
}
