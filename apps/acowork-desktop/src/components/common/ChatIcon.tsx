/**
 * Outline chat bubble — pill/oval silhouette, stroke-only.
 *
 * Single source of truth for the chat icon: the left NavBar and the
 * pinned Chat tab in the SessionTabBar import this so the Chat-surface
 * icon stays visually identical across the app. Execution sessions use
 * the brain pair instead (BrainIcon.tsx, design 27 §6.1).
 */
export function OutlineChatIcon({ className }: { className?: string }) {
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
      <g transform="translate(1.2, 1.2) scale(0.9)">
        <path d="M12 3C6.5 3 2 7.1 2 12c0 2.5 1.1 4.8 2.9 6.5L3 22l5.3-2.3C9.6 20.5 11.2 21 13 21c5.5 0 10-4.1 10-9s-4.5-9-11-9z" />
      </g>
    </svg>
  );
}

/**
 * Filled chat bubble — same pill/oval silhouette, solid fill.
 *
 * Used for the selected/active state (current nav item in NavBar, the
 * pinned Chat tab in SessionTabBar). Pairs with `OutlineChatIcon`.
 */
export function FilledChatIcon({ className }: { className?: string }) {
  return (
    <svg
      className={className}
      viewBox="0 0 24 24"
      fill="currentColor"
    >
      <g transform="translate(1.2, 1.2) scale(0.9)">
        <path d="M12 3C6.5 3 2 7.1 2 12c0 2.5 1.1 4.8 2.9 6.5L3 22l5.3-2.3C9.6 20.5 11.2 21 13 21c5.5 0 10-4.1 10-9s-4.5-9-11-9z" />
      </g>
    </svg>
  );
}
