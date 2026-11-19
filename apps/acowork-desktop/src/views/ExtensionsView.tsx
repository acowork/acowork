/**
 * ExtensionsView — hosted Apps workbench (ADR-116) + App Gallery browser.
 *
 * Left: card list (gallery catalog merged with local installs).
 * Right: README + install / open / lifecycle actions (VS Code–style).
 * Agents stay in Agent Library (PRD F7) — never mixed into this view.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { ExternalLink, Search } from "lucide-react";
import { CAPSULE_PANE_CN } from "../components/common/capsule";
import { SplitHandle } from "../components/common/SplitHandle";
import { StyledInput } from "../components/common/StyledInput";
import { useDragResize } from "../hooks/useDragResize";
import {
    filterGalleryCards,
    fetchGalleryIndex,
    galleryAttributionLinks,
    mergeGalleryCards,
    type GalleryAttributionKind,
    type GalleryCard,
    type InstallBadge,
} from "../lib/appGallery";
import {
    installAppFromUrl,
    installAppPackage,
    installErrorMessage,
    listAppVersions,
    listApps,
    rollbackApp,
    setAppEnabled,
    uninstallApp,
    type AppDto,
    type AppVersionInfo,
} from "../lib/apps-api";
import { useLayoutStore } from "../stores/layoutStore";
import { cn } from "../lib/utils";
import { useTranslation } from "../i18n/useTranslation";

function badgeClass(badge: InstallBadge): string {
    switch (badge) {
        case "installed":
        case "builtin":
            return "bg-ok-solid/15 text-ok-text";
        case "update":
            return "bg-warning-solid/15 text-warning-text";
        case "available":
            return "bg-accent-strong/15 text-accent-text";
        default:
            return "bg-nav-surface text-text-tertiary border border-border-divider";
    }
}

function attributionKey(kind: GalleryAttributionKind): string {
    if (kind === "homepage") return "extensions.homepage";
    if (kind === "upstream") return "extensions.upstream";
    return "extensions.repository";
}

function statusDot(app: AppDto | null): string {
    if (!app) return "bg-text-tertiary";
    if (app.status === "ready") return "bg-ok-solid";
    if (app.status === "disabled") return "bg-text-tertiary";
    return "bg-warning-solid";
}

export function ExtensionsView() {
    const { t } = useTranslation();
    const fileRef = useRef<HTMLInputElement>(null);
    const [query, setQuery] = useState("");
    const [selectedId, setSelectedId] = useState<string | null>(null);
    const [apps, setApps] = useState<AppDto[]>([]);
    const [cards, setCards] = useState<GalleryCard[]>([]);
    const [galleryError, setGalleryError] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const requestOpenHostedApp = useLayoutStore((s) => s.requestOpenHostedApp);
    const bumpHostedAppsEpoch = useLayoutStore((s) => s.bumpHostedAppsEpoch);
    const [actionError, setActionError] = useState<string | null>(null);
    const [versions, setVersions] = useState<AppVersionInfo[]>([]);
    const [currentVersion, setCurrentVersion] = useState<string | null>(null);
    const [confirmUninstall, setConfirmUninstall] = useState(false);
    const [purgeData, setPurgeData] = useState(false);
    const [installPct, setInstallPct] = useState<number | null>(null);

    const reloadLocal = async () => {
        if (typeof fetch !== "function") return [];
        try {
            const next = await listApps();
            setApps(next);
            setActionError(null);
            return next;
        } catch {
            return [] as AppDto[];
        }
    };

    const reloadAll = async () => {
        const local = await reloadLocal();
        try {
            const index = await fetchGalleryIndex();
            setGalleryError(null);
            setCards(mergeGalleryCards(index.apps, local));
        } catch (e) {
            setGalleryError(e instanceof Error ? e.message : String(e));
            // Still show local installs when the catalog is unreachable.
            setCards(mergeGalleryCards([], local));
        }
    };

    useEffect(() => {
        void reloadAll();
    }, []);

    useEffect(() => {
        setCards((prev) => {
            // Re-merge when local apps change but gallery entries are already loaded.
            const galleryApps = prev.map((c) => c.gallery).filter(Boolean) as NonNullable<
                GalleryCard["gallery"]
            >[];
            // Dedupe by id (prev may already be local-only cards).
            const unique = new Map(galleryApps.map((g) => [g.id, g]));
            return mergeGalleryCards([...unique.values()], apps);
        });
    }, [apps]);

    useEffect(() => {
        if (!selectedId) {
            setVersions([]);
            setCurrentVersion(null);
            return;
        }
        const app = apps.find((a) => a.id === selectedId);
        if (!app || app.builtin || !app.installed_version) {
            setVersions([]);
            setCurrentVersion(null);
            return;
        }
        void listAppVersions(selectedId)
            .then((v) => {
                setVersions(v.versions);
                setCurrentVersion(v.current);
            })
            .catch(() => {
                setVersions([]);
                setCurrentVersion(null);
            });
    }, [selectedId, apps]);

    const sidebar = useDragResize({
        storageKey: "acowork-extensions-list-width",
        defaultWidth: 320,
        minWidth: 240,
        maxWidth: 480,
    });

    const visible = useMemo(
        () => filterGalleryCards(cards, query),
        [cards, query],
    );
    const selected = visible.find((c) => c.id === selectedId) ?? cards.find((c) => c.id === selectedId) ?? null;

    const moveSelection = (key: string) => {
        if (visible.length === 0) return;
        const at = visible.findIndex((e) => e.id === selectedId);
        const next = key === "ArrowDown"
            ? Math.min(at < 0 ? 0 : at + 1, visible.length - 1)
            : Math.max(at < 0 ? 0 : at - 1, 0);
        setSelectedId(visible[next]?.id ?? visible[0].id);
    };

    const toggle = async (app: AppDto) => {
        setBusy(true);
        setActionError(null);
        try {
            await setAppEnabled(app.id, !app.enabled);
            await reloadLocal();
            bumpHostedAppsEpoch();
        } catch (e) {
            setActionError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    };

    const onInstallFile = async (file: File | undefined) => {
        if (!file) return;
        setBusy(true);
        setActionError(null);
        setInstallPct(0);
        try {
            const result = await installAppPackage(file, setInstallPct);
            await reloadAll();
            bumpHostedAppsEpoch();
            if (result.app?.id) setSelectedId(result.app.id);
            else if (result.app_id) setSelectedId(result.app_id);
        } catch (e) {
            setActionError(installErrorMessage(e));
        } finally {
            setBusy(false);
            setInstallPct(null);
            if (fileRef.current) fileRef.current.value = "";
        }
    };

    const onInstallUrl = async (card: GalleryCard) => {
        const pkg = card.gallery?.package;
        if (!pkg?.url || !pkg.sha256) return;
        setBusy(true);
        setActionError(null);
        try {
            const result = await installAppFromUrl(pkg.url, pkg.sha256);
            await reloadAll();
            bumpHostedAppsEpoch();
            if (result.app?.id) setSelectedId(result.app.id);
            else setSelectedId(card.id);
        } catch (e) {
            setActionError(installErrorMessage(e));
        } finally {
            setBusy(false);
        }
    };

    const onRollback = async (version: string) => {
        if (!selected?.installed) return;
        setBusy(true);
        setActionError(null);
        try {
            await rollbackApp(selected.installed.id, version);
            await reloadLocal();
        } catch (e) {
            setActionError(installErrorMessage(e));
        } finally {
            setBusy(false);
        }
    };

    const onUninstall = async () => {
        if (!selected?.installed || selected.installed.builtin) return;
        setBusy(true);
        setActionError(null);
        try {
            await uninstallApp(selected.installed.id, purgeData);
            setConfirmUninstall(false);
            setPurgeData(false);
            setSelectedId(null);
            await reloadAll();
            bumpHostedAppsEpoch();
        } catch (e) {
            setActionError(installErrorMessage(e));
        } finally {
            setBusy(false);
        }
    };

    const openInstalled = (app: AppDto) => {
        // Built-ins keep dedicated main-nav views (same as the left rail).
        if (app.builtin) {
            if (app.id === "pm") useLayoutStore.getState().requestNavView("projects");
            else if (app.id === "doc") useLayoutStore.getState().requestNavView("docs");
            return;
        }
        requestOpenHostedApp(app.id);
    };

    const badgeLabel = (badge: InstallBadge) => {
        switch (badge) {
            case "installed":
                return t("extensions.badgeInstalled");
            case "update":
                return t("extensions.badgeUpdate");
            case "available":
                return t("extensions.badgeAvailable");
            case "builtin":
                return t("extensions.badgeBuiltin");
            default:
                return t("extensions.badgeCatalogOnly");
        }
    };

    return (
        <div className="flex h-full min-h-0 w-full flex-1">
            <aside
                className={cn(CAPSULE_PANE_CN, "h-full shrink-0 bg-nav-surface text-xs")}
                style={{ width: sidebar.width }}
            >
                <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center gap-2 border-b border-border-divider px-3">
                    <div className="relative min-w-0 flex-1">
                        <Search
                            className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-text-tertiary"
                            aria-hidden="true"
                        />
                        <StyledInput
                            type="text"
                            value={query}
                            onChange={(e) => setQuery(e.target.value)}
                            placeholder={t("extensions.search")}
                            aria-label={t("extensions.search")}
                            className="rounded-md bg-input-bg pl-7 py-1.5 pr-2"
                        />
                    </div>
                    <button
                        type="button"
                        disabled={busy}
                        onClick={() => fileRef.current?.click()}
                        className="shrink-0 rounded-md border border-border-divider px-2 py-1 text-[11px] text-text-secondary disabled:opacity-50"
                        title={t("extensions.installLocal")}
                    >
                        {t("extensions.installLocal")}
                    </button>
                    <input
                        ref={fileRef}
                        type="file"
                        accept=".app,application/octet-stream"
                        className="hidden"
                        onChange={(e) => void onInstallFile(e.target.files?.[0])}
                    />
                </div>

                {galleryError && (
                    <div className="border-b border-border-divider px-3 py-2 text-[11px] text-warning-text">
                        {t("extensions.galleryUnavailable")}: {galleryError}
                    </div>
                )}

                <div
                    role="listbox"
                    aria-label={t("extensions.list")}
                    aria-controls="extensions-detail"
                    tabIndex={-1}
                    onKeyDown={(e) => {
                        if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
                        e.preventDefault();
                        moveSelection(e.key);
                    }}
                    className="min-h-0 flex-1 space-y-1.5 overflow-y-auto overflow-x-hidden p-2"
                >
                    {visible.map((card) => {
                        const active = card.id === selectedId;
                        return (
                            <button
                                key={card.id}
                                type="button"
                                role="option"
                                aria-selected={active}
                                onClick={() => {
                                    setSelectedId(card.id);
                                    setConfirmUninstall(false);
                                }}
                                className={cn(
                                    "flex w-full gap-3 rounded-lg border px-3 py-2.5 text-left transition-colors duration-150",
                                    active
                                        ? "border-accent-strong/40 bg-accent-strong/10"
                                        : "border-border-divider bg-page-bg/40 hover:bg-page-bg/80",
                                )}
                            >
                                <div className="flex h-10 w-10 shrink-0 items-center justify-center overflow-hidden rounded-md border border-border-divider bg-nav-surface">
                                    {card.icon ? (
                                        <img
                                            src={card.icon}
                                            alt=""
                                            className="h-full w-full object-cover"
                                        />
                                    ) : (
                                        <span className="text-sm font-medium text-text-tertiary">
                                            {card.name.slice(0, 1).toUpperCase()}
                                        </span>
                                    )}
                                </div>
                                <div className="min-w-0 flex-1">
                                    <div className="flex items-center gap-2">
                                        <span className="truncate text-[12px] font-medium text-text-primary">
                                            {card.name}
                                        </span>
                                        <span className={cn("h-1.5 w-1.5 shrink-0 rounded-full", statusDot(card.installed))} />
                                    </div>
                                    <div className="mt-0.5 truncate text-[11px] text-text-tertiary">
                                        {card.publisherLabel}
                                        {card.versionLabel && card.versionLabel !== "—"
                                            ? ` · ${card.versionLabel}`
                                            : ""}
                                    </div>
                                    <div className="mt-1 line-clamp-2 text-[11px] leading-snug text-text-secondary">
                                        {card.summary}
                                    </div>
                                    <span
                                        className={cn(
                                            "mt-1.5 inline-block rounded px-1.5 py-0.5 text-[10px]",
                                            badgeClass(card.badge),
                                        )}
                                    >
                                        {badgeLabel(card.badge)}
                                    </span>
                                </div>
                            </button>
                        );
                    })}

                    {visible.length === 0 && (
                        <div className="px-3 py-8 text-center text-xs text-text-tertiary">
                            {t("extensions.empty")}
                        </div>
                    )}
                </div>
            </aside>

            <SplitHandle
                onMouseDown={sidebar.onHandleMouseDown}
                ariaLabel={t("extensions.resizeSidebar")}
            />

            <div
                id="extensions-detail"
                role="tabpanel"
                aria-label={t("extensions.detail")}
                className={cn(CAPSULE_PANE_CN, "min-w-0 flex-1 overflow-hidden bg-page-bg")}
            >
                {selected ? (
                    <div className="flex h-full min-h-0 flex-col">
                        <div className="shrink-0 border-b border-border-divider px-5 py-4">
                            <div className="flex gap-4">
                                <div className="flex h-14 w-14 shrink-0 items-center justify-center overflow-hidden rounded-xl border border-border-divider bg-nav-surface">
                                    {selected.icon ? (
                                        <img src={selected.icon} alt="" className="h-full w-full object-cover" />
                                    ) : (
                                        <span className="text-lg text-text-tertiary">
                                            {selected.name.slice(0, 1).toUpperCase()}
                                        </span>
                                    )}
                                </div>
                                <div className="min-w-0 flex-1">
                                    <div className="text-base font-medium text-text-primary">{selected.name}</div>
                                    <div className="mt-0.5 text-xs text-text-tertiary">
                                        {selected.publisherLabel}
                                        {selected.gallery?.license ? ` · ${selected.gallery.license}` : ""}
                                    </div>
                                    <div className="mt-2 text-xs text-text-secondary">{selected.summary}</div>
                                </div>
                            </div>

                            {installPct !== null && (
                                <div className="mt-3 text-xs text-text-tertiary">
                                    {t("extensions.installing")} {installPct}%
                                </div>
                            )}
                            {actionError && (
                                <div className="mt-3 text-xs text-danger-text">{actionError}</div>
                            )}

                            <div className="mt-4 flex flex-wrap items-center gap-2">
                                {selected.badge === "available" || selected.badge === "update" ? (
                                    <button
                                        type="button"
                                        disabled={busy}
                                        onClick={() => void onInstallUrl(selected)}
                                        className="rounded-md btn-accent px-3 py-1.5 text-xs text-white disabled:opacity-50"
                                    >
                                        {selected.badge === "update"
                                            ? t("extensions.update")
                                            : t("extensions.install")}
                                    </button>
                                ) : null}

                                {selected.badge === "catalog_only" && selected.gallery?.repository ? (
                                    <a
                                        href={selected.gallery.repository}
                                        target="_blank"
                                        rel="noreferrer"
                                        className="inline-flex items-center gap-1 rounded-md border border-border-divider px-3 py-1.5 text-xs text-text-secondary"
                                    >
                                        {t("extensions.openRepo")}
                                        <ExternalLink className="h-3 w-3" aria-hidden />
                                    </a>
                                ) : null}

                                {selected.installed && (
                                    <>
                                        <button
                                            type="button"
                                            disabled={busy}
                                            onClick={() => void toggle(selected.installed!)}
                                            className="rounded-md border border-border-divider px-3 py-1.5 text-xs text-text-secondary disabled:opacity-50"
                                        >
                                            {selected.installed.enabled
                                                ? t("extensions.disable")
                                                : t("extensions.enable")}
                                        </button>
                                        <button
                                            type="button"
                                            disabled={
                                                !selected.installed.enabled ||
                                                selected.installed.status !== "ready"
                                            }
                                            onClick={() => openInstalled(selected.installed!)}
                                            className="rounded-md btn-accent px-3 py-1.5 text-xs text-white disabled:opacity-50"
                                        >
                                            {t("extensions.open")}
                                        </button>
                                        {!selected.installed.builtin && !confirmUninstall && (
                                            <button
                                                type="button"
                                                disabled={busy}
                                                onClick={() => setConfirmUninstall(true)}
                                                className="rounded-md border border-border-divider px-3 py-1.5 text-xs text-text-tertiary disabled:opacity-50"
                                            >
                                                {t("extensions.uninstall")}
                                            </button>
                                        )}
                                    </>
                                )}

                                {selected.gallery
                                    ? galleryAttributionLinks(selected.gallery)
                                          .filter(
                                              (link) =>
                                                  selected.badge !== "catalog_only" ||
                                                  link.kind !== "repository",
                                          )
                                          .map((link) => (
                                              <a
                                                  key={link.kind}
                                                  href={link.href}
                                                  target="_blank"
                                                  rel="noreferrer"
                                                  className="inline-flex items-center gap-1 text-[11px] text-text-tertiary hover:text-text-secondary"
                                              >
                                                  {t(attributionKey(link.kind))}
                                                  <ExternalLink className="h-3 w-3" aria-hidden />
                                              </a>
                                          ))
                                    : null}
                            </div>

                            {!selected.installed?.builtin && confirmUninstall && (
                                <div className="mt-3 rounded-md border border-border-divider bg-nav-surface p-3 text-xs">
                                    <div className="mb-2 text-text-secondary">{t("extensions.uninstallConfirm")}</div>
                                    <label className="mb-3 flex items-center gap-2 text-text-tertiary">
                                        <input
                                            type="checkbox"
                                            checked={purgeData}
                                            onChange={(e) => setPurgeData(e.target.checked)}
                                        />
                                        {t("extensions.purgeData")}
                                    </label>
                                    <div className="flex justify-end gap-2">
                                        <button
                                            type="button"
                                            onClick={() => setConfirmUninstall(false)}
                                            className="rounded-md border border-border-divider px-3 py-1.5"
                                        >
                                            {t("extensions.cancel")}
                                        </button>
                                        <button
                                            type="button"
                                            disabled={busy}
                                            onClick={() => void onUninstall()}
                                            className="rounded-md border border-danger-border px-3 py-1.5 text-danger-text disabled:opacity-50"
                                        >
                                            {t("extensions.uninstall")}
                                        </button>
                                    </div>
                                </div>
                            )}
                        </div>

                        <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
                            {selected.gallery?.readme_md ? (
                                <div className="prose prose-sm prose-zinc dark:prose-invert max-w-none text-text-secondary">
                                    <ReactMarkdown remarkPlugins={[remarkGfm]}>
                                        {selected.gallery.readme_md}
                                    </ReactMarkdown>
                                </div>
                            ) : selected.installed ? (
                                <dl className="grid grid-cols-[120px_1fr] gap-y-2 text-xs text-text-secondary">
                                    <dt className="text-text-tertiary">{t("extensions.status")}</dt>
                                    <dd className="flex items-center gap-2">
                                        <span className={cn("h-1.5 w-1.5 rounded-full", statusDot(selected.installed))} />
                                        {selected.installed.status}
                                        {selected.installed.port ? ` · port ${selected.installed.port}` : ""}
                                    </dd>
                                    <dt className="text-text-tertiary">{t("extensions.version")}</dt>
                                    <dd>{selected.installed.installed_version || selected.installed.version}</dd>
                                    <dt className="text-text-tertiary">{t("extensions.runtime")}</dt>
                                    <dd>
                                        {selected.installed.runtime}
                                        {selected.installed.builtin ? ` · ${t("extensions.builtin")}` : ""}
                                    </dd>
                                    <dt className="text-text-tertiary">MCP</dt>
                                    <dd className="break-all">{selected.installed.mcp_gateway_path}</dd>
                                    <dt className="text-text-tertiary">{t("extensions.appUi")}</dt>
                                    <dd className="break-all">
                                        /api/apps/{selected.installed.id}{selected.installed.ui_path}
                                    </dd>
                                </dl>
                            ) : (
                                <div className="text-xs text-text-tertiary">{t("extensions.noReadme")}</div>
                            )}

                            {versions.length > 0 && (
                                <div className="mt-6 rounded-md border border-border-divider p-3 text-xs">
                                    <div className="mb-2 text-text-tertiary">{t("extensions.versions")}</div>
                                    <ul className="space-y-1">
                                        {versions.map((v) => (
                                            <li key={v.version} className="flex items-center gap-2">
                                                <span className="min-w-0 flex-1 font-mono">
                                                    {v.version}
                                                    {v.current || v.version === currentVersion
                                                        ? ` · ${t("extensions.current")}`
                                                        : ""}
                                                </span>
                                                {!v.current && v.version !== currentVersion && (
                                                    <button
                                                        type="button"
                                                        disabled={busy}
                                                        onClick={() => void onRollback(v.version)}
                                                        className="rounded border border-border-divider px-2 py-0.5 text-[11px] disabled:opacity-50"
                                                    >
                                                        {t("extensions.rollback")}
                                                    </button>
                                                )}
                                            </li>
                                        ))}
                                    </ul>
                                </div>
                            )}
                        </div>
                    </div>
                ) : (
                    <div className="flex h-full items-center justify-center text-xs text-text-tertiary">
                        {t("extensions.selectHint")}
                    </div>
                )}
            </div>
        </div>
    );
}
