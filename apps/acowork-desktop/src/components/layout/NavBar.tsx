import { useEffect, useState } from "react";
import type { NavView } from "../../lib/types";
import { listApps, type AppDto } from "../../lib/apps-api";
import { fetchGalleryIndex } from "../../lib/appGallery";
import {
  hostedAppIconSrc,
  hostedAppLetter,
  hostedNavApps,
} from "../../lib/hostedNavApps";
import { NavButton } from "../common/NavButton";
import { AccountMenu } from "../account/AccountMenu";
import { useAuthStore } from "../../stores/authStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import { useLayoutStore } from "../../stores/layoutStore";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import chatNavIcon from "../../assets/nav-icons/chat.png";
import pmNavIcon from "../../assets/nav-icons/projects.png";
import docNavIcon from "../../assets/nav-icons/docs.png";
import appsNavIcon from "../../assets/nav-icons/apps.png";
import harnessNavIcon from "../../assets/nav-icons/harness.png";
import settingsNavIcon from "../../assets/nav-icons/settings.png";

interface NavBarProps {
  currentView: NavView;
  onViewChange: (view: NavView) => void;
  /** Called when user clicks their avatar — navigate to profile settings */
  onAvatarClick: () => void;
}

/**
 * The four product surfaces that own an artwork icon: chat, PM, docs and the
 * app gallery. The artwork is a raster PNG, so both states come from ONE file
 * — see `NavArtworkIcon`.
 */
const topNavItems: { view: NavView; iconSrc: string; i18nKey: string }[] = [
  { view: "chat", iconSrc: chatNavIcon, i18nKey: "navBar.chat" },
  { view: "projects", iconSrc: pmNavIcon, i18nKey: "navBar.projects" },
  { view: "docs", iconSrc: docNavIcon, i18nKey: "navBar.docs" },
];

/**
 * The utility surfaces at the foot of the rail. They were stroked line icons
 * once; they are artwork now, in the same family as the product surfaces
 * above, so they carry their own raster icon and take the same grayscale
 * treatment for the inactive state.
 */
const bottomNavItems: { view: NavView; iconSrc: string; i18nKey: string }[] = [
  { view: "harness", iconSrc: harnessNavIcon, i18nKey: "navBar.harness" },
  { view: "settings", iconSrc: settingsNavIcon, i18nKey: "navBar.settings" },
];

/**
 * Product artwork for a nav slot (chat / PM / docs / apps).
 *
 * The inactive state is the CSS grayscale filter — the same treatment a hosted
 * third-party app icon gets below (see the `hostedApps.map` block). One asset
 * therefore serves both states: there is no second "grey" file to ship and no
 * way for the two to drift apart. The artwork is a rounded square, so
 * `object-contain` keeps it whole; `object-cover` would crop the corners.
 *
 * Hover drops the grayscale (`group-hover:grayscale-0`, the `group` lives on
 * the `NavButton` tile). A tinted hover background alone does not read as
 * "this is clickable" — the icon has to come back to colour with the
 * pointer, or the affordance is invisible on a mostly-grey rail. Leaving
 * the tile restores the grey.
 *
 * `NAV_ARTWORK_CLASS` is the one size every artwork tile in this column uses
 * — the four product surfaces, the two utility surfaces at the foot of the
 * rail, and the hosted third-party apps. They share a 40px tile, so a tile
 * that is not the same size reads as a mistake rather than a hierarchy. 32px
 * in a 40px tile (4px of breathing room) is the density a desktop app dock
 * uses; the tile and the gap are untouched, only the artwork inside them grew.
 */
const NAV_ARTWORK_CLASS = "h-7 w-7";

function NavArtworkIcon({ src, active, className }: { src: string; active: boolean; className?: string }) {
  return (
    <img
      src={src}
      alt=""
      draggable={false}
      className={cn(
        NAV_ARTWORK_CLASS,
        "rounded-md object-contain",
        !active && "grayscale group-hover:grayscale-0",
        className,
      )}
    />
  );
}

export function NavBar({ currentView, onViewChange, onAvatarClick }: NavBarProps) {
  const { t } = useTranslation();
  // The nav dot is the unified chat surface's unread total — DMs (`user_user`)
  // and project groups alike, read straight off their chat sessions. Kept live
  // by `AgentList`'s fetch + the `user-chat-changed` doorbell (see chatStore).
  // Selector returns a number, so the re-render is reference-stable.
  const me = useAuthStore((s) => s.account?.user_id);
  const unreadChats = useProjectGroupStore((s) =>
    [...s.groups, ...s.dms].reduce((n, g) => n + ((me ? g.unread?.[me] : 0) ?? 0), 0),
  );
  const hostedAppId = useLayoutStore((s) => s.hostedAppId);
  const hostedAppsEpoch = useLayoutStore((s) => s.hostedAppsEpoch);
  const requestOpenHostedApp = useLayoutStore((s) => s.requestOpenHostedApp);
  const [hostedApps, setHostedApps] = useState<AppDto[]>([]);
  const [galleryIcons, setGalleryIcons] = useState<Record<string, string>>({});

  useEffect(() => {
    let cancelled = false;
    const load = () => {
      void listApps()
        .then((apps) => {
          if (!cancelled) setHostedApps(hostedNavApps(apps));
        })
        .catch(() => {
          if (!cancelled) setHostedApps([]);
        });
      void fetchGalleryIndex()
        .then((index) => {
          if (cancelled) return;
          const icons: Record<string, string> = {};
          for (const app of index.apps) {
            if (app.icon) icons[app.id] = app.icon;
          }
          setGalleryIcons(icons);
        })
        .catch(() => {
          /* catalog offline — letter mark until the next focus */
        });
    };
    load();
    const onFocus = () => load();
    window.addEventListener("focus", onFocus);
    return () => {
      cancelled = true;
      window.removeEventListener("focus", onFocus);
    };
  }, [hostedAppsEpoch]);

  return (
    <nav
      className="flex w-[var(--spacing-nav)] flex-col items-center gap-2 pt-2"
      role="navigation"
      aria-label={t("navBar.ariaLabelMainNavigation")}
    >
      {/* User avatar — account menu with a live session (ADR-076 §决策 7),
          otherwise the historical "edit profile" entry. */}
      <AccountMenu onOpenProfile={onAvatarClick} />

      {/* Top navigation items */}
      {topNavItems.map(({ view, iconSrc, i18nKey }) => (
        <NavButton
          key={view}
          active={currentView === view}
          onClick={() => onViewChange(view)}
          tooltip={t(i18nKey)}
          tooltipPosition="right"
          badge={view === "chat" ? unreadChats : undefined}
        >
          <NavArtworkIcon src={iconSrc} active={currentView === view} />
        </NavButton>
      ))}

      {/* App Gallery keeps a fixed slot (4th) — installing an app must never
          move it; hosted app icons are appended below it in install order. */}
      <NavButton
        active={currentView === "extensions"}
        onClick={() => onViewChange("extensions")}
        tooltip={t("navBar.extensions")}
        tooltipPosition="right"
      >
        <NavArtworkIcon src={appsNavIcon} active={currentView === "extensions"} />
      </NavButton>

      {/* Hosted webview apps (ADR-116) — appended below the gallery, oldest
          install first, so a new install lands at the bottom of the list. */}
      {hostedApps.map((app) => {
        const active = currentView === "app" && hostedAppId === app.id;
        const src = hostedAppIconSrc(app, galleryIcons);
        return (
          <NavButton
            key={app.id}
            active={active}
            onClick={() => requestOpenHostedApp(app.id)}
            tooltip={app.nav_label || app.name}
            tooltipPosition="right"
          >
            {src ? (
              <img
                src={src}
                alt=""
                draggable={false}
                className={
                  active
                    ? `${NAV_ARTWORK_CLASS} rounded-md object-cover`
                    : `${NAV_ARTWORK_CLASS} rounded-md object-cover grayscale group-hover:grayscale-0`
                }
              />
            ) : (
              <span
                className={cn(
                  NAV_ARTWORK_CLASS,
                  "flex items-center justify-center rounded-md text-[13px] font-semibold",
                  // The letter mark has no artwork to un-grey, so hover
                  // brightens its own background instead — the same affordance
                  // the artwork icons get from `group-hover:grayscale-0`.
                  // The two colours live in custom properties because an
                  // inline `background` would outrank the hover class and
                  // the hover would never show.
                  "bg-[var(--letter-bg)] transition-colors group-hover:bg-[var(--letter-bg-hover)]",
                )}
                style={
                  {
                    // Both branches are `color-mix(... transparent)` off a theme
                    // token: a literal fallback inside `var()` would win on every
                    // theme that does not declare the token, which is exactly the
                    // hard-coded colour the theme-pack red line exists to catch.
                    "--letter-bg": active
                      ? "color-mix(in srgb, var(--color-accent) 22%, transparent)"
                      : "color-mix(in srgb, var(--color-text-tertiary) 18%, transparent)",
                    "--letter-bg-hover": active
                      ? "color-mix(in srgb, var(--color-accent) 22%, transparent)"
                      : "color-mix(in srgb, var(--color-accent) 20%, transparent)",
                  } as React.CSSProperties
                }
              >
                {hostedAppLetter(app)}
              </span>
            )}
          </NavButton>
        );
      })}

      {/* Spacer */}
      <div className="flex-1" />

      {/* Bottom navigation items */}
      {bottomNavItems.map(({ view, iconSrc, i18nKey }) => (
        <NavButton
          key={view}
          active={currentView === view}
          onClick={() => onViewChange(view)}
          tooltip={t(i18nKey)}
          tooltipPosition="right"
        >
          <NavArtworkIcon src={iconSrc} active={currentView === view} />
        </NavButton>
      ))}
    </nav>
  );
}
