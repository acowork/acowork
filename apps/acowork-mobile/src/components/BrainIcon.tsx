/**
 * Outline brain — the execution-session icon, the mobile twin of the
 * desktop app's `components/common/BrainIcon.tsx` (design 27 §6.1).
 *
 * The paths are copied, not re-derived: the mark is a cross-surface identity
 * (an exec surface reads as "the agent's thinking" on both apps), and the
 * desktop source is the one that owns the silhouette. If it changes there,
 * this file changes with it in the same commit.
 */

export const BRAIN_HEMISPHERE_LEFT =
  'M12 5a3 3 0 1 0-5.997.125 4 4 0 0 0-2.526 5.77 4 4 0 0 0 .556 6.588A4 4 0 1 0 12 18Z'
export const BRAIN_HEMISPHERE_RIGHT =
  'M12 5a3 3 0 1 1 5.997.125 4 4 0 0 1 2.526 5.77 4 4 0 0 1-.556 6.588A4 4 0 1 1 12 18Z'

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
      aria-hidden
    >
      <path d={BRAIN_HEMISPHERE_LEFT} />
      <path d={BRAIN_HEMISPHERE_RIGHT} />
    </svg>
  )
}
