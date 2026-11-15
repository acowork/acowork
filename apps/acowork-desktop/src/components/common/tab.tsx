import type { ReactNode, ButtonHTMLAttributes, HTMLAttributes } from "react";
import { cn } from "../../lib/utils";

// ── TabUnderline — animated center-expanding accent line ──────────────

function TabUnderline({ active }: { active: boolean }) {
    // 2px tall + shifted down 1px so the accent line fully overlaps the
    // 1px `border-b` divider drawn by the tab container. Because this span
    // is absolutely positioned inside the tab (which sits inside the
    // container that owns the border), it naturally paints on top of the
    // border — no z-index needed.
    return (
        <span
            className={`absolute -bottom-px left-0 right-0 h-[2px] bg-[var(--color-accent)] transition-transform duration-200 ease-out origin-center ${active ? "scale-x-100" : "scale-x-0"}`}
        />
    );
}

// ── TabItem — inline tab (SessionTabBar / FileEditorPanel) ────────────

interface TabItemProps extends HTMLAttributes<HTMLDivElement> {
    active: boolean;
}

export function TabItem({ active, className, children, ...rest }: TabItemProps) {
    return (
        <div
            className={cn(
                "group relative flex items-center gap-1 pl-2.5 pr-1.5 py-[var(--tab-py)] min-w-[60px] max-w-[160px] cursor-pointer transition-colors shrink-0",
                active
                    ? "text-text-secondary "
                    : "text-text-tertiary  hover:text-text-secondary",
                className,
            )}
            {...rest}
        >
            <TabUnderline active={active} />
            {children}
        </div>
    );
}

// ── TabButton — standalone button tab (Settings / Harness / Results) ──

interface TabButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
    active: boolean;
    children: ReactNode;
}

export function TabButton({ active, className, children, ...rest }: TabButtonProps) {
    return (
        <button
            className={cn(
                "relative px-3 py-2 text-sm transition-colors whitespace-nowrap shrink-0",
                active
                    ? "font-semibold text-text-secondary "
                    : "font-normal text-text-tertiary hover:text-text-secondary ",
                className,
            )}
            {...rest}
        >
            <TabUnderline active={active} />
            {children}
        </button>
    );
}
