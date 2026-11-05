/**
 * Pure helper that decides whether the agent avatar/header should be
 * rendered above a given message block.
 *
 * Rule: an agent block displays the header iff the nearest prior block in
 * display order is a user-anchored block.  Compaction and pure system
 * markers between them are skipped.  A `user_with_parts` block is
 * treated as a user-anchored block — see ADR-FIX (avatar-after-attachments).
 *
 * Extracted from VirtualMessageList.tsx so the rule can be unit-tested
 * without mounting React.
 */

import type { MessageBlock } from "./messageFolder";

/** Block types that count as a user-anchored block in the backward scan. */
const USER_ANCHOR_TYPES: ReadonlySet<MessageBlock["type"]> = new Set([
  "user",
  "user_with_parts",
]);

/**
 * Block types that are skipped during the backward scan: they are
 * "transparent" markers that don't carry an avatar themselves but also
 * don't terminate the search.  The avatar must still appear above the
 * FIRST agent block after the user, even when compaction or a pure
 * system entry sits in between.
 *
 * `explore_group` is deliberately NOT a skip type — it is itself an
 * agent block (see isAgentBlock) and already shows the avatar header
 * above it.  Treating it as a skip would let the backward scan pass
 * through it and re-trigger the avatar on the next agent block
 * (e.g. the trailing `assistant` message after `thought + tool_call +
 * tool_result`), producing a duplicate avatar in the chat UI.
 */
const SKIP_TYPES: ReadonlySet<MessageBlock["type"]> = new Set([
  "compaction",
  "system",
]);

/**
 * True if `block` is an agent reply (assistant, tool_call, tool_result,
 * thought, error, explore_group, …) — i.e. not a user/system/compaction
 * marker.  We don't enumerate agent types because new ones are added
 * over time (assistant, error, …); the rule is "anything that isn't a
 * user or a marker".
 */
export function isAgentBlock(block: MessageBlock): boolean {
  if (block.type === "explore_group") return true;
  return !(
    block.type === "user" ||
    block.type === "user_with_parts" ||
    block.type === "system" ||
    block.type === "compaction"
  );
}

/**
 * Decide whether to show the agent avatar header above `current`.
 *
 * The header is shown when:
 *  1. `current` is itself an agent block (see isAgentBlock), AND
 *  2. The most-recent non-skip block before `current` in `blocks` is a
 *     user-anchored block (`user` or `user_with_parts`).
 *
 * Skip blocks are compaction and pure system markers; user-anchored
 * blocks are user messages, including those folded with attachment system
 * entries into a `user_with_parts` block.
 *
 * The avatar anchors to the FIRST agent reply after the user — never to
 * subsequent agent blocks.  Once the avatar has been shown above the
 * first agent block in a turn (which may be an `explore_group` if the
 * agent thinks / calls tools first), every later agent block in the same
 * turn must terminate the scan.  Because `explore_group` is itself an
 * agent block, encountering it during the backward scan must break the
 * search and return false.  Treating it as a skip — as the previous
 * version of this file did — would produce a duplicate avatar on the
 * `assistant` message that follows `thought + tool_call + tool_result`.
 */
export function shouldShowAgentAvatar(
  blocks: readonly MessageBlock[],
  currentIndex: number,
): boolean {
  if (currentIndex <= 0 || currentIndex >= blocks.length) return false;
  const current = blocks[currentIndex];
  if (!isAgentBlock(current)) return false;

  for (let i = currentIndex - 1; i >= 0; i--) {
    const prev = blocks[i];
    const t = prev.type;
    if (USER_ANCHOR_TYPES.has(t)) return true;
    if (SKIP_TYPES.has(t)) continue;
    // Any other block type (e.g. another agent reply) breaks the search —
    // the header belongs above the FIRST agent reply after the user.
    return false;
  }
  return false;
}

/**
 * Decide whether to show the agent avatar header as a **trailing**
 * element after the last user block, before the agent has replied.
 *
 * This provides the "agent is about to respond" visual anchor: the
 * avatar + name + role appear immediately after the user sends a
 * message, without waiting for the first agent block to arrive.
 * When the agent reply arrives, the user block is no longer the last
 * block, so this returns false, and the existing `shouldShowAgentAvatar`
 * rule takes over on the new agent block.
 *
 * Returns true iff:
 *  1. `currentIndex` is the last index in `blocks`, AND
 *  2. `blocks[currentIndex]` is a user-anchored block.
 */
export function shouldShowTrailingAgentHeader(
  blocks: readonly MessageBlock[],
  currentIndex: number,
): boolean {
  if (currentIndex < 0 || currentIndex >= blocks.length) return false;
  if (currentIndex !== blocks.length - 1) return false;
  return USER_ANCHOR_TYPES.has(blocks[currentIndex].type);
}
