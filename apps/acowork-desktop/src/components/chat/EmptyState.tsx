import brandMarkGray from "../../../../../assets/brand-mark-gray.svg";

/**
 * EmptyState — placeholder shown inside the chat surface when the current
 * session has zero messages. Kept visually quiet so it never competes with
 * actual conversation content once the user starts typing.
 *
 * Layout: brand mark on top, small uppercase tagline below.
 *
 * Visual hierarchy (intentional, both themes):
 *   - Logo      : 20% opacity, ~44px tall, brand identity (passive)
 *   - Tagline   : 40% opacity, 10px, bold + wide tracking, functional hint
 * The tagline is *more* opaque than the logo so it stays legible — a
 * completely uniform 20% would render bold uppercase glyphs as an
 * unreadable gray smudge. Logo still leads the eye; tagline earns its
 * legibility budget.
 *
 * Uses the grayscale brand mark (brand-mark-gray.svg). That asset carries a
 * single dark-toned palette (chip #3A→#2A, "A" #E8/#C8/#A8, wordmark #A8/#C8/#7A)
 * picked to stay readable on both the light and the dark chat surface, so it no
 * longer switches palette itself and this component needs no React-side theme
 * handling. The tagline relies on `text-text-tertiary/40` (Tailwind opacity
 * modifier) so it inherits the surrounding surface tone in both light and dark
 * modes.
 */
export function EmptyState(): React.ReactElement {
  return (
    <div
      className="absolute inset-0 flex flex-col items-center justify-center gap-3"
      // Pointer events stay on (default); the empty surface must still
      // receive clicks for things like "scroll to bottom" if added later.
      aria-label="ACowork"
    >
      <img
        src={brandMarkGray}
        alt=""
        // 166 × 96/364.25 ≈ 43.7 tall: the asset's frame is now the artwork plus
        // a uniform 6px gutter (it used to carry ~30px of dead space on the right),
        // so the width is scaled down to keep the mark at the documented ~44px.
        width={166}
        height={44}
        // Don't announce the decorative mark to screen readers — the
        // surrounding chat surface already exposes the app context.
        aria-hidden="true"
        draggable={false}
        // maxWidth guards against future viewBox tweaks blowing up the
        // placeholder if the asset is ever swapped.
        style={{ maxWidth: "60%", height: "auto", opacity: 0.2 }}
      />
      <p className="text-10 font-bold uppercase tracking-[0.25em] text-text-tertiary/40 ">
        Talk with your AI colleague
      </p>
    </div>
  );
}
