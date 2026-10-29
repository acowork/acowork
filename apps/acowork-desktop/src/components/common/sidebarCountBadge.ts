/**
 * sidebarCountBadge.ts — the one number-pill shape every count badge wears.
 *
 * The PM board already dressed its column counts as bubbles (`rounded-full`
 * + zinc fill + `tabular-nums`), while the chat sidebar's group banners
 * (Users / Project groups) still rendered theirs as bare dimmed text. The
 * same number then read as a different kind of element depending on which
 * pane it was in.
 *
 * Two constants, not one:
 *   - `COUNT_BADGE_BASE_CN`   — the shape only (rounding, padding, type
 *     scale, tabular figures). Every bubble uses it, so the three places
 *     cannot drift apart on geometry.
 *   - `COUNT_BADGE_NEUTRAL_CN` — the shape plus the neutral zinc fill, for
 *     counts that carry no special meaning. Callers that need a tinted
 *     variant (PM's amber "submitted" column) compose the base + their own
 *     colours instead of copying the whole class list.
 *
 * `tabular-nums` keeps the pill from jittering as the count changes.
 */
export const COUNT_BADGE_BASE_CN =
  "rounded-full px-1.5 py-px text-10 font-medium tabular-nums";

export const COUNT_BADGE_NEUTRAL_CN =
  `${COUNT_BADGE_BASE_CN} bg-zinc-200 text-text-secondary dark:bg-zinc-700`;
