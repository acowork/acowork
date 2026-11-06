/**
 * The type scale shared by a Chat execution block's chrome (design 27 §5.5).
 *
 * A block and the controls that hang off it — its header pill, its round rows,
 * the in-place reply field, the action row above it — are one object, so they
 * step on ONE scale. Two copies of these `calc()`s had already started drifting
 * (the reply field was written against the detail step, its replacement row
 * against whatever the parent happened to inherit), which is how a "same size"
 * row ends up 1px taller than the thing it sits next to.
 *
 * The base is the user's `--ui-font-size`, so all of it follows the UI-scale
 * setting; the block runs at 90% of it (`ExploreBlock`'s size, so a chat run and
 * an execution run read at one size) and detail text at 80%.
 */

export const BLOCK_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.9)";
export const BLOCK_DETAIL_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.8)";
