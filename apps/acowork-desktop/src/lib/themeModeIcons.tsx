/**
 * The theme-mode glyphs — ONE source for both doors.
 *
 * Settings ▸ Appearance (radio rows) and the title-bar appearance menu
 * (segmented control) are two doors to one state, so they must show the
 * SAME glyph for a mode; a Sun next to "Light" in one place and a
 * circle next to "Light" in the other reads as two different settings.
 */
import { Monitor, Moon, Sun, type LucideIcon } from "lucide-react";
import type { Theme } from "./types";

export const THEME_MODE_ICONS: Record<Theme, LucideIcon> = {
  light: Sun,
  dark: Moon,
  system: Monitor,
};

export function ThemeModeIcon({
  mode,
  className = "h-3.5 w-3.5",
}: {
  mode: Theme;
  className?: string;
}) {
  const Icon = THEME_MODE_ICONS[mode];
  return <Icon className={className} />;
}
