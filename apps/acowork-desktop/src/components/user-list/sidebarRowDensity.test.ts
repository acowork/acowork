/**
 * Agent rows and user rows share one vertical padding.
 *
 * Why this file exists:
 *   The agent sidebar and the user list are the same list language — same
 *   `row-divider-b` hairline, same `listRowHover`, same `px-3`. They each
 *   spelled their own `py-*`, so the two groups sat at different densities
 *   and moving between them felt like a different app. Both bind the same
 *   padding here; one drifting back reintroduces that split.
 *
 * Why source scanning: jsdom computes no layout, so padding is only
 * observable as the class string. Same approach as capsule.test.tsx and
 * listHeaderHeight.test.ts.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) =>
  readFileSync(resolve(__dirname, "..", "..", rel), "utf8");

const ROWS = ["components/agent-list/AgentList.tsx", "components/user-list/UserList.tsx"];

/** The padding the sidebar rows currently carry. Loosen both together, never one. */
const ROW_PADDING = "px-3 py-2";

describe("sidebar row density", () => {
  it.each(ROWS)("%s pads its rows the same way", (rel) => {
    expect(read(rel)).toContain(ROW_PADDING);
  });

  it.each(ROWS)("%s has no competing row padding left behind", (rel) => {
    // A leftover `py-1.5` row (or `py-1` / `py-3`) is the drift this pins.
    expect(read(rel)).not.toMatch(/items-center rounded-md px-3 py-(1|1\.5|3|3\.5)\b/);
  });
});
