/**
 * memoryPreview.test.tsx — the memory preview must never spin on a resolved
 * but absent node.
 *
 * Background: global search's memory payload lost `node_id` when ADR-095 added
 * retrieval evidence to it (the evidence object was assigned as the *whole*
 * payload instead of merged into it). `toMemoryHits` maps a missing node_id to
 * 0, `fetchNode(agentId, 0)` resolves to `null`, and `MemoryPreview` treated
 * `!node` as "still loading" — so every memory row showed an endless spinner
 * while the list itself looked fine, because the list renders `snippet` and
 * never needed the id.
 *
 * Two independent defects, therefore two guards:
 *   1. the producer must keep locate keys — asserted in
 *      core/acowork-runtime/tests/global_search_e2e.rs;
 *   2. this file asserts the consumer degrades to readable content instead of
 *      hanging, so the next lost field is diagnosable rather than silent.
 *
 * Run: `npx vitest run src/components/search/memoryPreview.test.tsx`
 */

import { render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { PreviewPane } from "./PreviewPane";
import type { MemoryHit } from "./GlobalSearchDialog";

// One shared mock instance, created before the module factory runs, so the
// tests can drive what `fetchNode` resolves to.
const { fetchNode } = vi.hoisted(() => ({ fetchNode: vi.fn() }));

vi.mock("../../stores/memoryStore", () => ({
    useMemoryStore: (selector: (s: { fetchNode: typeof fetchNode }) => unknown) =>
        selector({ fetchNode }),
}));

const hit: MemoryHit = {
    type: "memory",
    id: "42",
    title: "Knowledge",
    snippet: "Gateway 进程负责包管理、生命周期管理和反向代理",
    sub: "Knowledge · fact",
    score: 0.016,
    nodeId: 42,
    nodeType: "Knowledge",
    dedupKey: "42:Gateway",
};

beforeEach(() => {
    fetchNode.mockReset();
});

describe("memory preview — resolved-but-absent node", () => {
    it("shows the search snippet instead of an endless spinner when the node cannot be read", async () => {
        fetchNode.mockResolvedValue(null);

        render(<PreviewPane hit={hit} tab="memory" agentId="com.test.agent" />);

        // The snippet the list already had must stay readable.
        expect(await screen.findByText(/Gateway 进程负责包管理/)).toBeTruthy();
        // And the reason must be stated, not hidden behind a bare snippet.
        expect(await screen.findByText(/未能读取该记忆节点的详情/)).toBeTruthy();
    });

    it("does not keep the spinner after the fetch settles", async () => {
        fetchNode.mockResolvedValue(null);

        const { container } = render(
            <PreviewPane hit={hit} tab="memory" agentId="com.test.agent" />,
        );

        await waitFor(() => {
            expect(container.querySelector(".animate-spin")).toBeNull();
        });
    });

    it("still renders the node when it is found", async () => {
        fetchNode.mockResolvedValue({
            node_id: 42,
            node_type: "Knowledge",
            sub_type: "fact",
            content: "Gateway 进程负责包管理",
            confidence: 0.9,
            importance: 0.7,
            decay_score: 0.7,
            created_at: 1,
            last_accessed_at: 1,
            access_count: 0,
            status: "Active",
        } as never);

        render(<PreviewPane hit={hit} tab="memory" agentId="com.test.agent" />);

        expect(await screen.findByText(/Gateway 进程负责包管理/)).toBeTruthy();
        expect(screen.queryByText(/未能读取该记忆节点的详情/)).not.toBeTruthy();
    });
});
