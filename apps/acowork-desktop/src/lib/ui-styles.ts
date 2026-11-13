/**
 * Global UI style constants for consistent component appearance.
 * All input fields, buttons, and interactive elements should use these tokens.
 */

// ── Input field styles ──────────────────────────────────────────────

/** Standard input field (text, number, etc.) */
export const inputBase =
  "w-full rounded border border-input-border bg-input-bg px-3 py-[var(--ui-input-py)] text-xs outline-none transition-colors focus:border-[var(--color-accent)] ";

/** Read-only input field */
export const inputReadonly =
  "rounded border border-zinc-200 bg-zinc-50 px-3 py-[var(--ui-input-py)] text-xs dark:border-zinc-700 dark:bg-zinc-800 ";

// Note: dropdown styling was historically provided via `selectBase` and
// `selectArrowStyle` here. It now lives in
// `components/common/dropdownTheme.ts`, consumed by `Dropdown.tsx` and by
// every popup-menu surface. All call sites have been migrated; do not
// reintroduce these tokens or a second dropdown surface palette.

/** Font-mono input (for API keys, codes) */
export const inputMono =
  "rounded border border-zinc-200 px-3 py-[var(--ui-input-py)] font-mono text-xs dark:border-zinc-700 dark:bg-zinc-900 ";

// ── Button styles ───────────────────────────────────────────────────

/**
 * Toolbar button (borderless, compact) — used for Model/Workspace selectors.
 *
 * The `disabled:` variants are load-bearing, not decoration: ADR-076 read-only
 * sessions gate the whole session-write toolbar on the `disabled` attribute, and
 * a bare `disabled` with no visual difference is indistinguishable from an
 * enabled button — users click and nothing happens. `hover:bg-transparent` is
 * the key part: without it a disabled button still lights up on hover while
 * refusing the click, which reads as a broken control.
 */
export const toolbarButton =
  "inline-flex items-center gap-1 rounded px-2 py-1.5 text-xs transition-colors text-text-tertiary hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200 disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent dark:disabled:hover:bg-transparent";

/**
 * Composer right-cluster icon button — visibility / context-usage /
 * attachment / send.
 *
 * Fixed 28x28 (`h-7 w-7`) + `rounded-md` so all four hover highlight
 * rectangles are identical. Do NOT reach for `toolbarButton` here: it is
 * sized for a text label (`px-2 py-1.5` breathing room around the label),
 * which renders 30px wide with a 4px radius — 2px wider and visibly
 * boxier next to these. Icon-only buttons share this token instead.
 *
 * `disabled:*` carries the same load as `toolbarButton`: a disabled send
 * button with no visual difference is indistinguishable from an enabled
 * one, and `hover:bg-transparent` keeps a disabled button from lighting
 * up while refusing the click.
 */
export const toolbarIconButton =
  "inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-md transition-colors text-text-tertiary hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200 disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent dark:disabled:hover:bg-transparent";

// Toolbar "open / active" chrome now lives in
// `components/common/dropdownTheme.ts` (`DROPDOWN_TRIGGER_ACTIVE_CLASS`).
// A second literal copy used to sit here as `toolbarButtonActive`; it had no
// consumers and was removed so the recipe cannot drift. A grey active state
// is exactly what made an open dropdown read as a different widget from its
// own menu.

/**
 * List-row tokens.
 *
 * One tint family drives every list in the app — popups (model · account
 * · reasoning · workspace · skills), panels (`ListRow` / `ExpandableRow`
 * over `ListBox`), the todo/queued-message rows above the input, the
 * file tree, the ask-question options, the search results, etc. — so the
 * cursor passing over a row reads the same way everywhere.
 *
 * The four states step on one accent scale (`--color-accent` is the
 * preset the user picked; the percentages below keep the tint legible on
 * both light and dark surfaces without re-defining a separate dark tone):
 *
 *   listRowHover         — cursor passing over an unselected row
 *   listRowHoverInset    — same, on the darker sub-surface revealed by
 *                          an expanded card (one step stronger so it
 *                          still reads on the inset body)
 *   listRowSelected      — the current selection
 *   listRowSelectedInset — same, on the inset sub-surface
 *
 * The selected state is always at least one step louder than the
 * unselected hover, so "which row is active" is the first thing the eye
 * picks out in a long list. The tokens only set the background — the
 * LABEL keeps the unselected rows' own foreground so we don't stamp
 * accent text on top of an accent wash (that reads as "warning", not
 * as "this one"). The check mark / leading icon stays accent, and
 * carries the "selected" semantic by itself.
 *
 * `menuItemHover` / `menuItemSelected` are kept as aliases for the popup
 * menus so the call sites that imported them by their old names still
 * typecheck. New code should reach for `listRow*` directly.
 */

// Cursor over an unselected row on the raised list surface (default).
// One tint family for the whole app: every list — popups, side panels,
// file tree, search results, todo/queued rows, ask options, subtask
// tree, review queue, trash, etc. — shares the same accent /10 (light)
// / /15 (dark). The previous split between "card" and "inset" tints
// read as two different list families when the cursor crossed between
// them (AgentList vs FileTree was the canonical pair), so the cursor
// passing over a row now reads the same way everywhere.
export const listRowHover =
  "hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15";

// Cursor over an unselected row on the inset sub-surface (one step deeper).
// Reserved for the body revealed by an ExpandableRow header (PROMPT /
// context-snapshot / compression-history / tool lists), which sits one
// surface darker than the rest of the app.
export const listRowHoverInset =
  "hover:bg-[var(--color-accent)]/15 dark:hover:bg-[var(--color-accent)]/20";

// The current selection on the raised list surface.
export const listRowSelected =
  "bg-[var(--color-accent)]/15 hover:bg-[var(--color-accent)]/15 dark:bg-[var(--color-accent)]/20 dark:hover:bg-[var(--color-accent)]/20";

// The current selection on the inset sub-surface.
export const listRowSelectedInset =
  "bg-[var(--color-accent)]/20 hover:bg-[var(--color-accent)]/20 dark:bg-[var(--color-accent)]/25 dark:hover:bg-[var(--color-accent)]/25";

/**
 * @deprecated use `listRowSelected` — kept as an alias so the popup
 * call sites that imported `menuItemSelected` keep working.
 */
export const menuItemSelected = listRowSelected;

/**
 * @deprecated use `listRowHover` — kept as an alias so the popup call
 * sites that imported `menuItemHover` keep working.
 */
export const menuItemHover = listRowHover;

/** Dialog action button (Cancel/Save) — fixed width */
export const dialogButton =
  "w-20 rounded px-3 py-[var(--ui-btn-py)] text-xs font-medium text-center";

/** Dialog primary action (Save) */
export const dialogButtonPrimary =
  "w-20 rounded bg-zinc-800 px-3 py-[var(--ui-btn-py)] text-xs font-medium text-center text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600";

/** Dialog secondary action (Cancel) */
export const dialogButtonSecondary =
  "w-20 rounded px-3 py-[var(--ui-btn-py)] text-xs font-medium text-center text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-700";

// ── Interaction-card chrome ─────────────────────────────────────────

/**
 * The one card chrome: hairline shell, neutral title banner, agent-bubble
 * body. Written for `InteractionCardShell` and reused as-is by the Chat
 * execution block, whose title banner IS its collapse header and whose body
 * is the run's latest answer — a second copy of the recipe is a drift
 * waiting to happen (`chatCardChrome.test.tsx` pins the rule).
 *
 * Two title banners, one geometry: `chatCardTitleBar` is the card palette's
 * own (`--color-chat-title`, the same tone a code block's title wears), while
 * `chatCardTitleBarMuted` is one step lighter — a run's banner sits in the
 * transcript and must not out-shout the answer under it. The DARK step is
 * identical in both, so the muted variant's dark side references the token
 * directly (`dark:bg-chat-title`) rather than re-stating the tone; the quiet
 * variant only lightens the light theme, where the literal zinc-100/60 is
 * deliberately one step below `--color-chat-title`'s light value.
 */
export const chatCardShell = "overflow-hidden rounded-md border border-chat-border";
const chatCardTitleBarBase =
  "flex w-full items-center gap-[7px] border-b border-border-divider px-3 py-[7px] text-left text-xs font-medium text-text";
export const chatCardTitleBar = `${chatCardTitleBarBase} bg-chat-title`;
export const chatCardTitleBarMuted = `${chatCardTitleBarBase} bg-zinc-100/60 dark:bg-chat-title`;
export const chatCardBody = "bg-chat-bubble px-3 py-2.5";

// ── Interaction-card buttons ────────────────────────────────────────

/**
 * The approval pair every interaction surface uses: one side of one decision
 * sits on the accent, the other stays neutral. The recipe was written in
 * `ExploreBlock` (the execution surface's tool-approval row) and copied
 * verbatim into `ChatExecutionBlock`'s card; a third copy would drift the
 * first time the pair changes. The accent side keeps its background as an
 * inline `var(--color-accent-strong)` — the ONE place the highlight color is
 * allowed to paint (see `chatCardChrome.test.tsx`).
 */
export const cardButtonNeutral =
  "rounded-md border border-zinc-300 px-2 py-0.5 text-11 font-medium text-text-secondary transition-colors hover:bg-zinc-200 disabled:opacity-50 dark:border-zinc-500 dark:hover:bg-zinc-600";

export const cardButtonAccent =
  "rounded-md px-2 py-0.5 text-11 font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50";

// ── Test result styles ──────────────────────────────────────────────

/** Test result message (success/error) */
export const testResultBase =
  "rounded-md px-3 py-[var(--ui-btn-py)] text-xs truncate";

// ── Chat banner slot ──────────────────────────────────────────────

/**
 * Wrapper class for inline banners rendered inside the chat messages scroll
 * container (DebugPausedBanner, RetryWaitBanner, …). The banner component is
 * expected to render THIS wrapper itself and `return null` when not visible,
 * so that no DOM element — and crucially, no `mt-1.5` margin — is left behind
 * when the banner is hidden. An always-rendered empty wrapper with `mt-1.5`
 * would silently push sibling content below the scroll viewport and trigger
 * a phantom scrollbar on empty sessions.
 */
export const bannerSlot =
  "mt-1.5 flex justify-center px-6";

/**
 * Composer chip — the small inline badge above the chat input ("replying to",
 * "addressing", "/active skill").
 *
 * One recipe on purpose: the chips are the same affordance on every chat
 * surface, and the accent may only ever survive here as a background WASH —
 * never as the border, never as the label. `chatChipChrome.test.tsx` pins that
 * rule against this token, so a call site cannot drift on its own.
 */
export const composerChip =
  "inline-flex items-center gap-1 rounded border border-border-outer bg-[var(--color-accent)]/10 px-1.5 py-0.5 text-xs font-medium text-text-tertiary";

export const testResultSuccess =
  "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400";

export const testResultError =
  "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-400";
