/**
 * ExtensionsView — the hosted-application workbench (ADR-116).
 *
 * Two-column capsule layout shared with PM / DOC: search + list on the left
 * (`bg-nav-surface`), detail / app frame on the right (`bg-page-bg`). The
 * right column is a CONTENT main area, not the chat view's 6-tab inspector
 * (`bg-right-panel`), so it uses `bg-page-bg` like ProjectBoard / DocEditor.
 *
 * The list is driven by `GET /api/apps` (Gateway app registry). Selecting an
 * app shows its declared contract; "Open" loads the app's own front end in a
 * sandboxed iframe. The Desktop contributes the container and the
 * postMessage bridge only — the app owns the page, the Gateway owns identity
 * and routing. Note this view is about *applications*, not the Agent Hub
 * (PRD F7): keep those two concepts separate.
 *
 * The aria labels / placeholders remain hardcoded English: no `extensions`
 * i18n namespace exists yet, and introducing one here would be unrelated to
 * wiring the data. 5 locales stay untouched.
 */
import { useEffect, useState } from "react";
import { Search } from "lucide-react";
import { AppFrame } from "../components/apps/AppFrame";
import { CAPSULE_PANE_CN } from "../components/common/capsule";
import { SplitHandle } from "../components/common/SplitHandle";
import { StyledInput } from "../components/common/StyledInput";
import { useDragResize } from "../hooks/useDragResize";
import { listApps, setAppEnabled, type AppDto } from "../lib/apps-api";
import { cn } from "../lib/utils";
import { listRowHover } from "../lib/ui-styles";

const ariaLabel = {
    search: "Search extensions",
    list: "Extensions list",
    detail: "Extension details",
    resizeSidebar: "Resize extensions sidebar",
};

function statusColor(app: AppDto): string {
    if (app.status === "ready") return "bg-ok-solid";
    if (app.status === "disabled") return "bg-text-tertiary";
    return "bg-warning-solid";
}

export function ExtensionsView() {
    const [query, setQuery] = useState("");
    const [selectedId, setSelectedId] = useState<string | null>(null);
    const [apps, setApps] = useState<AppDto[]>([]);
    const [openedId, setOpenedId] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [actionError, setActionError] = useState<string | null>(null);

    const reload = async () => {
        if (typeof fetch !== "function") return;
        try {
            const next = await listApps();
            setApps(next);
            setActionError(null);
        } catch {
            // Gateway unreachable / not yet authenticated: keep the honest
            // empty state rather than rendering placeholder rows.
        }
    };

    useEffect(() => {
        void reload();
    }, []);

    // 与 pm / docs 左侧栏一致的可拖动宽度（localStorage 持久化）
    const sidebar = useDragResize({
        storageKey: "acowork-extensions-list-width",
        defaultWidth: 240,
        minWidth: 160,
        maxWidth: 400,
    });

    const visible = apps.filter((e) =>
        `${e.name} ${e.id}`.toLowerCase().includes(query.trim().toLowerCase()),
    );
    const selected = apps.find((e) => e.id === selectedId) ?? null;
    const opened = apps.find((e) => e.id === openedId) ?? null;

    // 上下键在可见项之间移动选中（标准 listbox 模式）；列表为空时无事发生。
    const moveSelection = (key: string) => {
        if (visible.length === 0) return;
        const at = visible.findIndex((e) => e.id === selectedId);
        const next = key === "ArrowDown"
            ? Math.min(at + 1, visible.length - 1)
            : Math.max(at - 1, 0);
        setSelectedId(visible[next]?.id ?? visible[0].id);
    };

    const toggle = async (app: AppDto) => {
        setBusy(true);
        setActionError(null);
        try {
            await setAppEnabled(app.id, !app.enabled);
            // Disabling the app we are currently viewing must close its
            // iframe immediately; the session is no longer valid there.
            if (app.enabled) setOpenedId(null);
            await reload();
        } catch (e) {
            setActionError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
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
                                onClick={() => {
                                    setSelectedId(e.id);
                                    setOpenedId(null);
                                }}
                                className={cn(
                                    "relative flex w-full items-center gap-2 rounded-md px-3 py-2.5 text-left transition-colors duration-150",
                                    active
                                        ? "bg-accent-strong/90 text-white"
                                        : cn("text-text-secondary ", listRowHover),
                                    !isLast &&
                                        "row-divider-b",
                                )}
                            >
                                <span className={cn("h-1.5 w-1.5 shrink-0 rounded-full", statusColor(e))} />
                                <span className={cn("min-w-0 flex-1 truncate", active && "text-white")}>
                                    {e.name}
                                </span>
                                <span className={cn("shrink-0 text-[11px]", active ? "text-white/80" : "text-text-tertiary")}>
                                    {e.version}
                                </span>
                            </button>
                        );
                    })}

                    {/* 空列表时保持诚实的空态，绝不塞假条目。 */}
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

            {/* 右栏：详情 / 应用 iframe。底色取 `bg-page-bg`（内容主区），
                不是 chat 的 `bg-right-panel`。 */}
            <div
                id="extensions-detail"
                role="tabpanel"
                aria-label={ariaLabel.detail}
                className={cn(CAPSULE_PANE_CN, "min-w-0 flex-1 overflow-hidden bg-page-bg")}
            >
                {opened ? (
                    <div className="flex h-full min-h-0 flex-col">
                        <div className="flex shrink-0 items-center justify-end gap-2 border-b border-border-divider px-3 py-1.5">
                            <span className="mr-auto text-xs text-text-tertiary">{opened.name}</span>
                            <button
                                type="button"
                                onClick={() => setOpenedId(null)}
                                className="rounded-md border border-border-divider px-2 py-1 text-xs text-text-secondary"
                            >
                                Back
                            </button>
                        </div>
                        <div className="min-h-0 flex-1">
                            <AppFrame app={opened} />
                        </div>
                    </div>
                ) : selected ? (
                    <div className="flex h-full flex-col gap-4 overflow-y-auto p-5 text-xs">
                        <div>
                            <div className="text-sm text-text-primary">{selected.name}</div>
                            <div className="mt-1 text-text-tertiary">
                                {selected.description || selected.id}
                            </div>
                        </div>
                        <dl className="grid grid-cols-[120px_1fr] gap-y-2 text-text-secondary">
                            <dt className="text-text-tertiary">Status</dt>
                            <dd className="flex items-center gap-2">
                                <span className={cn("h-1.5 w-1.5 rounded-full", statusColor(selected))} />
                                {selected.status}
                                {selected.port ? ` · port ${selected.port}` : ""}
                            </dd>
                            <dt className="text-text-tertiary">Version</dt>
                            <dd>{selected.version}</dd>
                            <dt className="text-text-tertiary">Runtime</dt>
                            <dd>{selected.runtime}{selected.builtin ? " · built-in" : ""}</dd>
                            <dt className="text-text-tertiary">MCP</dt>
                            <dd className="break-all">{selected.mcp_gateway_path}</dd>
                            <dt className="text-text-tertiary">App UI</dt>
                            <dd className="break-all">/api/apps/{selected.id}{selected.ui_path}</dd>
                            <dt className="text-text-tertiary">Permissions</dt>
                            <dd>{selected.permissions.join(", ") || "none"}</dd>
                        </dl>
                        {actionError && (
                            <div className="text-danger-text">{actionError}</div>
                        )}
                        <div className="mt-auto flex items-center justify-end gap-2 border-t border-border-divider pt-3">
                            <button
                                type="button"
                                disabled={busy}
                                onClick={() => void toggle(selected)}
                                className="rounded-md border border-border-divider px-3 py-1.5 text-xs text-text-secondary disabled:opacity-50"
                            >
                                {selected.enabled ? "Disable" : "Enable"}
                            </button>
                            <button
                                type="button"
                                disabled={!selected.enabled || selected.status !== "ready"}
                                onClick={() => setOpenedId(selected.id)}
                                className="rounded-md btn-accent px-3 py-1.5 text-xs text-white disabled:opacity-50"
                            >
                                Open app
                            </button>
                        </div>
                    </div>
                ) : null}
            </div>
        </div>
    );
}
