import { useId } from "react";

/**
 * Chat-history icon — a rounded speech bubble carrying three dots, the
 * WeChat-style glyph for "browse this conversation's records".
 *
 * It replaces the generic `History` (clock-arrow) lucide icon, which read as
 * "recent activity" rather than "chat". The bubble + ellipsis says "a
 * conversation, with more inside" — matching the entry it labels on all three
 * chat surfaces (1:1 agent, project group, and the inbox twin to come).
 *
 * Stroke geometry is shared verbatim by the outline + filled variants so the
 * silhouette is identical; only the fill treatment differs. `size` drives the
 * rendered box (the old `History size={14|16}` call sites map straight over).
 */

/** The bubble outline + three dots, drawn in `currentColor` strokes. */
function Bubble({ filled }: { filled: boolean }) {
  return (
    <>
      <path
        d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z"
        fill={filled ? "currentColor" : "none"}
      />
      {/* Dots stay paper-coloured (cut out) when the bubble is filled, so the
          "more" reading survives the active state. */}
      <circle cx="8.5" cy="11.5" r="1" fill={filled ? "var(--color-chat-body, #fff)" : "currentColor"} stroke="none" />
      <circle cx="12" cy="11.5" r="1" fill={filled ? "var(--color-chat-body, #fff)" : "currentColor"} stroke="none" />
      <circle cx="15.5" cy="11.5" r="1" fill={filled ? "var(--color-chat-body, #fff)" : "currentColor"} stroke="none" />
    </>
  );
}

export function OutlineChatHistoryIcon({ className, size = 16 }: { className?: string; size?: number }) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <Bubble filled={false} />
    </svg>
  );
}

/**
 * Filled variant for a selected/active rail tile. The bubble body fills with
 * `currentColor` while the three dots are punched out via an SVG `<mask>`
 * (same technique as `FilledSettingsIcon`), so the glyph stays legible rather
 * than collapsing into a solid blob.
 */
export function FilledChatHistoryIcon({ className, size = 16 }: { className?: string; size?: number }) {
  const maskId = useId();
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="currentColor"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <defs>
        <mask id={maskId}>
          <rect width="24" height="24" fill="white" />
          <circle cx="8.5" cy="11.5" r="1.1" fill="black" />
          <circle cx="12" cy="11.5" r="1.1" fill="black" />
          <circle cx="15.5" cy="11.5" r="1.1" fill="black" />
        </mask>
      </defs>
      <g mask={`url(#${maskId})`}>
        <path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z" />
      </g>
    </svg>
  );
}
