/**
 * Agent Status 卡片里的 per-model token 占比饼图。
 *
 * 纯 SVG（`<path>` 扇形），不引任何图表库。数据来自
 * `lib/tokenStats` 的 HTTP 直读（runtime `token_stats` 派生索引），
 * 占比按 `total_input + total_output`；`unknown|unknown`（升级前的旧
 * 数据 bucket）固定灰色并置末位。
 *
 * 交互：饼图下方默认只显示"共 N 个模型"摘要行；鼠标悬停到某个扇区
 * （或对应的隐藏图例）时，同一行原位切换为该模型的详情（名称 + 占比
 * + input/output + cost）。这样无论模型多少，面板高度恒定、同一时间
 * 只展示一条详情。
 */
import { useState } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import type { TokenStatItem } from "../../lib/types";
import {
    computeTokenSegments,
    formatTokenStatCost,
    modelStatLabel,
    UNKNOWN_MODEL_KEY,
    type TokenStatSegment,
} from "../../lib/tokenStats";

interface ModelTokenDonutProps {
    items: TokenStatItem[];
    /** 派生索引校验位（Σ聚合 vs Σ明细）。`false` 时展示漂移警示。 */
    consistent: boolean;
}

// 饼图几何：viewBox 42×42，圆心 (21,21)，半径 20。
const CX = 21;
const CY = 21;
const R = 20;

/** 极坐标 → 直角坐标（0° = 12 点方向，顺时针）。 */
function polar(angleDeg: number): { x: number; y: number } {
    const rad = ((angleDeg - 90) * Math.PI) / 180;
    return { x: CX + R * Math.cos(rad), y: CY + R * Math.sin(rad) };
}

/**
 * 单个扇形的 `d` 属性。
 *
 * 当 sweep ≥ 359.99° 时退化为整圆（`<circle>` 的 path 等价写法），
 * 因为 SVG arc 命令在 start == end 时不画任何内容。
 */
function slicePath(startAngle: number, sweep: number): string {
    if (sweep >= 359.99) {
        const top = polar(0);
        const bottom = polar(180);
        return `M ${CX} ${CY} L ${top.x} ${top.y} A ${R} ${R} 0 1 1 ${bottom.x} ${bottom.y} A ${R} ${R} 0 1 1 ${top.x} ${top.y} Z`;
    }
    const endAngle = startAngle + sweep;
    const s = polar(startAngle);
    const e = polar(endAngle);
    const largeArc = sweep > 180 ? 1 : 0;
    return `M ${CX} ${CY} L ${s.x} ${s.y} A ${R} ${R} 0 ${largeArc} 1 ${e.x} ${e.y} Z`;
}

/** 展示用模型名（unknown bucket 走 i18n 键）。 */
function segmentName(
    s: TokenStatSegment,
    t: (key: string, options?: Record<string, unknown>) => string,
): string {
    return s.isUnknown && (s.provider === "unknown" || s.model === "unknown")
        ? t("rightPanel.modelTokenUnknown")
        : modelStatLabel(s.item) || s.model;
}

export function ModelTokenDonut({ items, consistent }: ModelTokenDonutProps) {
    const { t } = useTranslation();
    // 悬停扇区索引；null = 未悬停，详情行回落为"共 N 个模型"摘要。
    // Hooks must run on EVERY render — keep them above the empty-guard
    // below, or a non-empty → empty transition crashes React ("rendered
    // fewer hooks than expected").
    const [hovered, setHovered] = useState<number | null>(null);
    // 无任何统计（或全部度量为 0）时不渲染图表区——空饼比缺失更像 bug。
    const segments = computeTokenSegments(items);
    if (segments.length === 0) return null;
    const active = hovered != null ? segments[hovered] ?? null : null;

    let cursor = 0; // 角度刻度上的扇形起点（度）
    // Sweep angles use the RAW measure ratio, not the display-rounded
    // sharePct — rounding to 0.1% would leave a visible gap (3×33.3% =
    // 99.9%) and make sub-0.05% segments invisible while still in legend.
    const totalValue = segments.reduce((s, x) => s + x.value, 0);

    return (
        <div className="py-1">
            <div className="flex items-center gap-3">
                <svg
                    viewBox="0 0 42 42"
                    className="h-16 w-16 shrink-0"
                    role="img"
                    aria-label={t("rightPanel.modelTokenShare")}
                    onMouseLeave={() => setHovered(null)}
                >
                    {segments.map((s, i) => {
                        const sweep = (s.value / totalValue) * 360;
                        const start = cursor;
                        cursor += sweep;
                        // 悬停时该扇区轻微放大突出，其余保持原样。
                        const isHovered = hovered === i;
                        return (
                            <path
                                key={`${s.provider}|${s.model}`}
                                d={slicePath(start, sweep)}
                                fill={s.color}
                                stroke="var(--color-bg-card, #fff)"
                                strokeWidth="0.3"
                                opacity={hovered == null || isHovered ? 1 : 0.55}
                                onMouseEnter={() => setHovered(i)}
                                style={{ cursor: "pointer", transition: "opacity 120ms" }}
                            />
                        );
                    })}
                </svg>
                {/* 详情区：同一时间只显示一条。悬停 → 该模型详情；
                    未悬停 → "共 N 个模型"摘要。固定最小高度避免抖动。 */}
                <div className="min-w-0 flex-1 text-xs">
                    {active ? (
                        <div className="flex flex-col gap-0.5">
                            <div className="flex items-center gap-1.5">
                                <span
                                    className="inline-block h-2 w-2 shrink-0 rounded-full"
                                    style={{ backgroundColor: active.color }}
                                />
                                <span className="min-w-0 flex-1 truncate font-medium text-text-secondary">
                                    {segmentName(active, t)}
                                </span>
                                <span className="shrink-0 font-mono text-text-tertiary">
                                    {active.sharePct}%
                                </span>
                            </div>
                            <div className="flex items-center justify-between font-mono text-10 text-text-tertiary">
                                <span>
                                    {t("rightPanel.totalInputTokens")}{" "}
                                    <span className="text-text-secondary">
                                        {active.item.total_input.toLocaleString()}
                                    </span>
                                </span>
                                <span>
                                    {t("rightPanel.totalOutputTokens")}{" "}
                                    <span className="text-text-secondary">
                                        {active.item.total_output.toLocaleString()}
                                    </span>
                                </span>
                            </div>
                            <div className="flex items-center justify-between font-mono text-10 text-text-tertiary">
                                <span>
                                    {t("rightPanel.modelTokenCacheRead")}{" "}
                                    <span className="text-text-secondary">
                                        {active.item.total_cache_read.toLocaleString()}
                                    </span>
                                </span>
                                <span>
                                    {t("rightPanel.modelTokenCost")}{" "}
                                    <span className="text-text-secondary">
                                        {formatTokenStatCost(active.item.total_cost)}
                                    </span>
                                </span>
                            </div>
                        </div>
                    ) : (
                        <div className="flex items-center gap-1.5 text-text-tertiary">
                            <span className="min-w-0 flex-1 truncate">
                                {t("rightPanel.modelTokenHoverHint", {
                                    count: segments.length,
                                })}
                            </span>
                        </div>
                    )}
                </div>
            </div>
            <div className="mt-1 flex items-center justify-between text-10 text-text-tertiary">
                <span>
                    {t("rightPanel.modelTokenLegend", {
                        input: t("rightPanel.totalInputTokens"),
                        output: t("rightPanel.totalOutputTokens"),
                    })}
                </span>
                {!consistent && <span>{t("rightPanel.modelTokenDrift")}</span>}
            </div>
        </div>
    );
}

// re-export for tests / callers that key off the same bucket name
export { UNKNOWN_MODEL_KEY };
