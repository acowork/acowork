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
import { ExternalLink, Loader2, Search } from "lucide-react";
import { CAPSULE_PANE_CN } from "../components/common/capsule";
import { RemoteArtwork } from "../components/common/RemoteArtwork";
import { SplitHandle } from "../components/common/SplitHandle";
import { StyledInput } from "../components/common/StyledInput";
import { useDragResize } from "../hooks/useDragResize";
import {
    filterGalleryCards,
    fetchGalleryIndex,
    galleryAttributionLinks,
    mergeGalleryCards,
    readCachedGalleryIndex,
    type GalleryAttributionKind,
    type GalleryCard,
    type InstallBadge,
} from "../lib/appGallery";
import { sameCatalog } from "../lib/catalogCache";
import {
    installErrorText,
    installErrorMessage,
    installJobActive,
    installJobPct,
    listAppVersions,
    listApps,
    listInstallJobs,
    readCachedInstalledApps,
    rollbackApp,
    setAppEnabled,
    startAppInstallFromPackage,
    startAppInstallFromUrl,
    uninstallApp,
    type AppDto,
    type AppVersionInfo,
    type InstallJobDto,
} from "../lib/apps-api";
import { useLayoutStore } from "../stores/layoutStore";
import { cn } from "../lib/utils";
import { useTranslation } from "../i18n/useTranslation";

/** Sentinel id for an uploaded `.app`: the gateway assigns the real app id
 *  only after it unpacks the package, but the upload has an in-flight state
 *  to show exactly like a gallery install does. */
const LOCAL_INSTALL = "\0local";

/** How often the panel polls the Gateway while an install is running. */
const INSTALL_POLL_MS = 600;

/**
 * A just-installed app reports `starting` until its own process answers. Poll
 * the local list until it flips, bounded — an app that never comes up must not
 * be polled forever (it then keeps the amber dot, which is the truth).
 */
const LAUNCH_POLL_MS = 1500;
const LAUNCH_POLL_MAX = 20;

/**
 * Identity-preserving compare (repo convention): a poll that returns what is
 * already on screen must not produce a new array, or every card re-renders
 * twice a second for nothing.
 */
function sameJobs(a: InstallJobDto[], b: InstallJobDto[]): boolean {
    if (a.length !== b.length) return false;
    return a.every((job, i) => {
        const other = b[i];
        return (
            job.job_id === other.job_id &&
            job.phase === other.phase &&
            job.received_bytes === other.received_bytes &&
            job.total_bytes === other.total_bytes &&
            job.error === other.error &&
            job.app_id === other.app_id
        );
    });
}

/** Insert-or-replace one job by id, keeping the list newest-first. */
function upsertJob(jobs: InstallJobDto[], job: InstallJobDto): InstallJobDto[] {
    return [job, ...jobs.filter((j) => j.job_id !== job.job_id)];
}

/**
 * One row's progress: a real byte bar while the package downloads, an
 * indeterminate one for the steps behind it (unpack / verify / spawn take
 * however long the package and the disk need, and no honest percentage
 * exists for them).
 */
function InstallProgress({ job }: { job: InstallJobDto }) {
    const { t } = useTranslation();
    const pct = installJobPct(job);
    const label =
        job.phase === "download"
            ? t("extensions.downloading")
            : job.phase === "verify"
              ? t("extensions.verifying")
              : t("extensions.installing");
    return (
        <div role="status" aria-live="polite" className="w-full">
            <div className="flex items-center justify-between gap-2 text-[11px] text-text-tertiary">
                <span className="inline-flex items-center gap-1">
                    <Loader2 className="h-3 w-3 animate-spin" aria-hidden="true" />
                    {label}
                </span>
                {pct !== null && <span className="tabular-nums">{pct}%</span>}
            </div>
            {/* Unknown size (no Content-Length yet) is 0 %, not "full": an
                empty track pulsing is honest, a full bar reads as finished. */}
            <div
                className={cn(
                    "mt-1 h-1 w-full overflow-hidden rounded-full bg-nav-surface",
                    pct === null && "animate-pulse",
                )}
            >
                <div
                    className="h-full rounded-full bg-accent-strong transition-[width] duration-500 ease-out"
                    style={{ width: `${pct ?? 0}%` }}
                />
            </div>
        </div>
    );
}

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

/** `app_status` in the Gateway is exactly these three plus "not installed". */
function statusLabelKey(app: AppDto | null): string {
    if (!app) return "extensions.statusNotInstalled";
    if (app.status === "ready") return "extensions.statusReady";
    if (app.status === "disabled") return "extensions.statusDisabled";
    return "extensions.statusStarting";
}

export function ExtensionsView() {
    const { t } = useTranslation();
    const fileRef = useRef<HTMLInputElement>(null);
    const [query, setQuery] = useState("");
    const [selectedId, setSelectedId] = useState<string | null>(null);
    // Optimistic first paint (catalogCache.ts): the view remounts on every
    // entry, and without the seed every entry paid a skeleton + letter-tile
    // frame for the round trip. The network pass below still runs; its data
    // is applied only when it differs from what is already on screen.
    const [apps, setApps] = useState<AppDto[]>(() => readCachedInstalledApps() ?? []);
    const [cards, setCards] = useState<GalleryCard[]>(() =>
        mergeGalleryCards(readCachedGalleryIndex()?.apps ?? [], readCachedInstalledApps() ?? []),
    );
    const [galleryError, setGalleryError] = useState<string | null>(null);
    // The optimistic seed paints cards immediately, so the network pass is
    // invisible without a sign of its own: a thin spinner strip above the
    // list (the same shape as the chat history's load-more row) says
    // "fresh data is on the way" instead of looking frozen.
    const [syncing, setSyncing] = useState(true);
    const [busy, setBusy] = useState(false);
    const requestOpenHostedApp = useLayoutStore((s) => s.requestOpenHostedApp);
    const bumpHostedAppsEpoch = useLayoutStore((s) => s.bumpHostedAppsEpoch);
    const [actionError, setActionError] = useState<string | null>(null);
    const [versions, setVersions] = useState<AppVersionInfo[]>([]);
    const [currentVersion, setCurrentVersion] = useState<string | null>(null);
    const [confirmUninstall, setConfirmUninstall] = useState(false);
    const [purgeData, setPurgeData] = useState(false);
    // Install jobs live on the Gateway (`/api/apps/install-jobs`), so this
    // panel is only a view of them: it refetches on mount — that is what makes
    // a running install survive switching panels — and polls while one is
    // active. The Gateway is also what refuses a duplicate submit for the same
    // card, so a re-click cannot start a second 17–77 MB download.
    const [jobs, setJobs] = useState<InstallJobDto[]>([]);

    // `quiet` for the launch poll below: it must not wipe an install error the
    // user is still reading.
    const reloadLocal = async (quiet = false) => {
        if (typeof fetch !== "function") return [];
        try {
            const next = await listApps();
            setApps((prev) => (sameCatalog(prev, next) ? prev : next));
            if (!quiet) setActionError(null);
            return next;
        } catch {
            return [] as AppDto[];
        }
    };

    const reloadAll = async () => {
        setSyncing(true);
        const local = await reloadLocal();
        try {
            const index = await fetchGalleryIndex();
            setGalleryError(null);
            const merged = mergeGalleryCards(index.apps, local);
            setCards((prev) => (sameCatalog(prev, merged) ? prev : merged));
        } catch (e) {
            setGalleryError(e instanceof Error ? e.message : String(e));
            // Catalog unreachable: keep the cache-seeded cards (they are the
            // last known truth, and the banner above says so). Only a cold
            // machine with nothing cached falls back to local-only cards.
            setCards((prev) =>
                prev.length > 0 ? prev : mergeGalleryCards([], local),
            );
        } finally {
            setSyncing(false);
        }
    };

    useEffect(() => {
        void reloadAll();
    }, []);

    // The install job's single `reloadAll()` lands before the spawned process
    // answers, so its dot used to stay amber until the panel was remounted.
    // While anything is mid-launch, ask again.
    const launching = apps.some((a) => a.status === "starting");
    useEffect(() => {
        if (!launching) return;
        let ticks = 0;
        const timer = setInterval(() => {
            if (++ticks > LAUNCH_POLL_MAX) {
                clearInterval(timer);
                return;
            }
            void reloadLocal(true);
        }, LAUNCH_POLL_MS);
        return () => clearInterval(timer);
    }, [launching]);

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

    // Running jobs by card id. A finished job drops out immediately: the card
    // then shows its real badge again ( Installed / Update ), because the
    // install either landed or errored — see the completion effect below.
    const jobsByCard = useMemo(() => {
        const map = new Map<string, InstallJobDto>();
        for (const job of jobs) {
            if (job.hint_id && installJobActive(job)) map.set(job.hint_id, job);
        }
        return map;
    }, [jobs]);
    const selectedJob = selected ? jobsByCard.get(selected.id) : undefined;
    const polling = useMemo(() => jobs.some(installJobActive), [jobs]);

    // Reattach on entry: the Gateway, not this component, knows what is
    // running. Without this the panel came back showing a fresh Install
    // button while the download was still going.
    useEffect(() => {
        void listInstallJobs()
            .then((next) => setJobs((prev) => (sameJobs(prev, next) ? prev : next)))
            .catch(() => {});
    }, []);

    // Poll only while something is actually moving.
    useEffect(() => {
        if (!polling) return;
        const timer = setInterval(() => {
            void listInstallJobs()
                .then((next) => setJobs((prev) => (sameJobs(prev, next) ? prev : next)))
                .catch(() => {});
        }, INSTALL_POLL_MS);
        return () => clearInterval(timer);
    }, [polling]);

    // One place where a job's terminal state becomes a UI effect, however the
    // job got here (started in this panel, or reattached on entry).
    const settledRef = useRef<ReadonlySet<string>>(new Set());
    useEffect(() => {
        const settled = jobs.filter((job) => !installJobActive(job));
        const fresh = settled.filter((job) => !settledRef.current.has(job.job_id));
        if (fresh.length === 0) return;
        settledRef.current = new Set([...settledRef.current, ...fresh.map((j) => j.job_id)]);
        const done = fresh.filter((job) => job.phase === "done");
        const failed = fresh.filter((job) => job.phase === "failed");
        if (done.length > 0) {
            void reloadAll();
            bumpHostedAppsEpoch();
            const last = done[done.length - 1];
            if (last.app_id) setSelectedId(last.app_id);
        }
        if (failed.length > 0) {
            const last = failed[failed.length - 1];
            setActionError(installErrorText(last.error_code, last.error));
            // The error line lives in the detail pane, so bring the failing
            // card into view — otherwise the click just left a dead button.
            if (last.hint_id && last.hint_id !== LOCAL_INSTALL) setSelectedId(last.hint_id);
        }
    }, [jobs]);

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

    // Both starters only ACCEPT the work: the Gateway answers with the job it
    // now owns, and the polling effect above carries it from there.
    const onInstallFile = async (file: File | undefined) => {
        if (!file) return;
        setActionError(null);
        try {
            const job = await startAppInstallFromPackage(file, LOCAL_INSTALL);
            setJobs((prev) => upsertJob(prev, job));
        } catch (e) {
            setActionError(installErrorMessage(e));
        } finally {
            if (fileRef.current) fileRef.current.value = "";
        }
    };

    const onInstallUrl = async (card: GalleryCard) => {
        const pkg = card.gallery?.package;
        // Unreachable from the UI (the button requires a package), kept so the
        // handler cannot fire an install POST with a half-read catalog entry.
        if (!pkg?.url || !pkg.sha256) return;
        setActionError(null);
        try {
            const job = await startAppInstallFromUrl(pkg.url, pkg.sha256, card.id);
            setJobs((prev) => upsertJob(prev, job));
        } catch (e) {
            setActionError(installErrorMessage(e));
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
            // Stay on the card: after the reload it is a plain catalog entry,
            // so the pane flips to its Install page instead of going blank
            // (the card itself may be gone for a local-only upload, and then
            // the pane falls back to the select-a-card hint on its own).
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
                        className="inline-flex shrink-0 items-center gap-1 rounded-md border border-border-divider px-2 py-1 text-[11px] text-text-secondary disabled:opacity-50"
                        title={t("extensions.installLocal")}
                    >
                        {jobsByCard.has(LOCAL_INSTALL) && (
                            <Loader2 className="h-3 w-3 animate-spin" aria-hidden="true" />
                        )}
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

                {/* Sync-in-progress strip — same shape as the chat history's
                    load-more row (spinner + label, centered, quiet). It only
                    rides the network pass; the cache-seeded cards stay put
                    underneath, so nothing jumps when it appears or goes. */}
                {syncing && (
                    <div
                        role="status"
                        aria-live="polite"
                        className="flex items-center justify-center gap-1.5 py-2 text-10 text-text-tertiary"
                    >
                        <Loader2 className="h-3 w-3 animate-spin" />
                        {t("common.loading")}
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
                        const job = jobsByCard.get(card.id);
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
                                        <RemoteArtwork
                                            src={card.icon}
                                            className="h-full w-full object-cover"
                                            fallback={
                                                <span className="text-sm font-medium text-text-tertiary">
                                                    {card.name.slice(0, 1).toUpperCase()}
                                                </span>
                                            }
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
                                        <span
                                            role="img"
                                            title={t(statusLabelKey(card.installed))}
                                            aria-label={t(statusLabelKey(card.installed))}
                                            className={cn("h-1.5 w-1.5 shrink-0 rounded-full", statusDot(card.installed))}
                                        />
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
                                    {job ? (
                                        <div className="mt-1.5">
                                            <InstallProgress job={job} />
                                        </div>
                                    ) : (
                                        <span
                                            className={cn(
                                                "mt-1.5 inline-block rounded px-1.5 py-0.5 text-[10px]",
                                                badgeClass(card.badge),
                                            )}
                                        >
                                            {badgeLabel(card.badge)}
                                        </span>
                                    )}
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
                                        <RemoteArtwork
                                            src={selected.icon}
                                            className="h-full w-full object-cover"
                                            fallback={null}
                                        />
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

                            {selectedJob && (
                                <div className="mt-3">
                                    <InstallProgress job={selectedJob} />
                                </div>
                            )}
                            {jobsByCard.has(LOCAL_INSTALL) && !selectedJob && (
                                <div className="mt-3">
                                    <InstallProgress job={jobsByCard.get(LOCAL_INSTALL)!} />
                                </div>
                            )}
                            {actionError && (
                                <div className="mt-3 text-xs text-danger-text">{actionError}</div>
                            )}

                            <div className="mt-4 flex flex-wrap items-center gap-2">
                                {/* A catalog entry with no downloadable package
                                    cannot be installed: showing a button for it
                                    was a dead click (badge `update` does not
                                    imply a `package`). */}
                                {(selected.badge === "available" || selected.badge === "update") &&
                                selected.gallery?.package?.url ? (
                                    <button
                                        type="button"
                                        disabled={busy || !!selectedJob}
                                        aria-busy={!!selectedJob}
                                        onClick={() => void onInstallUrl(selected)}
                                        className={cn(
                                            "inline-flex items-center gap-1.5 rounded-md btn-accent px-3 py-1.5 text-xs text-white",
                                            busy && !selectedJob && "opacity-50",
                                        )}
                                    >
                                        {selectedJob ? (
                                            <>
                                                <Loader2
                                                    className="h-3.5 w-3.5 animate-spin"
                                                    aria-hidden="true"
                                                />
                                                {selectedJob.phase === "download"
                                                    ? t("extensions.downloading")
                                                    : t("extensions.installing")}
                                            </>
                                        ) : selected.badge === "update" ? (
                                            t("extensions.update")
                                        ) : (
                                            t("extensions.install")
                                        )}
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
                                <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-2 text-xs">
                                    <span className="text-text-secondary">{t("extensions.uninstallConfirm")}</span>
                                    <label className="flex items-center gap-1.5 text-text-tertiary">
                                        <input
                                            type="checkbox"
                                            checked={purgeData}
                                            onChange={(e) => setPurgeData(e.target.checked)}
                                        />
                                        {t("extensions.purgeData")}
                                    </label>
                                    <div className="flex gap-2">
                                        <button
                                            type="button"
                                            onClick={() => setConfirmUninstall(false)}
                                            className="rounded-md border border-border-divider px-2.5 py-1"
                                        >
                                            {t("extensions.cancel")}
                                        </button>
                                        <button
                                            type="button"
                                            disabled={busy}
                                            onClick={() => void onUninstall()}
                                            className="rounded-md border border-danger-border px-2.5 py-1 text-danger-text disabled:opacity-50"
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
                                        <span
                                            role="img"
                                            title={t(statusLabelKey(selected.installed))}
                                            aria-label={t(statusLabelKey(selected.installed))}
                                            className={cn("h-1.5 w-1.5 rounded-full", statusDot(selected.installed))}
                                        />
                                        {t(statusLabelKey(selected.installed))}
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
