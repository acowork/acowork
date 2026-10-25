/**
 * `lib/tokenStats` 纯函数单测：donut 占比计算、unknown bucket 的
 * 渲染位次/取色、色板循环、成本 "—" 语义。
 */
import { describe, expect, it } from "vitest";
import type { TokenStatItem } from "./types";
import {
    computeAgentTotals,
    computeTokenSegments,
    DONUT_PALETTE,
    formatTokenStatCost,
    modelStatLabel,
    UNKNOWN_MODEL_KEY,
    UNKNOWN_SEGMENT_COLOR,
} from "./tokenStats";

function item(
    provider: string,
    model: string,
    input: number,
    output: number,
    cost: number | null = null,
    cacheRead: number = 0,
    cacheWrite: number = 0,
): TokenStatItem {
    return {
        provider,
        model,
        total_input: input,
        total_output: output,
        total_cache_read: cacheRead,
        total_cache_write: cacheWrite,
        total_cost: cost,
    };
}

describe("computeTokenSegments", () => {
    it("computes share by input+output, rounded to one decimal", () => {
        const segs = computeTokenSegments([
            item("openai", "gpt-a", 750, 0),
            item("anthropic", "claude-b", 250, 0),
        ]);
        expect(segs.map((s) => s.sharePct)).toEqual([75, 25]);
        expect(segs[0].value).toBe(750);
        // 33.33…% → 1 位小数
        const thirds = computeTokenSegments([
            item("p", "a", 100, 0),
            item("p", "b", 100, 0),
            item("p", "c", 100, 0),
        ]);
        expect(thirds.map((s) => s.sharePct)).toEqual([33.3, 33.3, 33.3]);
    });

    it("sorts known models by measure desc and keeps unknown|unknown last with fixed gray", () => {
        // unknown 占比最大（6000）也必须置末位、取灰色，不占色板首色。
        const segs = computeTokenSegments([
            item("unknown", "unknown", 6000, 0),
            item("openai", "gpt-a", 300, 0),
            item("anthropic", "claude-b", 100, 0),
        ]);
        expect(segs.map((s) => `${s.provider}/${s.model}`)).toEqual([
            "openai/gpt-a",
            "anthropic/claude-b",
            "unknown/unknown",
        ]);
        const unknown = segs[segs.length - 1];
        expect(unknown.isUnknown).toBe(true);
        expect(unknown.color).toBe(UNKNOWN_SEGMENT_COLOR);
        expect(segs[0].color).toBe(DONUT_PALETTE[0]);
    });

    it("cycles the fixed palette across more models than colors", () => {
        const many = Array.from({ length: DONUT_PALETTE.length + 2 }, (_, i) =>
            item("p", `m${i}`, 100, 0),
        );
        const segs = computeTokenSegments(many);
        expect(segs[DONUT_PALETTE.length].color).toBe(DONUT_PALETTE[0]);
        expect(segs.every((s) => !s.isUnknown)).toBe(true);
    });

    it("returns empty for no items or an all-zero aggregate", () => {
        expect(computeTokenSegments([])).toEqual([]);
        expect(computeTokenSegments([item("p", "m", 0, 0)])).toEqual([]);
    });

    it("keeps an unknown-only aggregate renderable (legacy data still shows)", () => {
        const segs = computeTokenSegments([item("unknown", "unknown", 500, 500)]);
        expect(segs).toHaveLength(1);
        expect(segs[0].sharePct).toBe(100);
        expect(segs[0].color).toBe(UNKNOWN_SEGMENT_COLOR);
    });
});

describe("labels and cost formatting", () => {
    it("labels provider/model, bare model for unknown provider, empty for the legacy bucket", () => {
        expect(modelStatLabel(item("openai", "gpt-a", 1, 1))).toBe("openai/gpt-a");
        expect(modelStatLabel(item("unknown", "gpt-a", 1, 1))).toBe("gpt-a");
        expect(modelStatLabel(item("unknown", "unknown", 1, 1))).toBe("");
        expect(UNKNOWN_MODEL_KEY).toBe("unknown|unknown");
    });

    it("renders null cost as an em dash, never $0", () => {
        expect(formatTokenStatCost(null)).toBe("—");
        expect(formatTokenStatCost(undefined)).toBe("—");
        expect(formatTokenStatCost(0.05)).toBe("$0.05");
    });

    it("computeAgentTotals sums four fields and poisons cost on any null", () => {
        const items = [
            item("openai", "gpt", 100, 10, 0.5, 5, 0),
            item("anthropic", "claude", 200, 20, 1.5, 10, 2),
        ];
        const totals = computeAgentTotals(items);
        expect(totals.totalInput).toBe(300);
        expect(totals.totalOutput).toBe(30);
        expect(totals.totalCacheRead).toBe(15);
        expect(totals.totalCacheWrite).toBe(2);
        expect(totals.totalCost).toBeCloseTo(2.0);

        // A single null cost poisons the total.
        const withNull = [...items, item("unknown", "unknown", 50, 5, null, 0, 0)];
        expect(computeAgentTotals(withNull).totalCost).toBeNull();

        // Empty items → all zeros, cost 0 (not null).
        const empty = computeAgentTotals([]);
        expect(empty.totalInput).toBe(0);
        expect(empty.totalCost).toBe(0);
    });
});
