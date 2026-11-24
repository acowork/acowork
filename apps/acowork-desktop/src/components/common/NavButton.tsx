import type { ReactNode } from "react";
import { cn } from "../../lib/utils";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "./Tooltip";

interface NavButtonProps {
    active: boolean;
    onClick: () => void;
    tooltip: string;
    tooltipPosition?: "left" | "right";
    className?: string;
    /**
     * Unread count to surface as a dot on the icon. `0`/`undefined` renders
     * nothing — the dot is the only affordance, the number is not shown on a
     * 40px tile.
     */
    badge?: number;
    /**
     * Pointer-enter on the tile. The right rail uses this to preview a
     * collapsed panel as a hover drawer (layout/RightPanelDrawer.tsx) — the
     * affordance has to live on the button itself, because the tile is the
     * only thing the user can hover while the panel is gone.
     */
    onMouseEnter?: () => void;
    /** Pointer-leave from the tile. See `onMouseEnter`. */
    onMouseLeave?: () => void;
    children: ReactNode;
}

/**
 * Reusable navigation bar icon button.
 * Used by both the left (NavBar) and right (RightNavBar) navigation panels.
 * Matches the VS Code-style icon nav pattern: 40×40 rounded button,
 * accent color when active, gray when inactive, with a hover background.
 *
 * `tooltip=""` renders the bare button. That is not a convenience: the right
 * rail labels a tile to explain the panel it will open, and once the panel is
 * already open as a hover drawer the bubble has nothing left to explain — it
 * would just paint over the drawer's own left edge.
 */
export function NavButton({ active, onClick, tooltip, tooltipPosition = "right", className, badge, onMouseEnter, onMouseLeave, children }: NavButtonProps) {
    const { t } = useTranslation();
    const button = (
        <button
            onClick={onClick}
            onMouseEnter={onMouseEnter}
            onMouseLeave={onMouseLeave}
            className={cn(
                // `group` so children can react to the tile's hover state
                // (the artwork icon drops its grayscale on hover).
                "group relative flex h-10 w-10 items-center justify-center rounded-md transition-colors duration-150",
                active
                    ? "hover:bg-[var(--color-accent)]/10"
                    : "text-text-tertiary hover:text-text-tertiary hover:bg-[var(--color-accent)]/10 ",
                className,
            )}
            style={active ? { color: "var(--color-accent)" } : undefined}
        >
            {children}
            {badge != null && badge > 0 && (
                /* Red, and only red: unread is the same signal as the inbox
                   row bubble and the mobile avatar dot. This app's token is
                   `--color-destructive` — the `--color-danger` named here
                   has never existed, which resolved the declaration to
                   `background-color: <invalid>` and made the dot invisible
                   while the DOM test kept passing. No ring either:
                   `--color-bg-primary` is undefined for the same reason,
                   and the rail is transparent over the window background,
                   so there is nothing for a ring to match. */
                <span
                    data-testid="nav-badge"
                    aria-label={t("navBar.unreadMessages")}
                    className="pointer-events-none absolute right-1 top-1 h-2 w-2 rounded-full bg-[var(--color-destructive)]"
                />
            )}
        </button>
    );
    if (!tooltip) return button;
    return (
        <Tooltip content={tooltip} variant="plain" position={tooltipPosition}>
            {button}
        </Tooltip>
    );
}
