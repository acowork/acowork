import { type ReactNode } from "react";
import { ChevronDown } from "lucide-react";
import { cn } from "../../lib/utils";
import { toolbarButton } from "../../lib/ui-styles";
import { Tooltip } from "./Tooltip";

/**
 * Trigger-label font size. Tracks the global `--ui-font-size` setting
 * (settingsStore) at the app's standard UI-small ratio: 14px × 0.85 ≈
 * the old fixed 0.75rem, so the default look is unchanged while the
 * model / workspace / skill / effort labels now follow Ctrl+= / Ctrl+-.
 *
 * ponytail: `max-w-[120px]` below still caps the label, so at the largest
 * font step long model ids collapse to an ellipsis a bit earlier — the
 * ResizeObserver icon-fold handles the width, just with a shorter label.
 */
const LABEL_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.85)";

/**
 * Shared toolbar dropdown trigger — icon + text + chevron + hover tooltip.
 *
 * Text/chevron collapse can be driven two ways:
 *  1. CSS container queries via `collapseClass` (legacy, static breakpoints)
 *  2. JS-driven `textHidden` prop set by a ResizeObserver in the parent
 *     (preferred — adapts to which buttons are actually rendered)
 *
 * The tooltip carries `tipClass` and is shown on hover only when text is hidden.
 */
export function ToolbarDropdownTrigger({
    icon,
    label,
    collapseClass,
    tipClass,
    open,
    onToggle,
    wrapperRef,
    buttonClassName,
    children,
    tooltip,
    textHidden,
    btnId,
    disabled,
}: {
    icon: ReactNode;
    label: string;
    /** CSS class that container-query rules target to hide text + chevron */
    collapseClass?: string;
    /** CSS class that container-query rules target to show tooltip */
    tipClass?: string;
    open: boolean;
    onToggle: () => void;
    wrapperRef?: React.Ref<HTMLDivElement>;
    buttonClassName?: string;
    children: ReactNode;
    /** Tooltip text (falls back to label if not provided) */
    tooltip?: string;
    /** When true, force-hide the label text and chevron (JS-driven collapse) */
    textHidden?: boolean;
    /** Unique id used by ChatPanel's ResizeObserver to identify this button */
    btnId?: string;
    /**
     * ADR-076 §决策 4: read-only session (the caller does not own it) — the
     * trigger stays visible so the current value is still readable, but
     * cannot be opened. Only the *trigger* needs this: the dropdown body is
     * unreachable while closed.
     */
    disabled?: boolean;
}) {
    return (
        // `flex` (not `inline-block`): as a flex item the root's
        // `min-width:auto` would still resolve to the content width, so the
        // label could never shrink and the trigger would overlap whatever
        // sits next to it (the workspace panel's three action buttons).
        // `flex` + `min-w-0` lets the row squeeze the trigger, and the
        // inner `min-w-0` wrapper propagates that squeeze through the
        // Tooltip's `inline-flex` box down to the truncating label.
        <div
            ref={wrapperRef}
            data-toolbar-btn={btnId}
            className="relative flex min-w-0"
        >
            <Tooltip content={tooltip ?? label} tipClass={tipClass}>
                <button
                    type="button"
                    onClick={onToggle}
                    disabled={disabled}
                    aria-disabled={disabled || undefined}
                    className={cn(
                        toolbarButton,
                        "min-w-0 max-w-full",
                        open && "bg-zinc-200 dark:bg-zinc-700 text-text ",
                        buttonClassName,
                    )}
                >
                    <span className="shrink-0">{icon}</span>
                    <span
                        data-toolbar-text=""
                        className={cn(collapseClass, "min-w-0 max-w-[120px] truncate")}
                        style={{ fontSize: LABEL_FONT_SIZE, display: textHidden ? "none" : undefined }}
                    >{label}</span>
                    <ChevronDown
                        data-toolbar-chevron=""
                        className={cn("h-3 w-3 shrink-0 text-text-tertiary", collapseClass)}
                        style={{ display: textHidden ? "none" : undefined }}
                    />
                </button>
            </Tooltip>
            {children}
        </div>
    );
}
