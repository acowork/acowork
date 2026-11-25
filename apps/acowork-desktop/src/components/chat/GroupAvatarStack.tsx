/**
 * GroupAvatarStack — the WeChat-style composite avatar for a project group
 * chat row in the sidebar.
 *
 * A group has no single face, so the row shows its members tiled into a
 * 40×40 square — the same footprint as the 40px agent / user avatar above and
 * below it (which is what makes the three lists read as one), but deliberately
 * **square**: those single avatars are circles, and a composite stays a
 * rounded square the way a group icon does in every messenger. The tile layout
 * follows WeChat's group-icon rules:
 *
 * - ≤ 4 members  → 2×2 four-grid
 * - 5 – 6 members → 2 rows (3 columns)
 * - > 6 members  → 3×3 nine-grid (the first 9 are shown)
 *
 * Cells past the member count stay as light-gray placeholder blocks, so the
 * composite is always a full square (the shape people recognise as "a group")
 * rather than a ragged cluster.
 *
 * Members are resolved from the group session's `participants` (already in
 * the list payload) through the same PM member resolver the group's
 * right-hand panel uses, so an agent participant shows its agent avatar and
 * a human shows their account avatar.
 */

import { useMemo } from "react";
import { cn } from "../../lib/utils";
import { MemberAvatar, useResolvedMembers } from "../../views/pm/pmMembers";
import type { PmProjectMember } from "../../lib/pm-types";

/** A session participant as carried on `ChatSessionInfo.participants`. */
interface ParticipantRef {
  id: string;
  identity?: string;
}

/** Grid geometry for a member count: columns, total cells, and the pixel
 *  metrics that make the tiles fill a 40×40 box the way WeChat's does.
 *  Exported as a pure function so the tier boundaries (≤4 / 5–6 / >6) are
 *  pinned by a unit test rather than only by eyeballing the sidebar. */
export function gridFor(count: number): { cols: number; cells: number; tile: number; gap: number; pad: number } {
  if (count <= 4) return { cols: 2, cells: 4, tile: 16, gap: 2, pad: 3 };
  if (count <= 6) return { cols: 3, cells: 6, tile: 11, gap: 1, pad: 2 };
  return { cols: 3, cells: 9, tile: 11, gap: 1, pad: 2 };
}

export function GroupAvatarStack({
  participants,
  className,
}: {
  participants: ParticipantRef[] | undefined;
  className?: string;
}) {
  // `identity === "human"` → resolve against the account directory; anything
  // else (including legacy untyped rows, which default to the agent side) →
  // resolve against `agentStore`. Same classification the group chat panel
  // uses for its participant namer.
  const members = useMemo<PmProjectMember[]>(
    () =>
      (participants ?? []).map((p) => ({
        instance_id: p.id,
        kind: p.identity === "human" ? "user" : "agent",
        added_at: "",
      })),
    [participants],
  );
  const resolved = useResolvedMembers(members);

  const { cols, cells, tile, gap, pad } = gridFor(resolved.length);

  return (
    <div
      className={cn(
        // Footprint mirrors the 40px agent / user avatar so the composite
        // lines up with the rows above and below it; the radius stays square
        // on purpose (a group is not a face — the single avatars are circles).
        "flex h-10 w-10 shrink-0 items-center justify-center overflow-hidden rounded-lg",
        "bg-surface ring-1 ring-border-strong/60",
        className,
      )}
      style={{ padding: pad }}
      aria-hidden
    >
      <div
        className="grid"
        style={{ gridTemplateColumns: `repeat(${cols}, ${tile}px)`, gap }}
      >
        {Array.from({ length: cells }).map((_, i) => {
          const member = resolved[i];
          if (!member) {
            // Empty cell → light-gray placeholder block.
            return (
              <div
                key={i}
                className="rounded-[2px] bg-surface-hover"
                style={{ width: tile, height: tile }}
              />
            );
          }
          return (
            <MemberAvatar
              key={i}
              member={member}
              size={tile}
              // Square the tiny tiles (they are shards of one square group
              // icon; a circle at 11–16px would also read as a dot) and drop
              // their per-tile ring — the gap and the container background
              // already separate them.
              className="!rounded-[2px] !ring-0"
            />
          );
        })}
      </div>
    </div>
  );
}
