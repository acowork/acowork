import { useState, useRef, useEffect, useCallback, forwardRef, type ReactNode, useImperativeHandle, Children } from "react";
import { ChevronLeft, ChevronRight } from "lucide-react";

export interface ScrollableTabBarHandle {
    hasMoved: React.MutableRefObject<boolean>;
}

interface ScrollableTabBarProps {
    children: ReactNode;
    /** CSS selector for the active item — used for auto-scrollIntoView */
    activeItemSelector?: string;
    /** The value that will be matched by `activeItemSelector` (e.g. session-id or file-id) */
    activeItemId?: string;
}

/**
 * Reusable scrollable tab container with:
 * - Drag-to-scroll
 * - Left/right arrow buttons when tabs overflow
 * - Active tab auto-scroll into view
 */
export const ScrollableTabBar = forwardRef<ScrollableTabBarHandle, ScrollableTabBarProps>(
    function ScrollableTabBar({ children, activeItemSelector, activeItemId }, ref) {
        const scrollRef = useRef<HTMLDivElement>(null);
        const [canScrollLeft, setCanScrollLeft] = useState(false);
        const [canScrollRight, setCanScrollRight] = useState(false);
        const isDragging = useRef(false);
        const dragStartX = useRef(0);
        const dragScrollLeft = useRef(0);
        const hasMoved = useRef(false);

        // Expose hasMoved so parent can suppress tab-click after drag
        useImperativeHandle(ref, () => ({ hasMoved }), []);

        // ── Scroll arrow state ─────────────────────────────────────────────
        const updateScrollState = useCallback(() => {
            const el = scrollRef.current;
            if (!el) return;
            setCanScrollLeft(el.scrollLeft > 2);
            setCanScrollRight(el.scrollLeft + el.clientWidth < el.scrollWidth - 2);
        }, []);

        // Trigger re-check after children (tabs) are committed to the DOM.
        // ResizeObserver only fires when the element's own box changes, not when
        // scrollWidth changes due to children being added/removed — we need a
        // side-effect that runs after every render to catch those transitions.
        const childCount = Children.count(children);
        useEffect(() => {
            updateScrollState();
            const el = scrollRef.current;
            if (!el) return;
            el.addEventListener("scroll", updateScrollState, { passive: true });
            const ro = new ResizeObserver(updateScrollState);
            ro.observe(el);
            return () => {
                el.removeEventListener("scroll", updateScrollState);
                ro.disconnect();
            };
            // eslint-disable-next-line react-hooks/exhaustive-deps
        }, [updateScrollState, childCount]);

        const scrollBy = (dir: "left" | "right") => {
            scrollRef.current?.scrollBy({ left: dir === "left" ? -160 : 160, behavior: "smooth" });
        };

        // ── Active tab auto-scroll ─────────────────────────────────────────
        useEffect(() => {
            if (!scrollRef.current || !activeItemSelector || activeItemId == null) return;
            const el = scrollRef.current.querySelector(activeItemSelector);
            el?.scrollIntoView({ block: "nearest", inline: "nearest" });
        }, [activeItemSelector, activeItemId]);

        // ── Drag-to-scroll ─────────────────────────────────────────────────
        const handleDragStart = useCallback((e: React.MouseEvent) => {
            const el = scrollRef.current;
            if (!el) return;
            isDragging.current = true;
            hasMoved.current = false;
            dragStartX.current = e.clientX;
            dragScrollLeft.current = el.scrollLeft;
            el.style.cursor = "grabbing";
            el.style.userSelect = "none";

            const onMouseMove = (ev: MouseEvent) => {
                if (!isDragging.current) return;
                const dx = ev.clientX - dragStartX.current;
                if (Math.abs(dx) > 3) hasMoved.current = true;
                el.scrollLeft = dragScrollLeft.current - dx;
            };

            const onMouseUp = () => {
                isDragging.current = false;
                el.style.cursor = "";
                el.style.userSelect = "";
                document.removeEventListener("mousemove", onMouseMove);
                document.removeEventListener("mouseup", onMouseUp);
            };

            document.addEventListener("mousemove", onMouseMove);
            document.addEventListener("mouseup", onMouseUp);
        }, []);

        return (
            <>
                {canScrollLeft && (
                    <button
                        onClick={() => scrollBy("left")}
                        className="shrink-0 flex items-center justify-center rounded p-1 text-text-tertiary hover:text-text-tertiary hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15 transition-colors"
                    >
                        <ChevronLeft className="h-3.5 w-3.5" />
                    </button>
                )}
                <div
                    ref={scrollRef}
                    className="flex flex-1 min-w-0 overflow-x-auto gap-0.5 cursor-grab active:cursor-grabbing [&::-webkit-scrollbar]:hidden pb-[1px]"
                    style={{ scrollbarWidth: "none", msOverflowStyle: "none" }}
                    onMouseDown={handleDragStart}
                >
                    {children}
                </div>
                {canScrollRight && (
                    <button
                        onClick={() => scrollBy("right")}
                        className="shrink-0 flex items-center justify-center rounded p-1 text-text-tertiary hover:text-text-tertiary hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15 transition-colors"
                    >
                        <ChevronRight className="h-3.5 w-3.5" />
                    </button>
                )}
            </>
        );
    },
);
