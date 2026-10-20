/**
 * SectionPane — the two-column shell the settings / harness views wear,
 * matching the pm / docs / extensions master-detail layout.
 *
 * Why: both pages were a full-width `bg-nav-surface` wall of inline tabs.
 * The rest of the app navigates a *set of sections* by picking one from a
 * left list and reading it on the right (pm project → board, doc tree →
 * editor, chat agent → conversation). Settings' four tabs and harness' four
 * tabs are the same interaction wearing a different shape, so they now
 * borrow the same shape: left `bg-nav-surface` list capsule, draggable
 * SplitHandle, right `bg-right-panel` detail capsule (card flow).
 *
 * The left header copies DocTreeSidebar's: icon + title on one line, a
 * hairline `border-b`, the list below it. That is the one place in the app
 * where "which section am I in" is named, so it is worth matching exactly.
 *
 * The detail column keeps the per-page header inside it (harness' provider
 * card, settings' Gateway tab) — the page owns its content, the shell owns
 * the frame. The `w-fit`/`max-w-*` reading column of the old tab strip is
 * preserved by the caller passing those classes as `detailBodyClassName`.
 *
 * The wrapper div is transparent on purpose: AppLayout no longer paints a
 * solid `bg-page-bg` plane behind these views, so the window vibrancy shows
 * through the gap between the two capsules (see capsule.ts).
 */
import type { ReactNode } from "react";
import { CAPSULE_PANE_CN } from "./capsule";
import { SplitHandle } from "./SplitHandle";
import { OverlayScrollbar } from "./OverlayScrollbar";
import { useDragResize } from "../../hooks/useDragResize";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";

export interface SectionItem {
  id: string;
  label: string;
  /** Small leading icon (lucide component, sized by the shell). */
  icon: ReactNode;
}

export interface SectionPaneProps {
  /** Left header title (e.g. i18n "settings" / "harness"). */
  title: string;
  /** Leading icon for the left header. */
  icon: ReactNode;
  items: SectionItem[];
  selected: string;
  onSelect: (id: string) => void;
  /** localStorage key for the list column width. */
  storageKey: string;
  /** Right column content. */
  children: ReactNode;
  /** aria-label for the left list (defaults to `title`). */
  listLabel?: string;
  /** Reading-width cap for the right column, e.g. "max-w-2xl". */
  detailBodyClassName?: string;
}

export function SectionPane({
  title,
  icon,
  items,
  selected,
  onSelect,
  storageKey,
  children,
  listLabel,
  detailBodyClassName,
}: SectionPaneProps) {
  // Same width envelope as pm / docs / extensions so the four views' split
  // handles sit at the same x once the user has touched any of them.
  const list = useDragResize({
    storageKey,
    defaultWidth: 240,
    minWidth: 160,
    maxWidth: 400,
  });

  return (
    // `w-full` is load-bearing: this root is a flex item of AppLayout's
    // wrapper, and without it `flex: 0 1 auto` sizes the view to its CONTENT
    // (the list column) and leaves a strip of bare vibrancy where the detail
    // capsule belongs. Same note as in DocsView.
    <div className="flex h-full min-h-0 w-full flex-1">
      {/* 左栏：图标+标题 头部 → 分割线 → 列表（DocTreeSidebar 头部样式） */}
      <aside
        className={cn(CAPSULE_PANE_CN, "h-full shrink-0 bg-nav-surface text-xs")}
        style={{ width: list.width }}
        aria-label={listLabel ?? title}
      >
        {/* Header — sized to match pm's list column, NOT doc's.
            pm renders its header as `py-2` around a 28px search input
            (≈44px); doc's title band is `py-1.5` around a 14px text
            line (≈27px). This pane is a flat list with no tree, so it
            is the pm shape, and the height is pinned to
            `--ui-list-header-h` so the three columns line up no matter
            which header the next view happens to use. The icon + title
            combination still borrows doc's wording, which is the part
            that actually reads well for a section list. */}
        <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center gap-1 border-b border-border-divider px-3">
          <span className="mr-1 shrink-0 text-text-tertiary" aria-hidden>
            {icon}
          </span>
          <span className="flex-1 truncate font-medium text-text-secondary ">
            {title}
          </span>
        </div>
        <OverlayScrollbar className="px-1.5 py-1">
          {items.map((item, index) => {
            const active = item.id === selected;
            const isLast = index === items.length - 1;
            return (
              <button
                key={item.id}
                type="button"
                onClick={() => onSelect(item.id)}
                aria-current={active ? "page" : undefined}
                className={cn(
                  // Row chrome copied from pm's ProjectSidebar so the
                  // list capsules read as one system: same py-2.5 height,
                  // same solid `accent/90` + white-text selection, same
                  // hairline divider between rows. doc is the deliberate
                  // exception — it is a tree, so it keeps its own denser
                  // py-1 rows and per-depth indentation.
                  "relative flex w-full items-center gap-2 rounded-md px-3 py-2.5 text-left transition-colors duration-150",
                  active
                    ? "bg-accent-strong/90 text-white"
                    : cn("text-text-secondary ", listRowHover),
                  !isLast &&
                    "row-divider-b",
                )}
              >
                {/* The icon follows the text colour: a `text-tertiary`
                    glyph left at its muted grey on the solid accent
                    selection is barely readable, so the selected state
                    forces white on both the icon and the label. */}
                <span
                  className={cn("shrink-0", active ? "text-white" : "text-text-tertiary")}
                  aria-hidden
                >
                  {item.icon}
                </span>
                {/* `font-medium` matches the agent name in the chat
                    sidebar (AgentList) — the list capsules read as one
                    system. `font-semibold` was too heavy at `text-xs`. */}
                <span className="min-w-0 flex-1 truncate font-medium">{item.label}</span>
              </button>
            );
          })}
        </OverlayScrollbar>
      </aside>

      <SplitHandle onMouseDown={list.onHandleMouseDown} ariaLabel={listLabel ?? title} />

      {/* 右栏：详情胶囊。
          底色是 `bg-right-panel` 而不是 `bg-page-bg`：这一栏装的是
          卡片流，而卡片那套三层阶梯（`panel-block` / `panel-inset` /
          `panel-inset-2`）是照着右侧 agent 设置面板的那套 surface
          调的。胶囊若用 `bg-page-bg`，卡片就会坐落在一个与它们
          配套的 surface 之外——深色下 page-bg L=8% 而 panel-block
          L=16.7%，卡片比容器亮 8.7 个点、浮在底上；浅色下同样是
          反向（卡片比 page-bg 暗，容器反而比卡片亮）。挂到
          `bg-right-panel` 上，卡片与容器回到 agent 面板已调好的
          同一组关系。

          pm / doc / extensions 的详情栏是内容主区、不是卡片流，仍用
          `bg-page-bg`；所以这不是"全局把 page-bg 换成 right-panel"，
          是按内容类型分流。 */}
      <div
        className={cn(CAPSULE_PANE_CN, "h-full min-w-0 flex-1 bg-right-panel")}
        role="tabpanel"
      >
        <OverlayScrollbar>
          <div className={cn("p-4", detailBodyClassName)}>{children}</div>
        </OverlayScrollbar>
      </div>
    </div>
  );
}
