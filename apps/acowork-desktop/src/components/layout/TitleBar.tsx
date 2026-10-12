import { Minus, Search, Square, X } from "lucide-react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { log } from "../../lib/logger";
import { useSearchStore } from "../../stores/searchStore";
import { useLayoutStore } from "../../stores/layoutStore";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";
import { GatewaySwitcherMenu } from "./GatewaySwitcherMenu";

/**
 * Right-panel toggle — VS Code's "layout panel" glyph: one outer frame with
 * a tall pane on the right.
 *
 * State is carried by the small pane's *fill*, not an arrow:
 *   - open   → pane filled with the global accent colour, frame stroked
 *   - closed → pane hollow (frame colour), only the outer frame stroked
 * The glyph is drawn in both states rather than swapped between two icons so
 * the frame stays pixel-identical and the button never "jumps" on toggle.
 */
export function RightPanelIcon({ className, open }: { className?: string; open: boolean }) {
  return (
    <svg
      className={className}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {/* Outer frame */}
      <rect x="2.75" y="4" width="18.5" height="16" rx="2.5" />
      {/* Right pane — the only part that changes between states */}
      {open ? (
        <rect x="14.5" y="4" width="6.75" height="16" rx="2.5" fill="var(--color-accent)" stroke="var(--color-accent)" />
      ) : (
        <rect x="14.5" y="4" width="6.75" height="16" rx="2.5" />
      )}
      {/* Divider between main area and pane */}
      <path d="M14.5 4v16" />
    </svg>
  );
}

export function TitleBar() {
  const isMacOS = navigator.platform.includes("Mac");
  const win = getCurrentWindow();
  const { t } = useTranslation();
  // Two stores, already the single source of truth for both actions:
  // ADR-081 search dialog and the chat view's right panel. Reading them here
  // (instead of taking props) keeps the toggle state correct for pm/doc when
  // they grow their own right panel off the same flag.
  const openSearch = useSearchStore((s) => s.openDialog);
  const rightPanelCollapsed = useLayoutStore((s) => s.rightPanelCollapsed);
  const setRightPanelCollapsed = useLayoutStore((s) => s.setRightPanelCollapsed);

  const handleMinimize = async () => {
    try {
      await win.minimize();
    } catch (error) {
      log.error("Failed to minimize:", error);
    }
  };

  const handleMaximize = async () => {
    try {
      await win.toggleMaximize();
    } catch (error) {
      log.error("Failed to toggle maximize:", error);
    }
  };

  const handleClose = async () => {
    try {
      await win.close();
    } catch (error) {
      log.error("Failed to close:", error);
    }
  };

  // On macOS, the native traffic lights provide close/minimize/maximize.
  // On Windows/Linux, we render custom buttons.
  //
  // `data-tauri-drag-region` enables native window dragging with zero JS
  // latency — Tauri's webview layer handles mousedown directly, so the
  // cursor stays anchored at the click point.  Double-click to maximize is
  // also handled natively by Tauri, replacing the previous setTimeout-based
  // workaround that caused a 250ms delay and cursor drift on macOS.
  return (
    <div
      data-tauri-drag-region
      className={`flex h-8 w-full items-center justify-between select-none ${
        isMacOS ? "pl-[80px]" : "pl-3"
      }`}
    >
      {/* Left: App title */}
      <div className="flex items-center gap-2">
        <span className="text-xs font-medium text-text-secondary ">
          ACowork
        </span>
      </div>

      {/* Right: one cluster — app toggles + window controls.
          The toggles are RIGHT-ALIGNED, never centred. This bar is
          `justify-between` over its children, so a third child is not
          free: it becomes the middle slot. An earlier revision of the
          gateway merge split the toggles and the window buttons into two
          sibling groups to get the toggles onto macOS, and that put the
          three toggles dead centre of the title bar — a louder mistake
          than the one it was fixing. Two children is the count this
          layout can carry: title left, everything else right.

          A right-aligned cluster also survives the outage pill, which
          grows the first toggle from 32px to ~140px. Centred, every
          outage would shove the whole row sideways; right-aligned, only
          the cluster's left edge moves, and the row stays anchored to
          the window controls the eye already tracks.

          The toggles are OUTSIDE the `!isMacOS` guard on purpose — only
          the window controls (which macOS draws natively as traffic
          lights) are platform-conditional. The Gateway button carries
          the outage pill, so hiding it on macOS would leave an outage
          signal with no way to act on it, while Windows had both. */}
      <div
        className="flex items-center gap-1"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <GatewaySwitcherMenu />
        <Tooltip content={t("titleBar.globalSearch")} position="bottom">
          <button
            className="flex h-8 w-8 items-center justify-center rounded text-text-secondary hover:bg-zinc-300  dark:hover:bg-zinc-700"
            onClick={openSearch}
            aria-label={t("titleBar.globalSearch")}
          >
            <Search className="h-3.5 w-3.5" />
          </button>
        </Tooltip>
        <Tooltip
          content={rightPanelCollapsed ? t("titleBar.expandRightPanel") : t("titleBar.collapseRightPanel")}
          position="bottom"
        >
          <button
            // `mr-2` is the ONLY thing separating the app toggles from the
            // OS chrome — it must stay on the last toggle, not on the
            // window-controls group, or the two clusters fuse back into
            // one row of undifferentiated buttons.
            className="mr-2 flex h-8 w-8 items-center justify-center rounded text-text-secondary hover:bg-zinc-300  dark:hover:bg-zinc-700"
            onClick={() => setRightPanelCollapsed((prev) => !prev)}
            aria-pressed={!rightPanelCollapsed}
            aria-label={rightPanelCollapsed ? t("titleBar.expandRightPanel") : t("titleBar.collapseRightPanel")}
          >
            {rightPanelCollapsed ? (
              <RightPanelIcon className="h-3.5 w-3.5" open={false} />
            ) : (
              <RightPanelIcon className="h-3.5 w-3.5" open={true} />
            )}
          </button>
        </Tooltip>

        {!isMacOS && (
          <>
            <button
              className="flex h-8 w-8 items-center justify-center rounded text-text-secondary hover:bg-zinc-300  dark:hover:bg-zinc-700"
              onClick={handleMinimize}
            >
              <Minus className="h-3.5 w-3.5" />
            </button>
            <button
              className="flex h-8 w-8 items-center justify-center rounded text-text-secondary hover:bg-zinc-300  dark:hover:bg-zinc-700"
              onClick={handleMaximize}
            >
              <Square className="h-3 w-3" />
            </button>
            <button
              className="flex h-8 w-8 items-center justify-center rounded text-text-secondary hover:bg-red-500 hover:text-white  dark:hover:bg-red-600"
              onClick={handleClose}
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </>
        )}
      </div>
    </div>
  );
}
