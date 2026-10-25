/**
 * GlobalSearchDialog.test.ts — P0-1 runnable checks (ADR-081).
 *
 * Covers the non-trivial pure pieces so a regression breaks a test instead
 * of shipping:
 *   1. `enterShouldSearch` — Enter means "search the box" or "open the
 *      focused row"; getting this wrong either re-runs per keystroke or
 *      makes Enter stop opening results.
 *   2. `filterGitCommits` — CommitPicker-compatible client-side filter
 *      (subject/author/hash, case-insensitive).
 *   3. `toFileHits` — ripgrep match → SearchHit mapping (file tab rows).
 *
 * Run: `npx vitest run src/components/search/GlobalSearchDialog.test.ts`
 */

import { describe, expect, it, vi } from "vitest";
import {
    enterShouldSearch,
    filterGitCommits,
    toFileHits,
    toMemoryHits,
    toConversationHits,
    pickFirstResultTab,
    formatTokenCount,
    memorySearchUrl,
    visibleTabOrder,
    TAB_ORDER,
} from "./GlobalSearchDialog";

// Importing the module pulls in zustand stores (fine in test env); the
// presentational asset modules are unused by these pure helpers.
vi.mock("../common/SetiIcon", () => ({ SetiIcon: () => null }));
vi.mock("../workspace/FileTree/fileIcons", () => ({ getFileIcon: () => null }));

const COMMITS = [
    {
        hash: "a1b2c3d4e5f6",
        shortHash: "a1b2c3d",
        author: "Alice",
        date: "2025-01-01",
        subject: "feat: add global search dialog",
    },
    {
        hash: "f6e5d4c3b2a1",
        shortHash: "f6e5d4c",
        author: "Bob",
        date: "2025-01-02",
        subject: "fix: sidebar overflow",
    },
];

describe("enterShouldSearch", () => {
    it("searches when the typed query differs from the last run", () => {
        expect(enterShouldSearch("migration", "")).toBe(true);
        expect(enterShouldSearch("migration", "migra")).toBe(true);
    });

    it("opens the row when the query already matches what was searched", () => {
        expect(enterShouldSearch("migration", "migration")).toBe(false);
    });

    it("never searches on an empty or whitespace-only box", () => {
        expect(enterShouldSearch("", "")).toBe(false);
        expect(enterShouldSearch("   ", "migration")).toBe(false);
    });

    it("ignores surrounding whitespace so a trailing space is not an edit", () => {
        expect(enterShouldSearch("  migration  ", "migration")).toBe(false);
    });
});

describe("filterGitCommits", () => {
    it("returns all commits for an empty query", () => {
        expect(filterGitCommits(COMMITS, "")).toHaveLength(2);
        expect(filterGitCommits(COMMITS, "   ")).toHaveLength(2);
    });

    it("filters by subject substring (case-insensitive)", () => {
        expect(filterGitCommits(COMMITS, "SEARCH")).toHaveLength(1);
        expect(filterGitCommits(COMMITS, "search")[0].shortHash).toBe("a1b2c3d");
    });

    it("filters by author substring", () => {
        expect(filterGitCommits(COMMITS, "bob")).toHaveLength(1);
        expect(filterGitCommits(COMMITS, "alice")[0].shortHash).toBe("a1b2c3d");
    });

    it("filters by short hash prefix", () => {
        expect(filterGitCommits(COMMITS, "a1b")).toHaveLength(1);
        expect(filterGitCommits(COMMITS, "f6e")).toHaveLength(1);
    });

    it("returns empty when nothing matches", () => {
        expect(filterGitCommits(COMMITS, "zzz")).toHaveLength(0);
    });
});

describe("toFileHits", () => {
    it("maps ripgrep matches to SearchHit rows with locate meta", () => {
        const hits = toFileHits("agent-1", "ws-1", [
            { file: "src/foo.ts", line: 42, column: 5, text: "const x = 1" },
            { file: "README.md", line: 1, column: 0, text: "# Hello" },
        ]);
        expect(hits).toHaveLength(2);
        expect(hits[0]).toMatchObject({
            type: "file",
            id: "src/foo.ts:42",
            title: "foo.ts",
            snippet: "const x = 1",
            sub: "src/foo.ts",
            agentId: "agent-1",
            workspaceId: "ws-1",
            path: "src/foo.ts",
            line: 42,
        });
        expect(hits[1].title).toBe("README.md");
    });
});

describe("toMemoryHits", () => {
    it("maps Runtime /search memory hits to rows carrying node_id", () => {
        const hits = toMemoryHits([
            {
                scope: "memory",
                title: "Knowledge",
                snippet: "Rust borrow checker",
                score: 0.92,
                payload: { node_id: 7, node_type: "Knowledge", sub_type: "Fact" },
            },
            {
                scope: "memory",
                title: "Episodic",
                snippet: "user prefers dark mode",
                score: 0.81,
                payload: { node_id: 8, node_type: "Episodic", sub_type: null },
            },
        ]);
        expect(hits).toHaveLength(2);
        expect(hits[0]).toMatchObject({
            type: "memory",
            id: "7",
            title: "Knowledge",
            snippet: "Rust borrow checker",
            nodeId: 7,
            nodeType: "Knowledge",
        });
        // sub_type surfaced in the secondary line only when present.
        expect(hits[0].sub).toContain("Fact");
        expect(hits[1].sub).toBe("Episodic");
    });
});

describe("toConversationHits", () => {
    it("maps Runtime /search conversation hits to rows carrying session_id + message_index", () => {
        const hits = toConversationHits([
            {
                scope: "conversation",
                title: "s-abc-123",
                snippet: "borrow checker rules",
                score: 0.9,
                payload: { session_id: "s-abc-123", message_index: 42, role: "assistant" },
            },
            {
                scope: "conversation",
                title: "s-abc-123",
                snippet: "lifetime elision",
                score: 0.8,
                payload: { session_id: "s-abc-123", message_index: 41, role: "user" },
            },
            // payload without session_id is not a conversation hit — drop it.
            { scope: "memory", title: "Knowledge", snippet: "x", score: 0.1, payload: {} },
        ]);
        expect(hits).toHaveLength(2);
        expect(hits[0]).toMatchObject({
            type: "conversation",
            id: "s-abc-123:42",
            title: "s-abc-123",
            snippet: "borrow checker rules",
            sessionId: "s-abc-123",
            messageIndex: 42,
            role: "assistant",
        });
        expect(hits[0].sub).toContain("assistant");
        expect(hits[1].sub).toContain("user");
    });
});

describe("pickFirstResultTab", () => {
    it("returns the first non-empty tab in canonical TAB_ORDER", () => {
        // file=0, doc>0 → doc wins, even though git (between them) is also empty.
        expect(
            pickFirstResultTab(
                { file: 0, git: 0, doc: 3, project: 1, memory: 0, conversation: 0 },
                TAB_ORDER,
            ),
        ).toBe("doc");
    });

    it("picks file first when file has hits — preserves default-tab intent", () => {
        expect(
            pickFirstResultTab(
                { file: 5, git: 0, doc: 0, project: 0, memory: 0, conversation: 0 },
                TAB_ORDER,
            ),
        ).toBe("file");
    });

    it("returns null when every tab is empty — caller stays put", () => {
        expect(
            pickFirstResultTab(
                { file: 0, git: 0, doc: 0, project: 0, memory: 0, conversation: 0 },
                TAB_ORDER,
            ),
        ).toBeNull();
    });

    it("treats missing keys as zero (partial counts map)", () => {
        // Caller might pass only the keys it knows about — the helper must
        // not crash and must still honour the documented TAB_ORDER priority.
        expect(pickFirstResultTab({ doc: 2 }, TAB_ORDER)).toBe("doc");
        expect(pickFirstResultTab({}, TAB_ORDER)).toBeNull();
    });

    it("ignores zero-valued keys and picks the first strictly-positive one", () => {
        expect(
            pickFirstResultTab(
                { file: 0, git: 0, doc: 0, project: 0, memory: 0, conversation: 1 },
                TAB_ORDER,
            ),
        ).toBe("conversation");
    });
});

describe("formatTokenCount", () => {
    it("returns null for undefined / non-finite / negative inputs", () => {
        expect(formatTokenCount(undefined)).toBeNull();
        expect(formatTokenCount(NaN)).toBeNull();
        expect(formatTokenCount(Infinity)).toBeNull();
        expect(formatTokenCount(-5)).toBeNull();
    });

    it("renders integers below 1000 verbatim", () => {
        expect(formatTokenCount(0)).toBe("0");
        expect(formatTokenCount(42)).toBe("42");
        expect(formatTokenCount(999)).toBe("999");
    });

    it("renders ≥1000 with one-decimal k suffix", () => {
        expect(formatTokenCount(1000)).toBe("1.0k");
        expect(formatTokenCount(1234)).toBe("1.2k");
        expect(formatTokenCount(12_500)).toBe("12.5k");
        expect(formatTokenCount(1_290_000)).toBe("1290.0k");
    });

    it("truncates non-integer values so the formatter never lies about precision", () => {
        expect(formatTokenCount(1234.9)).toBe("1.2k");
        expect(formatTokenCount(7.4)).toBe("7");
    });
});

/* ─── ADR-101: the memory scope talks to the standalone service ─────── */

describe("visibleTabOrder", () => {
    it("keeps all six tabs while the memory service is ready", () => {
        expect(visibleTabOrder({ memoryServiceReady: true })).toEqual([...TAB_ORDER]);
    });

    it("drops only the memory tab when it is not", () => {
        const tabs = visibleTabOrder({ memoryServiceReady: false });
        expect(tabs).not.toContain("memory");
        expect(tabs).toEqual(TAB_ORDER.filter((t) => t !== "memory"));
    });

    it("keeps the auto-switch off a memory-only result set when the tab is hidden", () => {
        const counts = { memory: 5 };
        // `memory` is last in TAB_ORDER, so it only wins when nothing else
        // landed — exactly the case where hiding it must mean "stay put",
        // never "switch to a tab that is not in the strip".
        expect(pickFirstResultTab(counts, TAB_ORDER)).toBe("memory");
        expect(pickFirstResultTab(counts, visibleTabOrder({ memoryServiceReady: false }))).toBeNull();
    });

    it("lets the auto-switch fall through to the next reachable tab", () => {
        const counts = { memory: 5, file: 2 };
        expect(pickFirstResultTab(counts, visibleTabOrder({ memoryServiceReady: false }))).toBe("file");
    });
});

describe("memorySearchUrl", () => {
    it("targets the Gateway's flat memory proxy, not the agent runtime", () => {
        const url = new URL(memorySearchUrl("http://gw.test", "inst-1", "hello", 20));
        expect(url.origin).toBe("http://gw.test");
        expect(url.pathname).toBe("/api/memory/search");
    });

    it("carries instance_id plus the aggregate defaults", () => {
        const url = new URL(memorySearchUrl("http://gw.test", "6f9d0c2e-1234-4abc-8def-0123456789ab", "hello world", 20));
        expect(url.searchParams.get("instance_id")).toBe("6f9d0c2e-1234-4abc-8def-0123456789ab");
        expect(url.searchParams.get("q")).toBe("hello world");
        expect(url.searchParams.get("mode")).toBe("hybrid");
        expect(url.searchParams.get("limit")).toBe("20");
    });

    it("encodes the query so a sigil in the text cannot rewrite the params", () => {
        const url = new URL(memorySearchUrl("http://gw.test", "inst-1", "a?b&c=d", 5));
        expect(url.searchParams.get("q")).toBe("a?b&c=d");
        expect(url.searchParams.get("limit")).toBe("5");
    });
});

