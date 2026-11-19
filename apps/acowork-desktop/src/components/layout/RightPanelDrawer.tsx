// Right-side hover drawer — the overlay shell that shows a side panel without
// taking layout width. See `rightPanelDrawerState.ts` for the intent rules and
// `useRightPanelDrawer.ts` for the clock; this is only the presentation.
//
// Two boxes, and the reason for the extra one is a CSS trap:
//
//   The peek is `position: absolute`, so it takes no part in the row's width
//   computation — that is the whole point, the chat, the file panel and the
//   editor keep their full width while it is up. But it slides with a
//   `transform`, and a transform DOES count toward the scrollable overflow
//   area of an ancestor scroll container. AppLayout's main box is
//   `overflow-hidden` (it has to be: see the comment there), so a drawer
//   mid-slide would silently make that box scrollable by its own width — and
//   the first thing that scrolls its content into view (the workspace tree
//   centering the selected row) shifts the entire app sideways.
//
//   So the drawer lives inside its own clip layer. That layer is absolutely
//   positioned, hence not a flex item, which is what makes `overflow: clip`
//   safe here — the same property on AppLayout's `flex-1` column item would
//   turn that row's automatic minimum size back into a content-based one and
//   push the app off screen. `clip` paints the same box as `hidden` but is
//   not scrollable, so there is nothing for the browser to scroll.
//
// The shell is view-agnostic on purpose. Every right rail has the same hover
// contract (one tile = one panel), so the peek geometry is one component and
// each view only supplies the panel it previews.

import type { CSSProperties, ReactNode } from "react";
import { RAIL_WIDTH_PX } from "./railWidth";
import { drawerMounted } from "./rightPanelDrawerState";
import type { RightPanelDrawerState } from "./useRightPanelDrawer";

interface RightPanelDrawerProps {
  drawer: RightPanelDrawerState;
  /** Same width the pinned panel would use, so a peek and a pin match. */
  width: number;
  /** The panel for `drawer.key`. Only evaluated while the drawer is mounted. */
  children: ReactNode;
}

export function RightPanelDrawer({ drawer, width, children }: RightPanelDrawerProps) {
  if (!drawerMounted(drawer)) return null;

  return (
    <div className="pointer-events-none absolute inset-0 overflow-clip">
      <div
        data-testid="right-panel-drawer"
        data-drawer-key={drawer.key}
        data-open={drawer.open ? "true" : "false"}
        className="right-drawer pointer-events-auto absolute inset-y-0 overflow-clip rounded-xl"
        style={
          {
            right: RAIL_WIDTH_PX,
            width,
            // Read by the `right-drawer-in/out` keyframes: the exit travels the
            // panel's width plus this strip, or a sliver stays under the rail.
            "--drawer-rail-offset": `${RAIL_WIDTH_PX}px`,
          } as CSSProperties
        }
        onMouseEnter={drawer.panelEnter}
        onMouseLeave={drawer.panelLeave}
      >
        {children}
      </div>
    </div>
  );
}
