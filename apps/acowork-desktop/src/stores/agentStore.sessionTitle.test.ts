/**
 * Title is a normal per-session config field, mapped by `sessionConfigToPatch`
 * (the single HTTP + MQTT source of truth) exactly like model / provider.
 *
 * Background for the additive semantics: a Chat-Router exec session has no
 * row in `agentStore.sessions[]` until `fetchSessions`, so the old bespoke
 * `updateSessionTitle` dropped the MQTT-pushed title ("no row → no-op") and
 * refused to refresh ("already titled → skip"), leaving the tab / status card
 * stuck on "Untitled" until the user clicked history. Routing title through
 * the mapper into `chatStore.sessionStates[sid].title` fixes both: it lands
 * without a list row (`updateSessionState` creates the session state on
 * demand), and a later authoritative push overwrites an optimistic seed.
 *
 * Title is deliberately NOT cleared on a null snapshot ("not generated yet"
 * ≠ "erase the optimistic value"), so both branches are covered here.
 */

import { describe, it, expect } from "vitest";

import { sessionConfigToPatch, deriveSessionTitle } from "../lib/sessionConfigMapper";

describe("sessionConfigToPatch — title is a per-session config field", () => {
  it("maps a non-empty title from a full snapshot (HTTP and MQTT alike)", () => {
    const patch = sessionConfigToPatch(
      { model: "m", provider: "p", title: "提交本地修改" },
      { clearOnNull: true },
    );
    expect(patch.title).toBe("提交本地修改");
  });

  it("carries the backend's refined title so the push overwrites the optimistic seed", () => {
    // The optimistic pass writes a derived truncation; the backend then
    // pushes its own (possibly LLM-refined) title through the SAME mapper.
    // Both land in the same `sessionStates[sid].title` slot, so the later
    // snapshot value replaces the earlier one.
    const optimistic = sessionConfigToPatch({ title: "提交代码…" }, { clearOnNull: true });
    const authoritative = sessionConfigToPatch(
      { title: "提交 ADR-099 修订的本地修改" },
      { clearOnNull: true },
    );
    expect(optimistic.title).toBe("提交代码…");
    expect(authoritative.title).toBe("提交 ADR-099 修订的本地修改");
  });

  it("does NOT emit a null title on a snapshot with no title yet (additive)", () => {
    // A snapshot delivered before the backend generates the title must leave
    // any optimistic title intact, i.e. no `title` key is produced — even
    // though every other field IS cleared on null (full snapshot).
    const patch = sessionConfigToPatch(
      { model: "m", provider: "p", title: null },
      { clearOnNull: true },
    );
    expect("title" in patch).toBe(false);
    expect(patch.model).toBe("m");
    expect(patch.provider).toBe("p");
  });

  it("treats the MQTT empty-string sentinel as no title", () => {
    // prost can't encode Option<T>, so "" means "absent" on the wire. The
    // MQTT handler normalizes it to null before calling the mapper; the
    // mapper must then leave the slot untouched.
    const patch = sessionConfigToPatch({ title: "" }, { clearOnNull: true });
    expect("title" in patch).toBe(false);
  });
});

describe("deriveSessionTitle — optimistic seed", () => {
  it("collapses whitespace and trims", () => {
    expect(deriveSessionTitle("  用户要求：   提交代码。\n承接结论  ")).toBe(
      "用户要求： 提交代码。 承接结论",
    );
  });

  it("caps by code points (CJK-safe), appending an ellipsis", () => {
    const long = deriveSessionTitle("字".repeat(50));
    expect(Array.from(long)).toHaveLength(25); // 24 chars + "…"
    expect(long.endsWith("…")).toBe(true);
  });

  it("returns empty for whitespace-only input", () => {
    expect(deriveSessionTitle("   \n\t ")).toBe("");
  });
});
