import { useRef, useState } from "react";
import { Check, Monitor, Moon, Palette, Sun } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useSettingsStore } from "../../stores/settingsStore";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";
import { cn } from "../../lib/utils";
import { Tooltip } from "../common/Tooltip";
import { AccentSwatches } from "../common/AccentSwatches";
import { titleBarMenuPanel, titleBarMenuTrigger, useDismissMenu } from "./titleBarMenu";
import type { Theme } from "../../lib/types";

/** Theme-mode rows, in the order the Settings ▸ Appearance tab lists
 *  them. The glyph is part of the row (not just a check mark) so the
 *  three modes read as three named states, not a bare radio list. */
const THEME_ROWS: { value: Theme; icon: typeof Sun }[] = [
  { value: "light", icon: Sun },
  { value: "dark", icon: Moon },
  { value: "system", icon: Monitor },
];

/**
 * Title-bar appearance quick-settings — theme mode + global accent color.
 *
 * A shortcut for the two things people change most often and change
 * most often BY ACCIDENT OF NAVIGATION: the theme and the accent color
 * live in Settings ▸ Appearance, three clicks and a page transition
 * away. This menu is the same two settings, one click from the title
 * bar.
 *
 * It owns NO state. Both values are read from and written through
 * `settingsStore` (`theme` / `setTheme`, `accentColor` / `setAccentColor`),
 * which is the same store the Settings ▸ Appearance tab drives — so the
 * two entry points are two doors to one state, and picking a mode here
 * is immediately visible there (and vice versa). The store persists both
 * to localStorage, so the choice survives a restart.
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
          {/* Theme mode — three rows, one per mode. The active row carries
              the app-wide `listRowSelected` tint and a check mark, the
              same vocabulary the Gateway menu uses for "the current
              address". `radiogroup`/`radio` (not `menuitemradio`): this
              is a set of mutually exclusive choices, and a `menuitemradio`
              would claim a `menu` ancestor the panel does not have. */}
          <div className="border-b border-border-divider pb-2" role="radiogroup" aria-label={t("settings.theme")}>
            <p className="px-1 pb-1.5 text-xs text-text-tertiary">
              {t("settings.theme")}
            </p>
            <div className="flex flex-col gap-0.5">
              {THEME_ROWS.map(({ value, icon: Icon }) => {
                const current = theme === value;
                return (
                  <button
                    key={value}
                    type="button"
                    role="radio"
                    aria-checked={current}
                    onClick={() => setTheme(value)}
                    className={cn(
                      "flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-xs transition-colors",
                      current ? listRowSelected : listRowHover,
                    )}
                  >
                    <Icon className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
                    <span className="flex-1">{t(`settings.${value}`)}</span>
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

          {/* Accent color — a swatch grid, matching the Settings ▸
              Appearance tab's swatches (same size, same ring treatment)
              so the two read as the same control. */}
          <div className="pt-2">
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
