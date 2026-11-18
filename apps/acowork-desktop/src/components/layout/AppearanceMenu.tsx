import { useRef, useState, useSyncExternalStore } from "react";
import { Check, Palette, type LucideIcon } from "lucide-react";
import { THEME_MODE_ICONS } from "../../lib/themeModeIcons";
import { useTranslation } from "../../i18n/useTranslation";
import { useSettingsStore } from "../../stores/settingsStore";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";
import { cn } from "../../lib/utils";
import { Tooltip } from "../common/Tooltip";
import { AccentSwatches } from "../common/AccentSwatches";
import { titleBarMenuPanel, titleBarMenuTrigger, useDismissMenu } from "./titleBarMenu";
import {
  DEFAULT_THEME_PACK,
  TINTED_PACKS,
  getActiveThemePack,
  packTint,
  setThemePack,
  subscribeThemePacks,
} from "../../styles/themePacks";
import type { Theme } from "../../lib/types";

/** Theme-mode rows, in the order the Settings ▸ Appearance tab lists
 *  them. The glyph is part of the row (not just a check mark) so the
 *  three modes read as three named states, not a bare radio list.
 *  The glyphs come from `themeModeIcons` — the same map Settings ▸
 *  Appearance draws its radio rows from, so both doors show one shape. */
const THEME_ROWS: { value: Theme; icon: LucideIcon }[] = (
  ["light", "dark", "system"] as const
).map((value) => ({ value, icon: THEME_MODE_ICONS[value] }));

/**
 * Title-bar appearance quick-settings — theme mode + global accent color.
 *
 * A shortcut for the two things people change most often and change
 * most often BY ACCIDENT OF NAVIGATION: the theme and the accent color
 * live in Settings ▸ Appearance, three clicks and a page transition
 * away. This menu is the same two settings, one click from the title
 * bar.
 *
 * It owns NO state. The theme mode and accent color are read from and
 * written through `settingsStore` (`theme` / `setTheme`, `accentColor` /
 * `setAccentColor`), which is the same store the Settings ▸ Appearance
 * tab drives — so the two entry points are two doors to one state, and
 * picking a mode here is immediately visible there (and vice versa). The
 * theme PACK goes through themePacks' own subscription for the same
 * guarantee. The store persists everything to localStorage, so the
 * choice survives a restart.
 *
 * The visual style is the Gateway menu's, not a new one: same panel
 * surface, same trigger chrome, same dismiss rules — all three come from
 * `titleBarMenu.ts`, which the Gateway menu also uses.
 */
export function AppearanceMenu() {
  const { t } = useTranslation();
  const theme = useSettingsStore((s) => s.theme);
  const setTheme = useSettingsStore((s) => s.setTheme);
  const accentColor = useSettingsStore((s) => s.accentColor);
  const customAccent = useSettingsStore((s) => s.customAccent);
  const setAccentColor = useSettingsStore((s) => s.setAccentColor);

  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  // Theme packs are plain DOM state (a class on <html>), not store state.
  // Subscribe via themePacks' listener set so this menu re-renders when the
  // OTHER door (Settings ▸ Appearance) changes the pack while this one is
  // mounted — and vice versa.
  const activePack = useSyncExternalStore(subscribeThemePacks, getActiveThemePack);

  const pickThemePack = (id: string) => {
    setThemePack(id);
  };

  useDismissMenu(wrapRef, open, () => setOpen(false));

  return (
    <div
      ref={wrapRef}
      className="relative"
      // The title bar is a native drag region; without this the mousedown
      // that opens the popover also starts a window drag.
      onMouseDown={(e) => e.stopPropagation()}
    >
      <Tooltip content={t("titleBar.appearance")} position="bottom">
        <button
          type="button"
          aria-expanded={open}
          aria-label={t("titleBar.appearance")}
          onClick={() => setOpen((v) => !v)}
          className={titleBarMenuTrigger(open)}
        >
          {/* `Palette` — the SAME glyph SettingsPage gives its Appearance
              tab (SettingsPage.tsx:39). Two doors to one feature must not
              learn two shapes. */}
          <Palette className="h-3.5 w-3.5" />
        </button>
      </Tooltip>

      {open && (
        <div className={titleBarMenuPanel}>
          {/* Theme mode — a horizontal segmented control, the same grammar
              the PM header uses for Board/List (ProjectHeader.tsx): bordered
              inset track, the active segment lifted on the page sheet with a
              shadow. Deliberately NOT the row list the pack section below
              uses — mode and palette are different decisions and the two
              shapes keep them apart at a glance. Radio semantics stay (not
              PM's `aria-pressed`): this is a mutually exclusive choice and
              the tests read it as radios. */}
          <div className="border-b border-border-divider pb-2" role="radiogroup" aria-label={t("settings.theme")}>
            <p className="px-1 pb-1.5 text-xs text-text-tertiary">
              {t("settings.theme")}
            </p>
            <div className="flex items-center justify-center gap-0.5 rounded-md border border-border-outer bg-panel-block p-0.5">
              {THEME_ROWS.map(({ value, icon: Icon }) => {
                const current = theme === value;
                return (
                  <button
                    key={value}
                    type="button"
                    role="radio"
                    aria-checked={current}
                    onClick={() => setTheme(value)}
                    // `flex-1` keeps the three segments as equal thirds of
                    // the track — content-sized segments left the wide track
                    // half empty. `whitespace-nowrap` is what keeps a long
                    // CJK label on one line instead of breaking the row.
                    className={cn(
                      "flex h-6 flex-1 items-center justify-center gap-1 whitespace-nowrap rounded px-2 text-xs font-medium transition-colors",
                      // `ring-border-outer` on the lifted segment: in
                      // light mode page-bg (white) over panel-block (98%)
                      // is a 2% step — invisible. Dark mode's 8-vs-16.7%
                      // step carries the shape on its own and the hairline
                      // disappears into it. Ring, not border: it must not
                      // steal 1px of content box when selection moves.
                      current
                        ? "bg-page-bg text-text shadow-sm ring-1 ring-border-outer"
                        : "text-text-tertiary hover:text-text",
                    )}
                  >
                    <Icon className="h-3.5 w-3.5 shrink-0" />
                    {t(`settings.${value}`)}
                  </button>
                );
              })}
            </div>
          </div>

          {/* Theme pack — the palette layer on top of the light/dark mode.
              Same row grammar as the mode rows above so the two sections
              read as one control family. "Default" restores the built-in
              :root/.dark palette; each other row is a CSS file dropped into
              styles/themes/ (auto-discovered, nothing to register). */}
          {TINTED_PACKS.length > 0 && (
            <div
              className="border-b border-border-divider pb-2 pt-2"
              role="radiogroup"
              aria-label={t("settings.themePack")}
            >
              <p className="px-1 pb-1.5 text-xs text-text-tertiary">
                {t("settings.themePack")}
              </p>
              <div className="flex flex-col gap-0.5">
                {[
                  { id: DEFAULT_THEME_PACK, name: t("settings.themeDefault"), hue: null as number | null },
                  ...TINTED_PACKS,
                ].map((pack) => {
                  const current = activePack === pack.id;
                  return (
                    <button
                      key={pack.id}
                      type="button"
                      role="radio"
                      aria-checked={current}
                      onClick={() => pickThemePack(pack.id)}
                      className={cn(
                        "flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-xs transition-colors",
                        current ? listRowSelected : listRowHover,
                      )}
                    >
                      {/* The row preview is a colour dot — same grammar as
                          the Settings ▸ Appearance radio dots, and both
                          sides take the colour from the shared `packTint`.
                          The Layers glyph it replaces was not a theme icon
                          anyway; "Default" has no tint of its own and
                          keeps the neutral radio-dot zinc. */}
                      <span
                        className="h-3.5 w-3.5 shrink-0 rounded-full"
                        style={{
                          backgroundColor:
                            packTint(pack.hue) ?? "var(--ui-radio-bg)",
                        }}
                      />
                      <span className="flex-1">{pack.name}</span>
                      {current && (
                        <Check
                          className="h-3 w-3 shrink-0"
                          style={{ color: "var(--color-accent)" }}
                          aria-hidden="true"
                        />
                      )}
                    </button>
                  );
                })}
              </div>
            </div>
          )}

          {/* Accent color — a swatch grid, matching the Settings ▸
              Appearance tab's swatches (same size, same ring treatment)
              so the two read as the same control. */}
          <div className="pt-2 pb-2">
            {/* `pb-3`, not the theme section's `pb-1.5`: the selected swatch
                is `scale-110` with a `ring-2 ring-offset-2`, so its paint
                reaches ~6px past the top of its 36px box. A 6px label gap is
                therefore no gap at all — the ring lands on the caption. The
                theme rows sit inside `py-1.5` buttons and need no such
                allowance. `mb-3` is also what Settings ▸ Appearance uses for
                this same caption (SettingsPage.tsx). */}
            <p className="px-1 pb-3 text-xs text-text-tertiary">
              {t("settings.accentColor")}
            </p>
            <AccentSwatches
              value={accentColor}
              customColor={customAccent}
              onChange={setAccentColor}
              className="px-1"
            />
          </div>
        </div>
      )}
    </div>
  );
}
