import { useRef } from "react";
import { Plus } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { ACCENT_PRESETS, isCustomAccent } from "../../lib/accentPresets";
import { cn } from "../../lib/utils";
import { Tooltip } from "../common/Tooltip";

/**
 * Accent color picker — the ten presets plus a custom color, shared by the
 * title-bar appearance menu and Settings ▸ Appearance.
 *
 * Both entry points drive the same `settingsStore.accentColor`, so they must
 * render the same control or the two doors show different rooms. Keeping it
 * in one place also means the custom-color affordance exists once.
 *
 * The custom color uses the native `<input type="color">` rather than a
 * hand-rolled palette: the OS picker already does hue/saturation picking,
 * remembers the last color itself, and needs no new dependency. Persistence
 * is on `change` (the commit event, fired when the picker closes), not
 * `input` (which fires per pixel while dragging) — so a drag through the
 * whole spectrum writes one value, matching "save after picking a color".
 */
export function AccentSwatches({
  value,
  onChange,
  className,
}: {
  /** The active accent as `#rrggbb`. */
  value: string;
  onChange: (hex: string) => void;
  /** Layout classes for the row; the two hosts space their grids alike
   *  today but own different containers. */
  className?: string;
}) {
  const { t } = useTranslation();
  const inputRef = useRef<HTMLInputElement>(null);
  const custom = isCustomAccent(value);

  return (
    <div className={cn("relative flex flex-wrap gap-[14px]", className)}>
      {ACCENT_PRESETS.map((c) => {
        const current = value.toLowerCase() === c.hex.toLowerCase();
        return (
          <Tooltip content={t(`accentColors.${c.id}`)} variant="plain" key={c.id}>
            <button
              type="button"
              onClick={() => onChange(c.hex)}
              aria-label={t(`accentColors.${c.id}`)}
              aria-pressed={current}
              data-accent-id={c.id}
              className={cn(
                "flex h-9 w-9 items-center justify-center rounded-full transition-transform",
                current
                  ? "scale-110 ring-2 ring-offset-2 ring-offset-white dark:ring-offset-zinc-900"
                  : "hover:scale-105",
              )}
              style={
                {
                  backgroundColor: c.hex,
                  "--tw-ring-color": c.hex,
                } as React.CSSProperties
              }
            />
          </Tooltip>
        );
      })}

      {/* The trailing slot doubles as the picker trigger and as the current
          custom swatch, so a picked color stays visible and re-editable
          instead of vanishing the moment the menu repaints. */}
      <Tooltip content={t("accentColors.custom")} variant="plain">
        <button
          type="button"
          onClick={() => inputRef.current?.click()}
          aria-label={t("accentColors.custom")}
          aria-pressed={custom}
          data-accent-id="custom"
          className={cn(
            "flex h-9 w-9 items-center justify-center rounded-full border transition-transform",
            custom
              ? "scale-110 border-transparent text-white ring-2 ring-offset-2 ring-offset-white dark:ring-offset-zinc-900"
              : "border-dashed border-border-divider text-text-tertiary hover:scale-105 hover:text-text-secondary",
          )}
          style={
            {
              ...(custom ? { backgroundColor: value, "--tw-ring-color": value } : null),
            } as React.CSSProperties
          }
        >
          {custom ? null : <Plus className="h-4 w-4" />}
        </button>
      </Tooltip>

      <input
        ref={inputRef}
        type="color"
        tabIndex={-1}
        aria-hidden="true"
        // Open at whatever is active, so re-picking starts from the current
        // color rather than a fixed preset. Lowercased because the color
        // input's value must be a *valid simple color* (HTML spec: lowercase
        // hex) — anything else the browser sanitizes to `#000000`, and green's
        // authored hex is `#00C375`.
        value={value.toLowerCase()}
        onChange={(e) => onChange(e.target.value)}
        className="pointer-events-none absolute h-0 w-0 opacity-0"
      />
    </div>
  );
}
