/**
 * Outline brain — two mirrored hemispheres, stroke-only.
 *
 * The execution-session icon (design 27 §6.1): the tab strip and the
 * session-history dropdown use this pair so an exec tab reads as "the
 * agent's thinking/execution process" at a glance, visually distinct
 * from the chat bubble that marks the pinned Chat tab and the NavBar.
 * Pairs with `FilledBrainIcon`; same stroke width / viewBox language as
 * `OutlineChatIcon` so both icons sit on one visual system at 14px.
 */

// Shared silhouette: each hemisphere is its own closed path meeting at
// the x=12 center line. Stroke → lucide-style brain outline; fill →
// two solid lobes (see FilledBrainIcon for the center-gap trick).
export const BRAIN_HEMISPHERE_LEFT =
  "M12 5a3 3 0 1 0-5.997.125 4 4 0 0 0-2.526 5.77 4 4 0 0 0 .556 6.588A4 4 0 1 0 12 18Z";
export const BRAIN_HEMISPHERE_RIGHT =
  "M12 5a3 3 0 1 1 5.997.125 4 4 0 0 1 2.526 5.77 4 4 0 0 1-.556 6.588A4 4 0 1 1 12 18Z";

export function OutlineBrainIcon({ className }: { className?: string }) {
  return (
    <svg
      className={className}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d={BRAIN_HEMISPHERE_LEFT} />
      <path d={BRAIN_HEMISPHERE_RIGHT} />
    </svg>
  );
}

/**
 * Filled brain — same two-lobe silhouette, solid fill.
 *
 * Used for the selected/active exec tab and the processing (breathing)
 * state. The hemispheres are pushed 0.35 units apart so the central
 * cleft survives the solid fill — without it the two lobes merge into
 * one featureless blob at 14px.
 */
export function FilledBrainIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="currentColor">
      <g transform="translate(-0.35, 0)">
        <path d={BRAIN_HEMISPHERE_LEFT} />
      </g>
      <g transform="translate(0.35, 0)">
        <path d={BRAIN_HEMISPHERE_RIGHT} />
      </g>
    </svg>
  );
}
