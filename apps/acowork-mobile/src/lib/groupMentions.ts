/**
 * Group mention resolution — the mobile twin of the pure functions inside
 * Desktop's `GroupChatPanel`. The client resolves `@Name` text against the
 * session's roster; the Gateway never parses free text (design 27 §5.3).
 * Keeping these as free functions (not panel-local) is what lets the inbox,
 * the screen and the tests share ONE rule.
 *
 * The matching is deliberately WeChat-style: longest name wins, and a name
 * inside another word still counts. Cheap and predictable beats clever.
 */

import type { ChatMessageRow } from './types'
import type { ChatAddress, ChatRefPick } from '../stores/chatSessionStore'

/** What the mention picker offers. `isAgent` splits the two fan-out rules:
 *  @-ing an Agent wakes it (it is a delivery target), @-ing a person is a
 *  mention that triggers nothing — but both are pickable (§5.3.1). */
export interface MentionCandidate {
  id: string
  name: string
  isAgent: boolean
}

/** Resolve `@<agent name>` tokens against the session's agent participants.
 *  Only agents become delivery targets; a human `@name` stays text.
 *
 *  Longest name wins: `@小助手Pro` must wake the Pro agent ALONE — a shorter
 *  name that is a prefix of a matched longer one is suppressed (the same
 *  containment rule Desktop's GroupChatPanel applies). Without it, mentioning
 *  one agent would silently wake its prefix-twin. */
export function resolveMentions(
  text: string,
  agents: Array<{ id: string; name: string }>,
): string[] {
  const ids: string[] = []
  // Blank out each matched `@name` span before testing shorter names, so a
  // prefix of an already-consumed mention cannot match again.
  let rest = text
  const sorted = [...agents].sort((a, b) => b.name.length - a.name.length)
  for (const a of sorted) {
    if (!a.name) continue
    const token = `@${a.name}`
    if (rest.includes(token)) {
      ids.push(a.id)
      rest = rest.split(token).join(' '.repeat(token.length))
    }
  }
  return [...new Set(ids)]
}

/** Active `@`-mention token under the caret, for the autocomplete picker.
 *  `start` is the index of the `@`; `query` is the (whitespace-free) text
 *  typed after it. The caret must sit inside the token, and the `@` must
 *  open a word (start of text or after whitespace) so `a@b` never triggers. */
export function detectMention(
  value: string,
  caret: number,
): { query: string; start: number } | null {
  const before = value.slice(0, caret)
  const at = before.lastIndexOf('@')
  if (at < 0) return null
  const token = before.slice(at + 1)
  // Whitespace ends the query — "@foo bar" closes the picker.
  if (/\s/.test(token)) return null
  if (at > 0 && !/\s/.test(before[at - 1]!)) return null
  return { query: token, start: at }
}

/** Case-insensitive substring filter on the display name (a name inside
 *  another word still matches — same rule as `resolveMentions`). Generic so
 *  the caller's extra fields survive the filter. */
export function filterMentionCandidates<T extends { id: string; name: string }>(
  candidates: T[],
  query: string,
): T[] {
  const q = query.toLowerCase()
  if (!q) return candidates
  return candidates.filter((c) => c.name.toLowerCase().includes(q))
}

/** Splice `@name ` over the active mention token and report the new caret
 *  position (just past the inserted text and one trailing space). */
export function applyMention(
  value: string,
  caret: number,
  mentionStart: number,
  name: string,
): { value: string; caret: number } {
  const inserted = `@${name} `
  const next = value.slice(0, mentionStart) + inserted + value.slice(caret)
  return { value: next, caret: mentionStart + inserted.length }
}

/** The composer's per-target address pick (the second level of the `@`,
 *  redrawn by the 2026-11-04 revision: typed picks are MATERIAL, not
 *  delivery). Mirrors Desktop's `AddressPick`. */
export interface GroupAddressPick {
  refs?: ChatRefPick[]
  forceNew?: boolean
  labels?: Record<string, string>
}

/**
 * Turn the composer's address picks into the wire address (ADR-106 D6/D7).
 *
 * An address rides ONLY the targets this message actually mentions: a chip
 * whose `@` the human later deleted must never reach the body. `undefined`
 * (nothing chosen, or nothing still mentioned) is not an empty object — the
 * absence of an address is what keeps the turn on the ladder's lower levels.
 */
export function buildChatAddress(
  targets: string[],
  addresses: Record<string, GroupAddressPick>,
): ChatAddress | undefined {
  const refs: ChatRefPick[] = []
  const forceNewFor: string[] = []
  for (const target of targets) {
    const pick = addresses[target]
    if (!pick) continue
    if (pick.refs?.length) refs.push(...pick.refs)
    else if (pick.forceNew) forceNewFor.push(target)
  }
  if (refs.length === 0 && forceNewFor.length === 0) return undefined
  return {
    ...(refs.length > 0 ? { refs } : {}),
    ...(forceNewFor.length > 0 ? { forceNewFor } : {}),
  }
}

/**
 * Who a row is addressed to (the `→` chips under a bubble) — the twin of
 * Desktop's `rowToTargets`.
 *
 * A human row names its targets in `meta.mentions`; an agent message names
 * the initiator it answers in `to`; an execution-session echo row carries
 * neither and resolves through the sender of the row it answers
 * (`triggerFrom`, passed by the caller — `to` is empty on that write path).
 * Empty = the row carries no address and no chip is painted.
 */
export function rowToTargets(row: ChatMessageRow, triggerFrom?: string): string[] {
  if (row.to) return [row.to]
  if (row.kind === 'user_message') return row.meta?.mentions ?? []
  if (triggerFrom) return [triggerFrom]
  return []
}
