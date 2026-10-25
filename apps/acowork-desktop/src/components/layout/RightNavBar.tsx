import { NavButton } from "../common/NavButton";
import { OutlineSettingsIcon, FilledSettingsIcon } from "../common/SettingsIcon";
import { OutlineDatabaseIcon, FilledDatabaseIcon } from "../common/DatabaseIcon";
import { OutlineFolderOpenIcon, FilledFolderOpenIcon } from "../common/FolderOpenIcon";
import { OutlineGaugeIcon, FilledGaugeIcon } from "../common/GaugeIcon";
import { OutlineBugIcon, FilledBugIcon } from "../common/BugIcon";
import { useTranslation } from "../../i18n/useTranslation";
import { memoryTabAvailable } from "./rightPanelGate";
import { Wrench } from "lucide-react";
import type { ComponentType } from "react";

type PanelTab = "debug" | "status" | "setup" | "tools" | "memory" | "workspace";

interface RightNavBarProps {
  activeTab: PanelTab;
  onTabChange: (tab: PanelTab) => void;
  agentRunning: boolean;
  collapsed: boolean;
  /** ADR-101: `memory_service.ready` from the Gateway. The memory tile needs
   *  the standalone service, not just a running agent — without it every
   *  panel request answers 503. Unknown (never read) counts as false. */
  memoryServiceReady: boolean;
}

interface NavItem {
  tab: PanelTab;
  /** Lucide icon component. Omitted for tabs that render a custom icon (e.g. setup). */
  icon?: ComponentType<{ className?: string; fill?: string; strokeWidth?: number }>;
  i18nKey: string;
  show: boolean;
}

export function RightNavBar({ activeTab, onTabChange, agentRunning, collapsed, memoryServiceReady }: RightNavBarProps) {
  const { t } = useTranslation();

  const items: NavItem[] = [
    // workspace uses the shared FilledFolderOpenIcon/OutlineFolderOpenIcon
    // so the filled state preserves the open-folder seam via SVG mask (a
    // naïve `fill="currentColor"` on lucide's single self-intersecting
    // path collapses both lids into one solid blob, losing the "open"
    // visual). `icon` is intentionally omitted.
    { tab: "workspace", i18nKey: "rightPanel.workspace", show: agentRunning },
    // status uses the shared FilledGaugeIcon/OutlineGaugeIcon so the
    // filled state preserves the needle via SVG mask. Lucide's Activity
    // (heartbeat zigzag) had almost no body to fill — it just turned
    // into three small filled triangles, giving a weak selected state.
    // Gauge has a solid half-disc body that fills cleanly, and the
    // needle is preserved as a negative-space groove. `icon` omitted.
    { tab: "status", i18nKey: "rightPanel.status", show: true },
    // memory uses the shared FilledDatabaseIcon/OutlineDatabaseIcon so the
    // middle disk-separator line stays visible on selection; `icon` is
    // intentionally omitted.
    // ADR-101: memory left the Runtime, so the tile needs the standalone
    // service to be up as well as the agent — otherwise the panel would be a
    // column of 503s. `AppLayout` bounces an active memory tab back to status
    // off the same rule (`memoryTabAvailable`).
    {
      tab: "memory",
      i18nKey: "rightPanel.memory",
      show: memoryTabAvailable({ agentRunning, memoryServiceReady }),
    },
    { tab: "tools", icon: Wrench, i18nKey: "rightPanel.tools", show: agentRunning },
    // debug uses the shared FilledBugIcon/OutlineBugIcon so the filled
    // state preserves the central spine line via SVG mask. Without the
    // mask, the spine (M12 20v-9, fully inside the body) would be
    // drowned by `fill="currentColor"` and the bug would lose its
    // segmented look. `icon` is intentionally omitted.
    // ADR-048 follow-up: ADR-048 guards the "agent running but DevMode
    // off" path — the runtime `enable_agent_debug` button is rendered
    // inside the debug tab when `debug_state !== "enabled"`. That branch
    // already implies agentRunning, so gating the nav entry on
    // agentRunning preserves the ADR's intent while removing an empty
    // `noAgentDebug` placeholder when nothing is running.
    { tab: "debug", i18nKey: "rightPanel.debug", show: agentRunning },
    // setup uses the shared FilledSettingsIcon/OutlineSettingsIcon so the
    // center hole is preserved on selection; `icon` is intentionally omitted.
    // Placed last so the settings gear sits at the bottom of the nav rail,
    // visually separated from the content tabs.
    { tab: "setup", i18nKey: "rightPanel.setup", show: agentRunning },
  ];

  return (
    <nav className="flex w-10 shrink-0 flex-col items-center gap-2 py-2 dark:border-zinc-800">
      {items
        .filter((item) => item.show)
        .map(({ tab, icon: Icon, i18nKey }, index) => {
          const isActive = !collapsed && activeTab === tab;
          return (
            <NavButton
              key={tab}
              active={isActive}
              onClick={() => onTabChange(tab)}
              tooltip={t(i18nKey)}
              tooltipPosition="left"
              // First button's top edge aligns with the SessionTabBar/RightPanel border-b (~33px)
              className={index === 0 ? "mt-[25px]" : undefined}
            >
              {tab === "setup" ? (
                // Settings gear: use the shared icon so the filled state
                // preserves the center hole via SVG mask (a naïve
                // `fill="currentColor"` would erase the inner circle).
                isActive ? (
                  <FilledSettingsIcon className="h-5 w-5" />
                ) : (
                  <OutlineSettingsIcon className="h-5 w-5" />
                )
              ) : tab === "memory" ? (
                // Database/cylinder: use the shared icon so the filled state
                // preserves the middle disk-separator line via SVG mask (a
                // naïve `fill="currentColor"` would close the open arc into
                // a thin lens shape, erasing the line and flattening the
                // cylinder). The mask carves a thin groove along the arc so
                // the background shows through, restoring the 3D feel.
                isActive ? (
                  <FilledDatabaseIcon className="h-5 w-5" />
                ) : (
                  <OutlineDatabaseIcon className="h-5 w-5" />
                )
              ) : tab === "workspace" ? (
                // Open-folder: use the shared icon so the filled state
                // preserves the front-lid seam via SVG mask. Lucide's
                // FolderOpen is a single self-intersecting path that, when
                // filled, collapses the two lids into one blob and erases
                // the "open" visual. The mask carves grooves along the
                // front-lid top edge and left scoop slant to keep the
                // open-folder silhouette readable in the active state.
                isActive ? (
                  <FilledFolderOpenIcon className="h-5 w-5" />
                ) : (
                  <OutlineFolderOpenIcon className="h-5 w-5" />
                )
              ) : tab === "status" ? (
                // Gauge: use the shared icon so the filled state keeps
                // the needle visible via SVG mask. Lucide's Activity
                // zigzag had no real body to fill (just three little
                // triangles) — Gauge has a solid half-disc that fills
                // cleanly while the needle is carved as negative space.
                isActive ? (
                  <FilledGaugeIcon className="h-5 w-5" />
                ) : (
                  <OutlineGaugeIcon className="h-5 w-5" />
                )
              ) : tab === "debug" ? (
                // Bug: use the shared icon so the filled state preserves
                // the central spine line via SVG mask. Lucide's Bug spine
                // (M12 20v-9) sits fully inside the body — a naïve fill
                // would drown it and erase the segmented look. The mask
                // carves it as a negative-space groove.
                isActive ? (
                  <FilledBugIcon className="h-5 w-5" />
                ) : (
                  <OutlineBugIcon className="h-5 w-5" />
                )
              ) : Icon ? (
                <Icon
                  className="h-5 w-5"
                  fill={isActive ? "currentColor" : "none"}
                  strokeWidth={isActive ? 1.5 : 1.75}
                />
              ) : null}
            </NavButton>
          );
        })}
    </nav>
  );
}

