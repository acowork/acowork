/**
 * Format a millisecond timestamp for the chat bubble hover tooltip.
 *
 * - Uses the user's locale + timezone via `toLocaleString` with explicit
 *   fields (year/month/day/hour/minute/second) so the output is stable
 *   across runtimes. Relying on the default `toLocaleString()` format
 *   produces subtly different strings between Tauri WebKit, Chromium,
 *   and jsdom tests (e.g. Tauri inserts " " between date and time on
 *   some locales).
 * - Returns "" for falsy / invalid input so the bubble component can
 *   skip rendering the timestamp span entirely when a legacy message has
 *   no usable timestamp.
 */
// One formatter for the whole app: `toLocaleString([], opts)` constructs a
// fresh Intl.DateTimeFormat per call, which the CPU profile showed burning
// ~1.7s of self time across a single streaming minute (every bubble, every
// re-render). The options are constant, so the formatter is too.
const bubbleFormatter = new Intl.DateTimeFormat([], {
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
});

export function formatBubbleTime(ms: number | undefined | null): string {
  if (!ms) return "";
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return "";
  return bubbleFormatter.format(d);
}