/**
 * CircularProgressIcon — the shared usage-ring glyph.
 *
 * One glyph, two surfaces: the execution session's `ContextUsageIcon`
 * and the Chat session's `ChatUsageIcon`. The two composers sit in the
 * same right-hand toolbar cluster, so a divergent glyph reads as "a
 * different kind of control" when it is in fact the same one. Keeping
 * the drawing here is what guarantees they cannot drift.
 *
 * Presentational only — it takes a percentage and draws it. Neither
 * surface's semantics (exec = assembled context window, chat = router
 * history budget) leak in.
 */

/** Circular progress ring showing usage percentage.
 *  Starts from bottom (6 o'clock), goes clockwise.
 *  16x16 SVG to match adjacent send button icon size. */
export function CircularProgressIcon({ usagePercent }: { usagePercent: number }) {
  const size = 16;
  const strokeWidth = 2;
  const radius = (size - strokeWidth) / 2;
  const center = size / 2;
  const circumference = 2 * Math.PI * radius;
  // Clamp: a caller may legitimately exceed 100% (the exec surface
  // budgets against a soft window), and a negative offset would draw a
  // ring past its own start point.
  const pct = Math.min(100, Math.max(0, usagePercent));
  const offset = circumference * (1 - pct / 100);

  const fillColor = "var(--color-text-secondary, hsl(240 3.7% 46.9%))";
  const emptyColor = "var(--shimmer-mid, #e8e8ec)";

  return (
    <svg
      width={size}
      height={size}
      viewBox={`0 0 ${size} ${size}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
    >
      {/* Background ring (total capacity) */}
      <circle
        cx={center}
        cy={center}
        r={radius}
        stroke={emptyColor}
        strokeWidth={strokeWidth}
        opacity={0.35}
      />
      {/* Progress ring (used amount) — starts from bottom, goes clockwise */}
      <circle
        cx={center}
        cy={center}
        r={radius}
        stroke={fillColor}
        strokeWidth={strokeWidth}
        strokeLinecap="round"
        strokeDasharray={circumference}
        strokeDashoffset={offset}
        transform={`rotate(90 ${center} ${center})`}
        style={{ transition: "stroke-dashoffset 0.3s ease" }}
      />
    </svg>
  );
}