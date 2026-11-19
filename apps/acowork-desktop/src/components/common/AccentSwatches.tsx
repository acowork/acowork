import { useRef } from "react";
import { Pencil, Plus } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { ACCENT_PRESETS } from "../../lib/accentPresets";
import { cn } from "../../lib/utils";
import { Tooltip } from "../common/Tooltip";

/**
 * Accent color picker — the eleven presets plus a custom color, shared by the
 * title-bar menu and Settings ▸ Appearance.
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
 *
 * The saved custom color is a separate value from the active accent, so the
 * trailing slot has three states: no custom color yet (a dashed "+"), a saved
 * one while a preset is active (its swatch, unringed), and a saved one that is
 * active (its swatch, ringed). Collapsing the first two is what made a custom
 * color look destroyed the moment the user tried a preset.
 *
 * Clicking the slot IS the choice: it commits the saved custom color as the
 * active accent and only then opens the picker for an optional re-pick.
 * Making the commit depend on the picker firing `change` meant "I just want to
 * switch to my custom color" was impossible — a user who closed the picker
 * without touching it stayed on the preset.
 */
export function AccentSwatches({
  value,
  customColor,
  onChange,
  className,
  columns = 12,
}: {
  /** The active accent as `#rrggbb`. */
  value: string;
  /** The last color the user mixed, or `null`. Shown in the trailing slot
   *  even while a shipped preset is the active accent, so a custom color
   *  survives browsing the presets and can be returned to in one click. */
  customColor: string | null;
  onChange: (hex: string) => void;
  /** Layout classes for the row; the two hosts space their grids alike
   *  today but own different containers. */
  className?: string;
  /** How many swatches fit on one line. The count is TWELVE (eleven presets
   *  plus the custom slot) and the two hosts have different widths, so they
   *  split it differently: Settings ▸ Appearance is wide enough for one line
   *  of twelve, the title-bar popover is not — it takes two lines of six. */
  columns?: 6 | 12;
}) {
  const { t } = useTranslation();
  const inputRef = useRef<HTMLInputElement>(null);
  // "Does the user have a custom color" and "is it the active one" are two
  // different questions — the first decides what the slot paints, the second
  // decides whether it is ringed.
  const hasCustom = customColor !== null;
  const customActive = hasCustom && value.toLowerCase() === customColor.toLowerCase();

  return (
    /* A fixed-column grid, not a wrapping flex row: eleven presets plus the
       custom slot make TWELVE swatches, and a flex row broke at whatever
       width the host happened to be, so the second line held a ragged
       remainder and the two doors looked like different controls. The
       column count is the host's call — Settings takes one line of twelve,
       the narrow title-bar popover two lines of six. */
    <div
      className={cn(
        // `justify-items-center`: a grid item stretches to its cell by
        // default, which would left-align the swatch inside a cell wider
        // than the swatch and space the row unevenly.
        "relative grid justify-items-center gap-x-1 gap-y-2",
        columns === 6 ? "grid-cols-6" : "grid-cols-12",
        className,
      )}
    >
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
                // 32px, not 36px: twelve slots (eleven presets + the custom
                // one) have to fit the narrowest host — the title-bar popover
                // — in one clean line, and the Settings row keeps its twelve
                // on one line too. The gap shrank with the swatch for the
                // same reason.
                "flex h-8 w-8 items-center justify-center rounded-full transition-transform",
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

      {/* The trailing slot is the picker trigger and, once a color has been
          mixed, that color's swatch — so the pick stays visible and
          re-editable rather than vanishing on the next repaint.

          A saved custom color must not look like just another preset: the
          swatch is a plain circle either way, so the pencil badge is what says
          "this one is yours, click to re-pick". Same badge vocabulary as the
          My Profile avatar's edit corner (ProfileTab.tsx), scaled from a 64px
          avatar to a 36px swatch. */}
      <Tooltip content={t("accentColors.custom")} variant="plain">
        <button
          type="button"
          onClick={() => {
            // Switching to the saved custom color is a complete action on its
            // own — commit it before the picker opens, so closing the picker
            // untouched leaves the custom slot selected. With no custom color
            // yet there is nothing to select; the picker's `change` supplies
            // it.
            if (hasCustom && !customActive) onChange(customColor);
            inputRef.current?.click();
          }}
          aria-label={t("accentColors.custom")}
          aria-pressed={customActive}
          data-accent-id="custom"
          className={cn(
            "relative flex h-8 w-8 items-center justify-center rounded-full border transition-transform",
            customActive
              ? "scale-110 border-transparent ring-2 ring-offset-2 ring-offset-white dark:ring-offset-zinc-900"
              : hasCustom
                ? "border-transparent hover:scale-105"
                : "border-dashed border-border-divider text-text-tertiary hover:scale-105 hover:text-text-secondary",
          )}
          style={
            {
              // Paint the *saved* custom color, never `value`: while a preset
              // is active, painting the active accent would show that preset a
              // second time and make the custom color look gone.
              ...(hasCustom
                ? { backgroundColor: customColor, "--tw-ring-color": customColor }
                : null),
            } as React.CSSProperties
          }
        >
          {hasCustom ? (
            <span
              data-accent-edit-badge=""
              className="absolute -bottom-0.5 -right-0.5 flex h-3 w-3 items-center justify-center rounded-full bg-btn-solid text-white shadow-sm"
            >
              <Pencil className="h-1.5 w-1.5" strokeWidth={3} />
            </span>
          ) : (
            <Plus className="h-4 w-4" />
          )}
        </button>
      </Tooltip>

      <input
        ref={inputRef}
        type="color"
        tabIndex={-1}
        aria-hidden="true"
        // Re-opening starts from the saved custom color, not from black or
        // from the active preset — the user is editing *that* color.
        // Lowercased because a color input's value must be a *valid simple
        // color* (HTML spec: lowercase hex); anything else the browser
        // rewrites to #000000, and green's authored hex is `#00C375`.
        value={(customColor ?? value).toLowerCase()}
        onChange={(e) => onChange(e.target.value)}
        className="pointer-events-none absolute h-0 w-0 opacity-0"
      />
    </div>
  );
}
