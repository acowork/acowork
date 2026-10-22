/**
 * agent 级 per-model token 统计的前端数据层。
 *
 * 数据链路："HTTP 拉完整数据, MQTT 推变化数据" — 统计走 HTTP 直读
 * runtime 的 `token_stats` 派生索引（经 Gateway
 * `GET /api/agents/{id}/token-stats` 透传），push 协议不携带这些；
 * `contextUsage.agent_total_*` 的值变化只当作失效信号（debounce 重拉）。
 */
import type { TokenStatItem, TokenStatsResponse } from "./types";
import { getGatewayUrl } from "./config";
import { with503Retry } from "./httpRetry";
import { log } from "./logger";

/** 拉取 agent 级 per-model 统计；失败返回 `null`（面板按"无数据"渲染，不弹错）。 */
export async function fetchAgentTokenStats(agentId: string): Promise<TokenStatsResponse | null> {
    try {
        const resp = await with503Retry(
            () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/token-stats`),
            { tag: `fetchAgentTokenStats(${agentId})`, logger: log },
        );
        if (!resp.ok) return null;
        return (await resp.json()) as TokenStatsResponse;
    } catch (e) {
        log.warn(`fetchAgentTokenStats failed: ${String(e)}`);
        return null;
    }
}

/** 旧数据 / 无归因 bucket 的固定 key（runtime 侧 `unknown|unknown`）。 */
export const UNKNOWN_MODEL_KEY = "unknown|unknown";

/** donut 分段后的展示形态（占比按 input+output，与计划一致）。 */
export interface TokenStatSegment {
    provider: string;
    model: string;
    /** 该模型的 input + output，donut 的度量值。 */
    value: number;
    /** 0-100，四舍五入到 1 位小数。 */
    sharePct: number;
    /** 固定色板取色；unknown bucket 恒为灰色。 */
    color: string;
    isUnknown: boolean;
    item: TokenStatItem;
}

/**
 * donut 色板：8 色循环，兼顾 light/dark（中饱和度、避免纯语义色冲突）。
 * `UNKNOWN_SEGMENT_COLOR` 不在色板内循环，专供 unknown bucket。
 */
export const DONUT_PALETTE = [
    "#6366f1", // indigo
    "#0ea5e9", // sky
    "#10b981", // emerald
    "#f59e0b", // amber
    "#ec4899", // pink
    "#8b5cf6", // violet
    "#14b8a6", // teal
    "#f97316", // orange
] as const;

export const UNKNOWN_SEGMENT_COLOR = "#9ca3af"; // gray-400

/**
 * 把聚合行折叠成 donut 分段：
 * - 占比与排序度量 = `total_input + total_output`；
 * - `unknown|unknown` 不参与模型名展示（i18n 由组件负责），固定灰色并
 *   **恒置末位**（哪怕占比更大），避免旧数据抢走"模型占比"的叙事；
 * - 其余按度量降序，色板按序循环取色；
 * - 总度量 <= 0 时返回空数组（组件据此不渲染图表区）。
 */
export function computeTokenSegments(items: TokenStatItem[]): TokenStatSegment[] {
    const measure = (i: TokenStatItem) => i.total_input + i.total_output;
    const known = items.filter((i) => modelUsageKey(i) !== UNKNOWN_MODEL_KEY);
    const unknown = items.filter((i) => modelUsageKey(i) === UNKNOWN_MODEL_KEY);
    known.sort((a, b) => measure(b) - measure(a) || a.model.localeCompare(b.model));
    const total = items.reduce((s, i) => s + measure(i), 0);
    if (total <= 0) return [];
    const seg = (item: TokenStatItem, idx: number, isUnknown: boolean): TokenStatSegment => ({
        provider: item.provider,
        model: item.model,
        value: measure(item),
        sharePct: Math.round((measure(item) / total) * 1000) / 10,
        color: isUnknown ? UNKNOWN_SEGMENT_COLOR : DONUT_PALETTE[idx % DONUT_PALETTE.length],
        isUnknown,
        item,
    });
    return [
        ...known.map((i, idx) => seg(i, idx, false)),
        ...unknown.map((i) => seg(i, 0, true)),
    ];
}

/** runtime 侧明细 map 的 key 约定（provider 缺失也是 "unknown"）。 */
export function modelUsageKey(item: Pick<TokenStatItem, "provider" | "model">): string {
    return `${item.provider}|${item.model}`;
}

/** 展示用模型标签：provider/model 拼接，unknown bucket 交给 i18n 键。 */
export function modelStatLabel(item: TokenStatItem): string {
    if (modelUsageKey(item) === UNKNOWN_MODEL_KEY) return "";
    return item.provider && item.provider !== "unknown"
        ? `${item.provider}/${item.model}`
        : item.model;
}

/** 成本展示："—" 而非 $0（miss 不估计，与 runtime 侧同规则）。 */
export function formatTokenStatCost(cost: number | null | undefined): string {
    if (cost == null) return "—";
    return `$${cost.toFixed(2)}`;
}

/** agent 级合计（从 token_stats 明细求和），不受 visibility 过滤。 */
export interface AgentTokenTotals {
    totalInput: number;
    totalOutput: number;
    totalCacheRead: number;
    totalCacheWrite: number;
    totalCost: number | null;
}

/**
 * 从 per-model 聚合行计算 agent 级四项合计。
 *
 * 这是 "per-agent 统计单独处理、不受 visible 限制" 的前端实现：
 * 数据源是 runtime 的 `token_stats` 表（agent 级、无 user 过滤），
 * 而非 MQTT push 的 `agent_total_*`（受 scan_sessions_async 可见性
 * 过滤 + atomic-MAX merge 影响，口径与 token_stats 不一致）。
 */
export function computeAgentTotals(items: TokenStatItem[]): AgentTokenTotals {
    let totalInput = 0;
    let totalOutput = 0;
    let totalCacheRead = 0;
    let totalCacheWrite = 0;
    let totalCost: number | null = 0;
    for (const item of items) {
        totalInput += item.total_input;
        totalOutput += item.total_output;
        totalCacheRead += item.total_cache_read;
        totalCacheWrite += item.total_cache_write;
        if (item.total_cost == null) {
            totalCost = null;
        } else if (totalCost !== null) {
            totalCost += item.total_cost;
        }
    }
    return { totalInput, totalOutput, totalCacheRead, totalCacheWrite, totalCost };
}
