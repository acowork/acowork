/**
 * ExtensionsView — 扩展面板（VSCode Extensions 风格）的两栏胶囊布局。
 *
 * 形状统一走全局胶囊布局（CAPSULE_PANE_CN，见 components/common/capsule.ts）：
 * 左栏搜索 + 列表胶囊（bg-nav-surface），右栏详情胶囊（bg-page-bg），
 * 与 ProjectSidebar / DocTreeSidebar + ProjectBoard / DocEditor 同一套
 * 圆角 + 发丝边框 + 底色分工。视图容器保持透明，让 AppLayout 的窗口毛玻璃透出。
 *
 * 底色分工（别抄错，这是本文件最容易错的一处）：本视图是 "列表 + 详情"
 * 两栏，对标 pm / doc，不是 chat 的 "正文 + 右侧检视器"。
 *   - 左栏 list     → bg-nav-surface（同 ProjectSidebar / DocTreeSidebar）
 *   - 右栏 详情主区 → bg-page-bg（同 ProjectBoard / DocEditor）
 * chat 的 `bg-right-panel` 是那个 6-tab 侧栏 / 右侧检视器专用的、比 page-bg
 * 深一档的 raised surface，套到这里会让详情栏比左栏还深、层级读反。
 *
 * 数据尚未接入：`core/acowork-gateway/src/http/extensions.rs` 还在计划中，
 * desktop 侧没有 store / API，所以 EXTENSIONS 恒为空数组。空列表区域保留
 * 滚动根（`min-h-0 flex-1 overflow-y-auto`）而不是塞假条目 —— 假数据会让
 * "哪些是真的" 变得不可验证，等真数据接上时这些占位直接被覆盖即可。
 *
 * TODO(extensions): 真正接入 extension 数据时统一补 `extensions` 命名空间
 * （搜索占位符 / 无结果文案 / 空态），5 个语言包同步。
 * TODO(i18n): 下面的 `ariaLabel` / 搜索占位符 / 空态文案是硬编码英文。
 * 5 个语言包都没有 `extensions` 命名空间，且改动前本视图就是硬编码英文（没走
 * i18n），所以本票维持现状；接入数据时与上面那条一起做。**不要**借用
 * `pm.searchPlaceholder` 这类 pm 命名空间的 key —— 语义不对。
 */
import { useState } from "react";
import { Search } from "lucide-react";
import { CAPSULE_PANE_CN } from "../components/common/capsule";
import { SplitHandle } from "../components/common/SplitHandle";
import { StyledInput } from "../components/common/StyledInput";
import { useDragResize } from "../hooks/useDragResize";
import { cn } from "../lib/utils";
import { listRowHover } from "../lib/ui-styles";

interface Extension {
    id: string;
    name: string;
}

/** 恒空：Gateway 扩展清单端点尚未落地。接入时替换为 store 的列表。 */
const EXTENSIONS: Extension[] = [];

const ariaLabel = {
    search: "Search extensions",
    list: "Extensions list",
    detail: "Extension details",
    resizeSidebar: "Resize extensions sidebar",
};

export function ExtensionsView() {
    const [query, setQuery] = useState("");
    const [selectedId, setSelectedId] = useState<string | null>(null);

    // 与 pm / docs 左侧栏一致的可拖动宽度（localStorage 持久化）
    const sidebar = useDragResize({
        storageKey: "acowork-extensions-list-width",
        defaultWidth: 240,
        minWidth: 160,
        maxWidth: 400,
    });

    const visible = EXTENSIONS.filter((e) =>
        e.name.toLowerCase().includes(query.trim().toLowerCase()),
    );

    // 上下键在可见项之间移动选中（标准 listbox 模式）；列表为空时无事发生。
    const moveSelection = (key: string) => {
        if (visible.length === 0) return;
        const at = visible.findIndex((e) => e.id === selectedId);
        const next = key === "ArrowDown"
            ? Math.min(at + 1, visible.length - 1)
            : Math.max(at - 1, 0);
        setSelectedId(visible[next]?.id ?? visible[0].id);
    };

    return (
        // `w-full` matters for the same reason as in DocsView: as an
        // AppLayout flex item this root would otherwise size itself to its
        // content and leave bare vibrancy to the right of the capsule.
        <div className="flex h-full min-h-0 w-full flex-1">
            {/* 左栏：搜索吸顶 + 列表独立滚动（滚动不带搜索框） */}
            <aside
                className={cn(CAPSULE_PANE_CN, "h-full shrink-0 bg-nav-surface text-xs")}
                style={{ width: sidebar.width }}
            >
                {/* `border-b` matches the other sidebar headers (SectionPane,
                    DocTreeSidebar, ProjectSidebar) so the search band is
                    separated from the list the same way everywhere. */}
                <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center border-b border-border-divider px-3">
                    <div className="relative min-w-0 flex-1">
                        <Search
                            className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-text-tertiary"
                            aria-hidden="true"
                        />
                        <StyledInput
                            type="text"
                            value={query}
                            onChange={(e) => setQuery(e.target.value)}
                            placeholder={ariaLabel.search}
                            aria-label={ariaLabel.search}
                            className="rounded-md bg-input-bg pl-7 py-1.5 pr-2"
                        />
                    </div>
                </div>

                <div
                    role="listbox"
                    aria-label={ariaLabel.list}
                    aria-controls="extensions-detail"
                    tabIndex={-1}
                    onKeyDown={(e) => {
                        if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
                        e.preventDefault();
                        moveSelection(e.key);
                    }}
                    className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden px-1.5"
                >
                    {visible.map((e, index) => {
                        const active = e.id === selectedId;
                        const isLast = index === visible.length - 1;
                        return (
                            <button
                                key={e.id}
                                type="button"
                                role="option"
                                aria-selected={active}
                                onClick={() => setSelectedId(e.id)}
                                className={cn(
                                    "relative flex w-full items-center gap-2 rounded-md px-3 py-2.5 text-left transition-colors duration-150",
                                    active
                                        ? "bg-accent-strong/90 text-white"
                                        : cn("text-text-secondary ", listRowHover),
                                    !isLast &&
                                        "row-divider-b",
                                )}
                            >
                                <span className={cn("min-w-0 flex-1 truncate", active && "text-white")}>
                                    {e.name}
                                </span>
                            </button>
                        );
                    })}

                    {/* 列表恒空时的占位：胶囊内的空白滚动根，不是裸 div。
                        接入真实数据后这里换成 "无匹配" / 空态文案。 */}
                    {visible.length === 0 && (
                        <div className="px-3 py-8 text-center text-xs text-text-tertiary">
                            No extensions available yet.
                        </div>
                    )}
                </div>
            </aside>

            <SplitHandle
                onMouseDown={sidebar.onHandleMouseDown}
                ariaLabel={ariaLabel.resizeSidebar}
            />

            {/* 右栏：详情占位胶囊。纯框不塞假数据，只保证两栏布局的边框与
                底色到位，撑满右栏。

                底色取 `bg-page-bg`（不是 `bg-right-panel`）：`--color-right-panel`
                是 chat 侧那个 6-tab 侧栏 / 右侧检视器专用、比 page-bg 深一档的
                "raised surface"，而本栏是扩展详情的**内容主区**，对应 pm 的
                ProjectBoard、doc 的 DocEditor —— 那两处都是 `bg-page-bg`。
                左栏 list 同样不是 chat 的 AgentList 色系，而是和
                ProjectSidebar / DocTreeSidebar 共用 `bg-nav-surface`。 */}
            <div
                id="extensions-detail"
                role="tabpanel"
                aria-label={ariaLabel.detail}
                className={cn(CAPSULE_PANE_CN, "min-w-0 flex-1 bg-page-bg")}
            />
        </div>
    );
}
