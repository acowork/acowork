import { useState, useEffect, useCallback, useRef } from "react";
import { useGatewayStore, type MigrationSession } from "../../stores/gatewayStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { EmbeddingModelWithStatus, CloudEmbeddingProvider, ActiveCloudEmbeddingProvider, CloudEmbeddingProvidersResponse } from "../../lib/types";
import { cn } from "../../lib/utils";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { ErrorBox } from "../common/ErrorBox";
import { Dropdown } from "../common/Dropdown";
import { fetchEmbeddingModels, downloadEmbeddingModel, selectEmbeddingModel, fetchEmbeddingModelStatus, testEmbeddingModel, deleteEmbeddingModel, startMigration, selectEmbeddingModelWithMigration } from "../../lib/gateway-api";
import { fetchCloudEmbeddingProviders, selectCloudEmbeddingModel, setCloudEmbeddingApiKey, deleteCloudEmbeddingApiKey, testCloudEmbeddingProvider, addCloudEmbeddingProvider } from "../../lib/gateway-api";
import type { EmbeddingTestResponse } from "../../lib/types";
import { Download, Check, Loader2, Cpu, Languages, Zap, CheckCircle2, XCircle, Trash2, Cloud, KeyRound, HardDrive, Plus, RefreshCw } from "lucide-react";
import { Badge, EmptyState, ExpandableRow, ListBox, ListRow } from "../common/list";
import { Tooltip } from "../common/Tooltip";
import { OverlayScrollbar } from "../common/OverlayScrollbar";

export function EmbeddingModelTab() {
    const { t } = useTranslation();
    const status = useGatewayStore((s) => s.status);
    const migrationProgress = useGatewayStore((s) => s.migrationProgress);
    const pollMigrationProgress = useGatewayStore((s) => s.pollMigrationProgress);
    const [models, setModels] = useState<EmbeddingModelWithStatus[]>([]);
    const [activeModelId, setActiveModelId] = useState<string | null>(null);
    const [serviceRunning, setServiceRunning] = useState(false);
    const [loading, setLoading] = useState(false);
    const [downloadingIds, setDownloadingIds] = useState<Set<string>>(new Set());
    const [selectingId, setSelectingId] = useState<string | null>(null);
    const [deletingId, setDeletingId] = useState<string | null>(null);
    const [deleteConfirm, setDeleteConfirm] = useState<{ modelId: string; modelName: string } | null>(null);
    const [downloadProgress, setDownloadProgress] = useState<Record<string, number>>({});
    const [error, setError] = useState<string | null>(null);
    const [dimensionConfirm, setDimensionConfirm] = useState<{ modelId: string; message: string } | null>(null);
    const [migrationStarting, setMigrationStarting] = useState(false);
    // The migration session lives in the store, not here: a rebuild outlives
    // this tab, and holding it in `useState` meant that visiting the chat
    // unmounted the panel and the progress list never came back. See
    // `MigrationSession` in gatewayStore.
    const migrationSession = useGatewayStore((s) => s.migrationSession);
    const beginMigrationSession = useGatewayStore((s) => s.beginMigrationSession);
    const markMigrationStarted = useGatewayStore((s) => s.markMigrationStarted);
    const setMigrationSelection = useGatewayStore((s) => s.setMigrationSelection);
    const clearMigrationSession = useGatewayStore((s) => s.clearMigrationSession);
    const [testing, setTesting] = useState(false);
    const [testResult, setTestResult] = useState<EmbeddingTestResponse | null>(null);
    // Tools-tab level-1 collapsible group shells for Local / Cloud, default open.
    const [localOpen, setLocalOpen] = useState(true);
    const [cloudOpen, setCloudOpen] = useState(true);
    // Service status card — collapsible, default open.
    const [serviceStatusOpen, setServiceStatusOpen] = useState(true);

    // ── Cloud embedding providers (S1-7) ─────────────────────────────
    const [cloudProviders, setCloudProviders] = useState<CloudEmbeddingProvider[]>([]);
    const [cloudActive, setCloudActive] = useState<ActiveCloudEmbeddingProvider | null>(null);
    const [cloudLoading, setCloudLoading] = useState(false);
    const [cloudError, setCloudError] = useState<string | null>(null);
    const [cloudSelecting, setCloudSelecting] = useState<string | null>(null);
    const [keyEditingProvider, setKeyEditingProvider] = useState<string | null>(null);
    const [keyDraft, setKeyDraft] = useState("");
    const [keySaving, setKeySaving] = useState(false);
    const [cloudTesting, setCloudTesting] = useState<string | null>(null);
    // Per-provider test result map — each card renders ONLY its own result.
    // (Was a single global state which caused one provider's failure to bleed
    //  into every other card's UI.)
    const [cloudTestResults, setCloudTestResults] = useState<
        Record<string, EmbeddingTestResponse>
    >({});

    // Add custom cloud embedding provider dialog (S2)
    const [customDialogOpen, setCustomDialogOpen] = useState(false);

    const loadModels = useCallback(async () => {
        setLoading(true);
        setError(null);
        try {
            const resp = await fetchEmbeddingModels();
            setModels(resp.models);
            setActiveModelId(resp.active_model_id);
            setServiceRunning(resp.service_running);
        } catch (e) {
            setError(e instanceof Error ? e.message : "Failed to load models");
        } finally {
            setLoading(false);
        }
    }, []);

    useEffect(() => {
        if (status === "connected") {
            loadModels();
        }
    }, [status, loadModels]);

    useEffect(() => {
        if (status !== "connected" || !serviceRunning || activeModelId) return;
        const timer = setInterval(loadModels, 2000);
        return () => clearInterval(timer);
    }, [status, serviceRunning, activeModelId, loadModels]);

    useEffect(() => {
        const downloading = models.filter((model) => model.status === "downloading");
        const downloadingSet = new Set(downloading.map((model) => model.id));

        setDownloadingIds((prev) => {
            const next = new Set(prev);
            for (const model of downloading) next.add(model.id);
            return next;
        });
        setDownloadProgress((prev) => {
            const next = { ...prev };
            for (const model of downloading) {
                if (next[model.id] == null) next[model.id] = 0;
            }
            for (const model of models) {
                if (model.status !== "downloading" && !downloadingSet.has(model.id)) {
                    delete next[model.id];
                }
            }
            return next;
        });
    }, [models]);

    const handleDownload = useCallback(async (modelId: string, variant?: string) => {
        setDownloadingIds((prev) => new Set(prev).add(modelId));
        setDownloadProgress((prev) => ({ ...prev, [modelId]: 0 }));
        setError(null);
        try {
            await downloadEmbeddingModel(modelId, variant);
            // Fire-and-forget: response is immediate, polling handles progress
        } catch (e) {
            setError(e instanceof Error ? e.message : "Download failed");
            setDownloadingIds((prev) => {
                const next = new Set(prev);
                next.delete(modelId);
                return next;
            });
        }
    }, []);

    // Poll download progress for all in-flight downloads
    const pollingRef = useRef<ReturnType<typeof setInterval> | null>(null);
    useEffect(() => {
        if (downloadingIds.size === 0) {
            if (pollingRef.current) clearInterval(pollingRef.current);
            pollingRef.current = null;
            return;
        }

        let needsRefresh = false;

        const poll = async () => {
            // Snapshot current downloading ids to avoid stale closure
            const ids = Array.from(downloadingIds);

            // Query ALL model statuses in parallel to avoid serial round-trips
            const results = await Promise.allSettled(
                ids.map((id) => fetchEmbeddingModelStatus(id)),
            );

            const completedIds: string[] = [];
            const failedIds: string[] = [];

            results.forEach((result, i) => {
                if (result.status !== "fulfilled") return;
                const resp = result.value;
                const id = ids[i];

                if (resp.status === "downloading") {
                    // Update progress without deleting — avoid flicker
                    setDownloadProgress((prev) => ({ ...prev, [id]: resp.progress ?? 0 }));
                } else if (resp.status === "downloaded" || resp.status === "loaded") {
                    completedIds.push(id);
                } else if (resp.status === "failed") {
                    failedIds.push(id);
                }
            });

            // Report errors for any failed downloads
            for (const id of failedIds) {
                const idx = ids.indexOf(id);
                const result = results[idx];
                if (result.status === "fulfilled" && result.value.status === "failed") {
                    setError(`Download failed: ${result.value.error ?? "unknown error"}`);
                }
            }

            // Remove completed/failed IDs in a single batch to avoid
            // intermediate renders that wipe other models' progress
            if (completedIds.length > 0 || failedIds.length > 0) {
                const removeSet = new Set([...completedIds, ...failedIds]);

                setDownloadingIds((prev) => {
                    const next = new Set(prev);
                    for (const id of removeSet) next.delete(id);
                    return next;
                });

                // Set completed models to 100% before removing so user sees
                // the bar reach the end instead of jumping back to 0
                setDownloadProgress((prev) => {
                    const next = { ...prev };
                    for (const id of completedIds) next[id] = 100;
                    return next;
                });

                // Clean up progress after a short delay so the 100% bar renders
                setTimeout(() => {
                    setDownloadProgress((prev) => {
                        const next = { ...prev };
                        for (const id of removeSet) delete next[id];
                        return next;
                    });
                }, 400);

                needsRefresh = true;
            }

            // Refresh model list ONCE after all statuses are processed
            if (needsRefresh) {
                needsRefresh = false;
                await loadModels();
            }
        };

        poll(); // immediate first poll
        pollingRef.current = setInterval(poll, 2000);

        return () => {
            if (pollingRef.current) clearInterval(pollingRef.current);
            pollingRef.current = null;
        };
    }, [downloadingIds, loadModels]);

    const handleSelect = useCallback(async (modelId: string, force = false) => {
        setSelectingId(modelId);
        setError(null);
        try {
            if (force) {
                // Use migration-aware endpoint for forced selects
                const result = await selectEmbeddingModelWithMigration(modelId, force);
                if ("agents" in result && result.status === "migration_required") {
                    // Dimension changed — show migration agent list. The
                    // session goes to the store, not local state: a rebuild
                    // outlives this tab (the user will go back to chat), and
                    // local state took the whole progress list with it.
                    beginMigrationSession({
                        modelId: result.model_id,
                        oldDimension: result.old_dimension ?? null,
                        newDimension: result.new_dimension,
                        message: result.message,
                        agents: result.agents,
                        selected: new Set(
                            result.agents.filter((a) => a.is_running).map((a) => a.instance_id),
                        ),
                        started: false,
                    });
                    setSelectingId(null);
                    await loadModels();
                    return;
                }
                // Same dimension or simple loaded response
                if (result.status === "loaded" || result.status === "migration_started") {
                    clearMigrationSession();
                }
                await loadModels();
            } else {
                const result = await selectEmbeddingModel(modelId, force);
                if (result.status === "dimension_mismatch") {
                    setDimensionConfirm({ modelId, message: result.message });
                    return;
                }
                await loadModels();
            }
        } catch (e) {
            setError(e instanceof Error ? e.message : "Select failed");
        } finally {
            setSelectingId(null);
        }
    }, [loadModels, beginMigrationSession, clearMigrationSession]);

    const handleDimensionConfirm = useCallback(async () => {
        if (!dimensionConfirm) return;
        setDimensionConfirm(null);
        await handleSelect(dimensionConfirm.modelId, true);
    }, [dimensionConfirm, handleSelect]);

    const handleStartMigration = useCallback(async () => {
        if (!migrationSession || migrationSession.selected.size === 0) return;
        setMigrationStarting(true);
        setError(null);
        try {
            const instanceIds = Array.from(migrationSession.selected);
            await startMigration(migrationSession.modelId, instanceIds);
            markMigrationStarted();
            // Polling itself lives in the effect below, keyed on
            // `migrationSession.started`, so it survives this tab unmounting
            // and resumes when the user comes back.
            await pollMigrationProgress();
        } catch (e) {
            setError(e instanceof Error ? e.message : "Migration start failed");
        } finally {
            setMigrationStarting(false);
        }
    }, [migrationSession, markMigrationStarted, pollMigrationProgress]);

    const handleMigrationCancel = useCallback(() => {
        // Only reachable pre-flight or once the rebuild has finished — the
        // running rebuild offers no cancel, because the Gateway has no way to
        // stop a half-finished re-embed.
        clearMigrationSession();
    }, [clearMigrationSession]);

    const toggleMigrationAgent = useCallback((agentId: string) => {
        if (!migrationSession) return;
        const next = new Set(migrationSession.selected);
        if (next.has(agentId)) {
            next.delete(agentId);
        } else {
            next.add(agentId);
        }
        setMigrationSelection(next);
    }, [migrationSession, setMigrationSelection]);

    // Poll progress while a rebuild is running.
    //
    // Keyed on the store's session rather than a ref, so it restarts when the
    // user returns to this tab: the interval used to be a local ref whose
    // callback set local state, so after one visit to the chat it was polling
    // into a dead component. It also stops of its own accord the moment every
    // agent reports done or failed — the panel stays until the work is
    // actually over, and the user dismisses it.
    useEffect(() => {
        if (!migrationSession?.started) return;
        let cancelled = false;
        const tick = async () => {
            const inProgress = await pollMigrationProgress();
            if (cancelled) return;
            if (!inProgress) await loadModels();
        };
        const timer = setInterval(tick, 2000);
        return () => {
            cancelled = true;
            clearInterval(timer);
        };
    }, [migrationSession?.started, pollMigrationProgress, loadModels]);

    const handleTest = useCallback(async () => {
        setTesting(true);
        setTestResult(null);
        try {
            const result = await testEmbeddingModel();
            setTestResult(result);
        } catch (e) {
            setTestResult({
                success: false,
                error: e instanceof Error ? e.message : "Test failed",
            });
        } finally {
            setTesting(false);
        }
    }, []);

    // ── Cloud embedding handlers (S1-7) ──────────────────────────────
    const loadCloudProviders = useCallback(async () => {
        setCloudLoading(true);
        setCloudError(null);
        try {
            const resp: CloudEmbeddingProvidersResponse = await fetchCloudEmbeddingProviders();
            setCloudProviders(resp.providers);
            setCloudActive(resp.active);
        } catch (e) {
            setCloudError(e instanceof Error ? e.message : "Failed to load cloud providers");
        } finally {
            setCloudLoading(false);
        }
    }, []);

    // Load cloud providers when Gateway connects (separate effect so the
    // `loadCloudProviders` callback is declared before use).
    useEffect(() => {
        if (status === "connected") {
            loadCloudProviders();
        }
    }, [status, loadCloudProviders]);

    const handleCloudSelect = useCallback(
        async (providerId: string, modelId: string) => {
            const key = `${providerId}/${modelId}`;
            setCloudSelecting(key);
            setCloudError(null);
            try {
                await selectCloudEmbeddingModel(providerId, modelId);
                await loadCloudProviders();
            } catch (e) {
                setCloudError(e instanceof Error ? e.message : "Select failed");
            } finally {
                setCloudSelecting(null);
            }
        },
        [loadCloudProviders],
    );

    const handleCloudKeySubmit = useCallback(
        async (providerId: string) => {
            if (!keyDraft.trim()) return;
            setKeySaving(true);
            setCloudError(null);
            try {
                await setCloudEmbeddingApiKey(providerId, keyDraft.trim());
                setKeyEditingProvider(null);
                setKeyDraft("");
                await loadCloudProviders();
                // After the key is persisted, fire a verification ping so
                // the user sees whether the provider accepted it. Backend
                // POST /api/embedding-providers/{id}/test returns
                // `{ ok, dimension, message }`; we map into the frontend
                // EmbeddingTestResponse shape (`success`, `error`) so the
                // existing inline UI can render success / failure.
                setCloudTesting(providerId);
                try {
                    const resp = await testCloudEmbeddingProvider(providerId);
                    setCloudTestResults((prev) => ({
                        ...prev,
                        [providerId]: {
                            success: resp.ok,
                            model_id: resp.model_id,
                            dimension: resp.dimension,
                            error: resp.ok ? null : resp.message,
                        },
                    }));
                } catch (e) {
                    setCloudTestResults((prev) => ({
                        ...prev,
                        [providerId]: {
                            success: false,
                            error: e instanceof Error ? e.message : "Verification failed",
                        },
                    }));
                } finally {
                    setCloudTesting(null);
                }
            } catch (e) {
                setCloudError(e instanceof Error ? e.message : "Failed to save API key");
            } finally {
                setKeySaving(false);
            }
        },
        [keyDraft, loadCloudProviders],
    );

    const handleCloudKeyDelete = useCallback(
        async (providerId: string) => {
            setCloudError(null);
            try {
                await deleteCloudEmbeddingApiKey(providerId);
                await loadCloudProviders();
            } catch (e) {
                setCloudError(e instanceof Error ? e.message : "Failed to remove API key");
            }
        },
        [loadCloudProviders],
    );

    const handleCloudTest = useCallback(async (providerId: string) => {
        setCloudTesting(providerId);
        // Clear only this provider's previous result (don't blow away others')
        setCloudTestResults((prev) => {
            if (!(providerId in prev)) return prev;
            const next = { ...prev };
            delete next[providerId];
            return next;
        });
        try {
            const result = await testCloudEmbeddingProvider(providerId);
            // Map CloudEmbeddingTestResponse → EmbeddingTestResponse so the
            // shared UI shape (success/error) renders uniformly.
            setCloudTestResults((prev) => ({
                ...prev,
                [providerId]: {
                    success: result.ok,
                    model_id: result.model_id,
                    dimension: result.dimension ?? null,
                    error: result.ok ? null : result.message ?? null,
                },
            }));
        } catch (e) {
            setCloudTestResults((prev) => ({
                ...prev,
                [providerId]: {
                    success: false,
                    error: e instanceof Error ? e.message : "Test failed",
                },
            }));
        } finally {
            setCloudTesting(null);
        }
    }, []);

    const handleDelete = useCallback(async (modelId: string) => {
        setDeletingId(modelId);
        setError(null);
        try {
            await deleteEmbeddingModel(modelId);
            await loadModels();
        } catch (e) {
            setError(e instanceof Error ? e.message : "Delete failed");
        } finally {
            setDeletingId(null);
        }
    }, [loadModels]);

    const handleDeleteConfirm = useCallback(async () => {
        if (!deleteConfirm) return;
        const id = deleteConfirm.modelId;
        setDeleteConfirm(null);
        await handleDelete(id);
    }, [deleteConfirm, handleDelete]);

    if (status !== "connected") {
        return (
            <div className="max-w-lg">
                <p className="text-xs text-text-tertiary">{t("embedding.connectToManage")}</p>
            </div>
        );
    }

    return (
        <div className="max-w-2xl space-y-4">
            {/* Service status — Tools-tab level-1 collapsible card.
                Header holds the title only; status / active model / test
                action all live in the fold body to keep the header
                chrome consistent with sibling cards. */}
            <ListBox dividers={false}>
                <ExpandableRow
                    open={serviceStatusOpen}
                    onToggle={() => setServiceStatusOpen((v) => !v)}
                    title={t("embedding.serviceStatus")}
                    ariaLabel={t("embedding.serviceStatus")}
                    bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
                >
                    <div className="space-y-2">
                        <div className="flex items-center gap-2 text-xs">
                            <span className="text-text-tertiary ">{t("embedding.status")}</span>
                            <span
                                className={cn(
                                    "h-2 w-2 rounded-full",
                                    serviceRunning ? "bg-[var(--color-accent)]" : "bg-zinc-400",
                                )}
                            />
                            <span className={cn(
                                serviceRunning
                                    ? "text-[var(--color-accent)]"
                                    : "text-text-tertiary "
                            )}>
                                {serviceRunning ? t("embedding.running") : t("embedding.stopped")}
                            </span>
                        </div>
                        {activeModelId && serviceRunning && (
                            <div className="flex items-center gap-2 text-xs">
                                <span className="text-text-tertiary ">{t("embedding.activeModel")}</span>
                                <span className="font-medium">{activeModelId}</span>
                            </div>
                        )}
                        {serviceRunning && activeModelId && (
                            <div className="flex items-center gap-2">
                                <button
                                    onClick={handleTest}
                                    disabled={testing}
                                    className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
                                >
                                    {testing ? (
                                        <Loader2 className="h-3 w-3 animate-spin" />
                                    ) : (
                                        <Zap className="h-3 w-3" />
                                    )}
                                    {testing ? t("embedding.testing") : t("embedding.test")}
                                </button>
                                {/* Test result inline */}
                                {testResult && (
                                    <span className="flex items-center gap-1 text-11">
                                        {testResult.success ? (
                                            <>
                                                <CheckCircle2 className="h-3 w-3 text-green-500" />
                                                <span className="text-green-600 dark:text-green-400">
                                                    {t("embedding.testPassed")}
                                                    {testResult.dimension && ` (${testResult.dimension}d)`}
                                                    {testResult.latency_ms != null && ` ${testResult.latency_ms}ms`}
                                                </span>
                                            </>
                                        ) : (
                                            <>
                                                <XCircle className="h-3 w-3 text-red-500" />
                                                <span className="text-red-600 dark:text-red-400">
                                                    {testResult.error ?? t("embedding.testFailed")}
                                                </span>
                                            </>
                                        )}
                                    </span>
                                )}
                            </div>
                        )}
                    </div>
                </ExpandableRow>
            </ListBox>

            {/* Error message */}
            {error && (
                <ErrorBox
                    message={error}
                    onClose={() => setError(null)}
                />
            )}

            {/* Migration panel — rendered from the store-held session, so it is
                still here after a round trip to the chat. */}
            {migrationSession && (
                <MigrationPanel
                    session={migrationSession}
                    migrationStarting={migrationStarting}
                    migrationProgress={migrationProgress}
                    onToggleAgent={toggleMigrationAgent}
                    onStartMigration={handleStartMigration}
                    onCancel={handleMigrationCancel}
                />
            )}

            {/* Local models — Tools-tab level-1 collapsible card: title
                + count in the header, refresh in the trailing slot
                (same pattern as the LSP card). */}
            <ListBox dividers={false}>
                <ExpandableRow
                    open={localOpen}
                    onToggle={() => setLocalOpen((v) => !v)}
                    title={
                        <span className="inline-flex items-center gap-1.5">
                            <HardDrive className="h-3.5 w-3.5" />
                            {t("embedding.localModels", { count: models.length })}
                        </span>
                    }
                    ariaLabel={t("embedding.localModels", { count: models.length })}
                    trailing={
                        <span onClick={(e) => e.stopPropagation()}>
                            <Tooltip
                                content={loading ? t("embedding.loading") : t("embedding.refresh")}
                                variant="plain"
                            >
                                <button
                                    aria-label={loading ? t("embedding.loading") : t("embedding.refresh")}
                                    onClick={loadModels}
                                    disabled={loading}
                                    className="inline-flex items-center justify-center rounded h-6 w-6 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 disabled:opacity-60 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 transition-colors"
                                >
                                    {loading ? (
                                        <Loader2 className="h-3.5 w-3.5 animate-spin" />
                                    ) : (
                                        <RefreshCw className="h-3.5 w-3.5" />
                                    )}
                                </button>
                            </Tooltip>
                        </span>
                    }
                    bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
                >
                    {loading && models.length === 0 ? (
                        <EmptyState message={t("embedding.loading")} />
                    ) : (
                        // Plain ListBox — the panel-inset body hands each
                        // model its own row with a hairline divider, and
                        // ModelCard drops its old nested-card chrome.
                        <ListBox variant="plain">
                            {models.length === 0 ? (
                                <EmptyState message={t("embedding.noModels")} />
                            ) : (
                                models.map((model) => (
                                    <ModelCard
                                        key={model.id}
                                        model={model}
                                        isActive={model.id === activeModelId}
                                        isDownloading={downloadingIds.has(model.id)}
                                        isSelecting={selectingId === model.id}
                                        isDeleting={deletingId === model.id}
                                        progress={downloadProgress[model.id]}
                                        onDownload={handleDownload}
                                        onSelect={() => handleSelect(model.id)}
                                        onDelete={() => setDeleteConfirm({ modelId: model.id, modelName: model.name })}
                                    />
                                ))
                            )}
                        </ListBox>
                    )}
                </ExpandableRow>
            </ListBox>

            {/* Cloud Embedding Providers (S1-7) — Tools-tab level-1
                collapsible card; add / refresh live in the header
                trailing slot (same pattern as the LSP / MCP cards). */}
            <ListBox dividers={false}>
                <ExpandableRow
                    open={cloudOpen}
                    onToggle={() => setCloudOpen((v) => !v)}
                    title={
                        <span className="inline-flex items-center gap-1.5">
                            <Cloud className="h-3.5 w-3.5" />
                            {t("embedding.cloudProviders", { count: cloudProviders.length })}
                        </span>
                    }
                    ariaLabel={t("embedding.cloudProviders", { count: cloudProviders.length })}
                    trailing={
                        <span
                            onClick={(e) => e.stopPropagation()}
                            className="flex items-center gap-1"
                        >
                            <Tooltip content={t("embedding.addCustomProvider")} variant="plain">
                                <button
                                    aria-label={t("embedding.addCustomProvider")}
                                    onClick={() => setCustomDialogOpen(true)}
                                    className="inline-flex items-center justify-center rounded h-6 w-6 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 transition-colors"
                                >
                                    <Plus className="h-3.5 w-3.5" />
                                </button>
                            </Tooltip>
                            <Tooltip
                                content={cloudLoading ? t("embedding.loading") : t("embedding.refresh")}
                                variant="plain"
                            >
                                <button
                                    aria-label={cloudLoading ? t("embedding.loading") : t("embedding.refresh")}
                                    onClick={loadCloudProviders}
                                    disabled={cloudLoading}
                                    className="inline-flex items-center justify-center rounded h-6 w-6 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 disabled:opacity-60 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 transition-colors"
                                >
                                    {cloudLoading ? (
                                        <Loader2 className="h-3.5 w-3.5 animate-spin" />
                                    ) : (
                                        <RefreshCw className="h-3.5 w-3.5" />
                                    )}
                                </button>
                            </Tooltip>
                        </span>
                    }
                    bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
                >
                    {/* Active cloud selection summary — inline status row
                        (no nested card box; the surrounding ExpandableRow
                        body is the only surface here). */}
                    {cloudActive && (
                        <div className="mx-3 mt-3 flex items-center gap-2 text-11">
                            <Badge tone="accent">{t("embedding.cloudActive")}</Badge>
                            <span className="font-medium">
                                {cloudActive.provider_id}/{cloudActive.model_id}
                            </span>
                            <span className="text-text-tertiary">· {cloudActive.dimension}d</span>
                            {!cloudActive.has_api_key && (
                                <span className="text-amber-600 dark:text-amber-400">
                                    · {t("embedding.apiKeyMissing")}
                                </span>
                            )}
                        </div>
                    )}

                    {/* Cloud error inline (kept as an emphasised box —
                        errors need to read at a glance). */}
                    {cloudError && (
                        <div className="mx-3 mt-2 rounded border border-red-200 bg-red-50 px-2 py-1 text-11 text-red-700 dark:border-red-800 dark:bg-red-950 dark:text-red-300">
                            {cloudError}
                        </div>
                    )}

                    {cloudProviders.length === 0 ? (
                        <EmptyState message={t("embedding.cloudNoProviders")} />
                    ) : (
                        // Plain ListBox — the parent ExpandableRow's
                        // panel-inset body hands each provider a row, and
                        // CloudProviderCard drops its old nested-card
                        // chrome.
                        <ListBox variant="plain">
                            {cloudProviders.map((provider) => (
                                <CloudProviderCard
                                    key={provider.id}
                                    provider={provider}
                                    active={cloudActive?.provider_id === provider.id ? cloudActive : null}
                                    testing={cloudTesting === provider.id}
                                    testResult={
                                        // Hide result while testing this card OR while editing
                                        // its key. Otherwise render this provider's own result
                                        // (was previously a single global state — caused
                                        //  one card's failure to bleed into every other card).
                                        cloudTesting === provider.id || keyEditingProvider === provider.id
                                            ? null
                                            : cloudTestResults[provider.id] ?? null
                                    }
                                    selectingModelId={cloudSelecting?.startsWith(`${provider.id}/`)
                                        ? cloudSelecting.split("/", 2)[1] ?? null
                                        : null}
                                    keyEditing={keyEditingProvider === provider.id}
                                    keyDraft={keyDraft}
                                    keySaving={keySaving}
                                    onChangeKeyDraft={setKeyDraft}
                                    onStartKeyEdit={() => {
                                        setKeyEditingProvider(provider.id);
                                        setKeyDraft("");
                                        // Clear only this provider's previous test result
                                        setCloudTestResults((prev) => {
                                            if (!(provider.id in prev)) return prev;
                                            const next = { ...prev };
                                            delete next[provider.id];
                                            return next;
                                        });
                                    }}
                                    onCancelKeyEdit={() => {
                                        setKeyEditingProvider(null);
                                        setKeyDraft("");
                                    }}
                                    onSubmitKey={() => handleCloudKeySubmit(provider.id)}
                                    onDeleteKey={() => handleCloudKeyDelete(provider.id)}
                                    onTest={() => handleCloudTest(provider.id)}
                                    onSelectModel={(modelId) => handleCloudSelect(provider.id, modelId)}
                                />
                            ))}
                        </ListBox>
                    )}
                </ExpandableRow>
            </ListBox>

            {/* Add custom cloud embedding provider dialog (S2) */}
            <AddCustomEmbeddingProviderDialog
                open={customDialogOpen}
                onClose={() => setCustomDialogOpen(false)}
                onSaved={async () => {
                    setCustomDialogOpen(false);
                    await loadCloudProviders();
                }}
                existingIds={cloudProviders.map((p) => p.id)}
            />

            {/* Dimension mismatch confirmation dialog */}
            {dimensionConfirm && (
                <ConfirmDialog
                    open={true}
                    title={t("embedding.dimensionChange")}
                    message={dimensionConfirm.message}
                    confirmLabel={t("embedding.confirmSwitch")}
                    destructive
                    onConfirm={handleDimensionConfirm}
                    onCancel={() => {
                        setDimensionConfirm(null);
                        setSelectingId(null);
                    }}
                />
            )}

            {/* Delete model confirmation dialog */}
            {deleteConfirm && (
                <ConfirmDialog
                    open={true}
                    title={t("embedding.deleteConfirmTitle")}
                    message={t("embedding.deleteConfirmMessage", { name: deleteConfirm.modelName })}
                    confirmLabel={t("embedding.deleteConfirm")}
                    destructive
                    onConfirm={handleDeleteConfirm}
                    onCancel={() => setDeleteConfirm(null)}
                />
            )}
        </div>
    );
}

/** Migration panel — the agents that must re-embed for a new model.
 *
 * Standard settings-card chrome (ListBox + ListRow), the same grammar as the
 * sibling service-status / local-models cards. It was a hand-rolled
 * `border-amber-200 bg-amber-50` box, which read as an alert that never
 * cleared — the tint had nothing to do with severity, only with "this box needs
 * a rebuild".
 *
 * Accent is now rationed: the only accent-coloured pixels are the primary
 * action and the in-flight progress bar, the two things the user is waiting on.
 * Row state is carried by Badge tones, which read as status rather than as a
 * background wash. Interaction is unchanged — still checkbox-per-row.
 */
function MigrationPanel({
    session,
    migrationStarting,
    migrationProgress,
    onToggleAgent,
    onStartMigration,
    onCancel,
}: {
    session: MigrationSession;
    migrationStarting: boolean;
    migrationProgress: Record<string, { progress?: { rebuilt: number; total_scanned: number; errors: number; phase: string; label: string } | null; done: boolean; error?: string | null }>;
    onToggleAgent: (agentId: string) => void;
    onStartMigration: () => void;
    onCancel: () => void;
}) {
    const { t } = useTranslation();
    const { started } = session;
    const allDone = session.agents
        .filter((a) => session.selected.has(a.instance_id))
        .every((a) => {
            const p = migrationProgress[a.instance_id];
            return p?.done;
        });

    return (
        <ListBox dividers={false}>
            <ExpandableRow
                open
                // Collapsing means "cancel" here, which is what the old panel's
                // Cancel button did, so the chevron takes over that role — but
                // only before the rebuild starts. Once a job is in flight the
                // header must not hide live progress over work that keeps
                // running, so the title switches to the in-progress wording and
                // the chevron goes inert rather than lying about being clickable.
                onToggle={started ? () => {} : onCancel}
                title={started
                    ? t("embedding.migrationInProgress")
                    : t("embedding.migrationRequired")}
                ariaLabel={t("embedding.migrationRequired")}
                bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
            >
                <p className="mb-3 text-11 text-text-secondary">
                    {session.message}
                    {` (Old: ${session.oldDimension ?? "?"}, New: ${session.newDimension})`}
                </p>

                {/* Agent list */}
                <ListBox variant="plain" className="mb-3">
                    {session.agents.map((agent) => {
                        const isSelected = session.selected.has(agent.instance_id);
                        const prog = migrationProgress[agent.instance_id];
                        const pct = prog?.progress?.total_scanned
                            ? Math.round((prog.progress.rebuilt / prog.progress.total_scanned) * 100)
                            : 0;
                        const isDone = prog?.done;
                        const hasError = prog?.error;
                        const inFlight = started && !!prog && !isDone && !hasError;

                        return (
                            <ListRow
                                key={agent.instance_id}
                                disabled={!agent.is_running && !started}
                                trailing={
                                    <>
                                        {!agent.is_running ? (
                                            <Badge tone="neutral">{t("embedding.notRunning")}</Badge>
                                        ) : isDone ? (
                                            <Badge tone="success">{t("embedding.done")} ✓</Badge>
                                        ) : hasError ? (
                                            <Badge tone="danger">{t("embedding.failed")} ✗</Badge>
                                        ) : inFlight ? (
                                            <Badge tone="accent">{pct}%</Badge>
                                        ) : (
                                            <Badge tone="neutral">{t("embedding.pending")}</Badge>
                                        )}

                                        {/* Accent because this bar is the one thing the
                                            user is actively waiting on. */}
                                        {inFlight && (
                                            <div className="flex w-24 items-center gap-1">
                                                <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
                                                    <div
                                                        className="h-full rounded-full bg-[var(--color-accent)] transition-all"
                                                        style={{ width: `${pct}%` }}
                                                    />
                                                </div>
                                                <span className="text-10 tabular-nums text-text-tertiary">
                                                    {prog?.progress?.rebuilt ?? 0}/{prog?.progress?.total_scanned ?? "?"}
                                                </span>
                                            </div>
                                        )}
                                    </>
                                }
                            >
                                {/* Checkbox is the only control, as before — a
                                 * restyle must not change who can be clicked. */}
                                <span className="flex items-center gap-2 text-xs">
                                    {!started && (
                                        <input
                                            type="checkbox"
                                            checked={isSelected}
                                            disabled={!agent.is_running}
                                            onChange={() => onToggleAgent(agent.instance_id)}
                                            className="h-3.5 w-3.5"
                                        />
                                    )}
                                    {/* ADR-073: backend currently fills `name` with the
                                     * package `agent_id` (reverse-domain) — fine for display.
                                     * Fallback is a SHORT instance id (first 8 hex chars,
                                     * mirroring acowork_core::AgentInstanceId::short()) so
                                     * we never leak a full 36-char UUID into the UI when
                                     * `name` is unexpectedly empty. */}
                                    <span className="min-w-[100px] truncate font-medium">
                                        {agent.name
                                            || agent.instance_id.slice(0, 8)
                                            || agent.instance_id}
                                    </span>
                                </span>
                            </ListRow>
                        );
                    })}
                </ListBox>

            {/* Actions */}
            <div className="flex items-center gap-2">
                {!started ? (
                    <>
                        <button
                            onClick={onStartMigration}
                            disabled={migrationStarting || session.selected.size === 0}
                            className="rounded btn-accent px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
                        >
                            {migrationStarting ? t("embedding.starting") : t("embedding.startMigration")}
                        </button>
                        <button
                            onClick={onCancel}
                            className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium"
                        >
                            Cancel
                        </button>
                    </>
                ) : allDone ? (
                    <button
                        onClick={onCancel}
                        className="rounded btn-accent px-3 py-[var(--ui-btn-py)] text-xs font-medium"
                    >
                        {t("embedding.migrationComplete")}
                    </button>
                ) : (
                    <span className="flex items-center gap-1.5 text-xs text-text-tertiary">
                        <Loader2 className="h-3 w-3 animate-spin" />
                        {t("embedding.migratingAgents")}
                    </span>
                )}
            </div>
            </ExpandableRow>
        </ListBox>
    );
}

/** Variant display label */
const VARIANT_LABELS: Record<string, string> = {
    fp32: "FP32",
    fp16: "FP16",
    int8: "INT8",
};

function ModelCard({
    model,
    isActive,
    isDownloading,
    isSelecting,
    isDeleting,
    progress,
    onDownload,
    onSelect,
    onDelete,
}: {
    model: EmbeddingModelWithStatus;
    isActive: boolean;
    isDownloading: boolean;
    isSelecting: boolean;
    isDeleting: boolean;
    progress?: number;
    onDownload: (modelId: string, variant?: string) => void;
    onSelect: () => void;
    onDelete: () => void;
}) {
    const { t } = useTranslation();
    const variants = model.onnx_variants ? Object.keys(model.onnx_variants) : [];
    const hasVariants = variants.length > 1;
    const [selectedVariant, setSelectedVariant] = useState<string>(
        variants.includes("fp16") ? "fp16" : (variants[0] ?? "fp32"),
    );

    const isBusy = isDownloading || isSelecting || isDeleting;

    return (
        <div
            // Row inside a plain ListBox (parent ExpandableRow's inset
            // body) — no own border/bg; the ListBox hairline divider
            // separates rows. Active state keeps an accent wash so the
            // currently selected model still pops.
            className={cn(
                "px-3 py-3 transition-colors",
                isActive && "bg-[var(--color-accent)]/5",
            )}
        >
            {/* Header: name + badges */}
            <div className="flex items-start justify-between gap-2">
                <div className="min-w-0">
                    <div className="flex items-center gap-2">
                        <span className="text-xs font-semibold">{model.name}</span>
                        {model.recommended && (
                            <span
                                className="rounded px-1.5 py-0.5 text-10 font-medium"
                                style={{ backgroundColor: "color-mix(in srgb, var(--color-accent) 15%, transparent)", color: "var(--color-accent)" }}
                            >
                                {t("embedding.recommended")}
                            </span>
                        )}
                        {isActive && (
                            <span
                                className="rounded px-1.5 py-0.5 text-10 font-medium"
                                style={{ backgroundColor: "color-mix(in srgb, var(--color-accent) 15%, transparent)", color: "var(--color-accent)" }}
                            >
                                {t("embedding.active")}
                            </span>
                        )}
                    </div>
                    <p className="mt-0.5 text-10 text-text-tertiary ">{model.id}</p>
                </div>
                <div className="flex shrink-0 items-center gap-1.5">
                    {/* Variant selector — show when downloading and model has multiple variants */}
                    {hasVariants && (model.status === "not_downloaded" || model.status === "service_not_running" || model.status === "unknown" || model.status === "downloading" || model.status.startsWith("failed")) && (
                        <Dropdown
                            size="small"
                            value={selectedVariant}
                            onChange={setSelectedVariant}
                            disabled={isBusy}
                            options={variants.map((v) => ({
                                value: v,
                                label: VARIANT_LABELS[v] ?? v.toUpperCase(),
                            }))}
                        />
                    )}
                    {/* Download button — show when not downloaded/loaded or unknown */}
                    {(model.status === "not_downloaded" || model.status === "service_not_running" || model.status === "unknown" || model.status === "downloading" || model.status.startsWith("failed")) && (
                        <button
                            onClick={() => onDownload(model.id, hasVariants ? selectedVariant : undefined)}
                            disabled={isBusy || !model.id}
                            className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
                        >
                            {isDownloading ? (
                                <Loader2 className="h-3 w-3 animate-spin" />
                            ) : (
                                <Download className="h-3 w-3" />
                            )}
                            {isDownloading ? t("embedding.downloading") : t("embedding.download")}
                        </button>
                    )}
                    {/* Select button — show when downloaded but not active */}
                    {!isActive && (model.status === "downloaded" || model.status === "loaded") && (
                        <button
                            onClick={onSelect}
                            disabled={isBusy}
                            className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
                        >
                            {isSelecting ? (
                                <Loader2 className="h-3 w-3 animate-spin" />
                            ) : (
                                <Check className="h-3 w-3" />
                            )}
                            {isSelecting ? t("embedding.switching") : t("embedding.switchTo")}
                        </button>
                    )}
                    {/* Delete button — show when downloaded and not active */}
                    {!isActive && (model.status === "downloaded" || model.status === "loaded") && (
                        <button
                            onClick={onDelete}
                            disabled={isBusy}
                            className="group/del inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
                        >
                            {isDeleting ? (
                                <Loader2 className="h-3 w-3 animate-spin" />
                            ) : (
                                <Trash2 className="h-3 w-3 transition-colors group-hover/del:text-[var(--color-accent)]" />
                            )}
                            <span className="transition-colors group-hover/del:text-[var(--color-accent)]">
                                {isDeleting ? t("embedding.deleting") : t("embedding.delete")}
                            </span>
                        </button>
                    )}
                </div>
            </div>

            {/* Download progress bar */}
            {isDownloading && typeof progress === "number" && (
                <div className="mt-2 space-y-1">
                    <div className="h-1.5 w-full overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
                        <div
                            className="h-full rounded-full transition-all duration-300"
                            style={{
                                width: `${Math.max(progress, 2)}%`,
                                backgroundColor: "var(--color-accent)",
                            }}
                        />
                    </div>
                    <p className="text-right text-10 text-text-tertiary ">
                        {progress > 0 ? `${progress}%` : t("embedding.connecting")}
                    </p>
                </div>
            )}

            {/* Meta info */}
            <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-10 text-text-tertiary ">
                <span className="inline-flex items-center gap-1">
                    <Cpu className="h-3 w-3" />
                    {model.dimension}d
                </span>
                <span>{model.size_mb} MB</span>
                <span>{model.max_tokens} tokens</span>
                {model.languages.length > 0 && (
                    <span className="inline-flex items-center gap-1">
                        <Languages className="h-3 w-3" />
                        {model.languages.join(", ")}
                    </span>
                )}
                {hasVariants && (
                    <span className="text-text-tertiary">
                        {t("embedding.variants")}: {variants.map((v) => VARIANT_LABELS[v] ?? v.toUpperCase()).join("/")}
                    </span>
                )}
            </div>
        </div>
    );
}

// ── Cloud Provider Card (S1-7) ──────────────────────────────────────────

interface CloudProviderCardProps {
    provider: CloudEmbeddingProvider;
    active: ActiveCloudEmbeddingProvider | null;
    testing: boolean;
    testResult: EmbeddingTestResponse | null;
    selectingModelId: string | null;
    keyEditing: boolean;
    keyDraft: string;
    keySaving: boolean;
    onChangeKeyDraft: (s: string) => void;
    onStartKeyEdit: () => void;
    onCancelKeyEdit: () => void;
    onSubmitKey: () => void;
    onDeleteKey: () => void;
    onTest: () => void;
    onSelectModel: (modelId: string) => void;
}

function CloudProviderCard({
    provider,
    active,
    testing,
    testResult,
    selectingModelId,
    keyEditing,
    keyDraft,
    keySaving,
    onChangeKeyDraft,
    onStartKeyEdit,
    onCancelKeyEdit,
    onSubmitKey,
    onDeleteKey,
    onTest,
    onSelectModel,
}: CloudProviderCardProps) {
    const { t } = useTranslation();
    // Each provider card must reflect its OWN key state, not just the
    // active selection's. Backend returns `has_api_key` per-provider on
    // `GET /api/embedding-providers`; fall back to the active selection
    // (only valid when this card IS the active provider) for safety.
    const hasKey = provider.has_api_key ?? active?.has_api_key ?? false;
    const isActiveProvider = active?.provider_id === provider.id;

    return (
        // Row inside a plain ListBox (parent ExpandableRow's inset body)
        // — no own border/bg; the ListBox hairline divider separates rows.
        <div className="px-3 py-3">
            {/* Header: name + api + key state */}
            <div className="mb-2 flex items-center justify-between gap-2">
                <div className="min-w-0">
                    <div className="flex items-center gap-2">
                        <h3 className="text-xs font-medium">{provider.name}</h3>
                        <span className="text-10 text-text-tertiary">({provider.id})</span>
                    </div>
                    <p className="mt-0.5 truncate font-mono text-10 text-text-tertiary ">
                        {provider.api}
                    </p>
                </div>
                <div className="flex shrink-0 items-center gap-1.5">
                    {!keyEditing ? (
                        hasKey ? (
                            <>
                                <span className="inline-flex items-center gap-1 rounded bg-green-50 px-1.5 py-0.5 text-10 font-medium text-green-700 dark:bg-green-950 dark:text-green-300">
                                    <CheckCircle2 className="h-2.5 w-2.5" />
                                    Key
                                </span>
                                <button
                                    onClick={onTest}
                                    disabled={testing}
                                    className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
                                >
                                    {testing ? (
                                        <Loader2 className="h-3 w-3 animate-spin" />
                                    ) : (
                                        <Zap className="h-3 w-3" />
                                    )}
                                    {testing ? t("embedding.testing") : t("embedding.test")}
                                </button>
                                <button
                                    onClick={onStartKeyEdit}
                                    className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium"
                                >
                                    {t("embedding.changeKey")}
                                </button>
                                <button
                                    onClick={onDeleteKey}
                                    className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium"
                                >
                                    {t("embedding.deleteKey")}
                                </button>
                            </>
                        ) : (
                            <button
                                onClick={onStartKeyEdit}
                                className="inline-flex items-center gap-1 rounded btn-accent px-2 py-1 text-11 font-medium"
                            >
                                <KeyRound className="h-3 w-3" />
                                {t("embedding.setApiKey")}
                            </button>
                        )
                    ) : (
                        <div className="flex items-center gap-1.5">
                            <input
                                type="password"
                                value={keyDraft}
                                onChange={(e) => onChangeKeyDraft(e.target.value)}
                                placeholder={provider.env[0] ?? "API Key"}
                                className="w-44 rounded-md border border-zinc-300 px-2 py-1 text-11 dark:border-zinc-600 dark:bg-zinc-800"
                                autoFocus
                                onKeyDown={(e) => {
                                    if (e.key === "Enter") onSubmitKey();
                                    if (e.key === "Escape") onCancelKeyEdit();
                                }}
                            />
                            <button
                                onClick={onSubmitKey}
                                disabled={keySaving || !keyDraft.trim()}
                                className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
                            >
                                {keySaving ? (
                                    <Loader2 className="h-3 w-3 animate-spin" />
                                ) : (
                                    <Check className="h-3 w-3" />
                                )}
                                {t("embedding.save")}
                            </button>
                            <button
                                onClick={onCancelKeyEdit}
                                disabled={keySaving}
                                className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
                            >
                                {t("embedding.cancel")}
                            </button>
                        </div>
                    )}
                </div>
            </div>

            {/* Test result inline */}
            {testResult && (
                <div className="mb-2 flex items-center gap-1 text-11">
                    {testResult.success ? (
                        <>
                            <CheckCircle2 className="h-3 w-3 text-green-500" />
                            <span className="text-green-600 dark:text-green-400">
                                {t("embedding.testPassed")}
                                {testResult.dimension && ` (${testResult.dimension}d)`}
                                {testResult.latency_ms != null && ` ${testResult.latency_ms}ms`}
                            </span>
                        </>
                    ) : (
                        <>
                            <XCircle className="h-3 w-3 text-red-500" />
                            <span className="text-red-600 dark:text-red-400">
                                {testResult.error ?? t("embedding.testFailed")}
                            </span>
                        </>
                    )}
                </div>
            )}

            {/* Model list */}
            <div className="space-y-1">
                {Object.values(provider.models).map((m) => {
                    const isActiveModel = isActiveProvider && active?.model_id === m.id;
                    const isSelecting = selectingModelId === m.id;
                    return (
                        <div
                            key={m.id}
                            className="flex items-center justify-between rounded border border-zinc-100 px-2 py-1.5 text-11 dark:border-zinc-800"
                        >
                            <div className="flex min-w-0 items-center gap-2">
                                <span className="font-medium">{m.name || m.id}</span>
                                <span className="font-mono text-10 text-text-tertiary">{m.id}</span>
                                <span className="text-text-tertiary">· {m.dimensions}d</span>
                                {m.context_length && (
                                    <span className="text-text-tertiary">· {m.context_length} ctx</span>
                                )}
                                {isActiveModel && (
                                    <span
                                        className="rounded px-1.5 py-0.5 text-10 font-medium"
                                        style={{
                                            backgroundColor:
                                                "color-mix(in srgb, var(--color-accent) 15%, transparent)",
                                            color: "var(--color-accent)",
                                        }}
                                    >
                                        {t("embedding.active")}
                                    </span>
                                )}
                            </div>
                            {!isActiveModel && (
                                <button
                                    onClick={() => onSelectModel(m.id)}
                                    disabled={isSelecting || !hasKey}
                                    title={!hasKey ? t("embedding.apiKeyRequired") : undefined}
                                    className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
                                >
                                    {isSelecting ? (
                                        <Loader2 className="h-3 w-3 animate-spin" />
                                    ) : (
                                        <Check className="h-3 w-3" />
                                    )}
                                        {isSelecting ? t("embedding.switching") : t("embedding.switchTo")}
                                    </button>
                                )}
                            </div>
                        );
                    })}
                </div>
        </div>
    );
}

// ── Add custom cloud embedding provider dialog (S2) ─────────────────

interface CustomProviderModelDraft {
    id: string;
    name: string;
    dimensions: string;
    context_length: string;
}

const EMPTY_MODEL_DRAFT: CustomProviderModelDraft = {
    id: "",
    name: "",
    dimensions: "",
    context_length: "",
};

interface AddCustomEmbeddingProviderDialogProps {
    open: boolean;
    onClose: () => void;
    onSaved: () => void | Promise<void>;
    existingIds: string[];
}

function AddCustomEmbeddingProviderDialog({
    open,
    onClose,
    onSaved,
    existingIds,
}: AddCustomEmbeddingProviderDialogProps) {
    const { t } = useTranslation();
    const [name, setName] = useState("");
    const [id, setId] = useState("");
    const [api, setApi] = useState("");
    const [apiKey, setApiKey] = useState("");
    const [models, setModels] = useState<CustomProviderModelDraft[]>([{ ...EMPTY_MODEL_DRAFT }]);
    const [saving, setSaving] = useState(false);
    const [error, setError] = useState<string | null>(null);

    if (!open) return null;

    const slugifyProviderId = (raw: string): string => {
        return (
            "custom-" +
            raw
                .toLowerCase()
                .replace(/[^a-z0-9]+/g, "-")
                .replace(/^-|-$/g, "")
        );
    };

    const handleNameChange = (v: string) => {
        setName(v);
        setId(slugifyProviderId(v));
    };

    const updateModel = (idx: number, patch: Partial<CustomProviderModelDraft>) => {
        setModels((prev) => prev.map((m, i) => (i === idx ? { ...m, ...patch } : m)));
    };

    const addModel = () => {
        setModels((prev) => [...prev, { ...EMPTY_MODEL_DRAFT }]);
    };

    const removeModel = (idx: number) => {
        setModels((prev) => prev.filter((_, i) => i !== idx));
    };

    const reset = () => {
        setName("");
        setId("");
        setApi("");
        setApiKey("");
        setModels([{ ...EMPTY_MODEL_DRAFT }]);
        setError(null);
        setSaving(false);
    };

    const handleClose = () => {
        reset();
        onClose();
    };

    const handleSave = async () => {
        setError(null);
        const trimmedName = name.trim();
        const trimmedId = id.trim();
        const trimmedApi = api.trim();
        if (!trimmedName) {
            setError(t("embedding.customProviderNameRequired"));
            return;
        }
        if (!trimmedId) {
            setError(t("embedding.customProviderIdRequired"));
            return;
        }
        if (existingIds.includes(trimmedId)) {
            setError(t("embedding.duplicateProviderId", { id: trimmedId }));
            return;
        }
        if (!trimmedApi || !(trimmedApi.startsWith("http://") || trimmedApi.startsWith("https://"))) {
            setError(t("embedding.customBaseUrlRequired"));
            return;
        }
        if (models.length === 0) {
            setError(t("embedding.customModelsRequired"));
            return;
        }
        const modelsMap: Record<string, { id: string; name: string; dimensions: number; context_length?: number | null; embedding_modalities?: string[] }> = {};
        for (const m of models) {
            const mid = m.id.trim();
            const mname = m.name.trim();
            const dim = parseInt(m.dimensions.trim(), 10);
            if (!mid) {
                setError(t("embedding.customModelIdRequired"));
                return;
            }
            if (!mid) continue;
            if (modelsMap[mid]) {
                setError(t("embedding.duplicateModelId", { id: mid }));
                return;
            }
            if (!Number.isFinite(dim) || dim <= 0) {
                setError(t("embedding.dimensionMustBePositive", { id: mid }));
                return;
            }
            const ctxStr = m.context_length.trim();
            const ctx = ctxStr ? parseInt(ctxStr, 10) : null;
            modelsMap[mid] = {
                id: mid,
                name: mname || mid,
                dimensions: dim,
                context_length: ctx && Number.isFinite(ctx) && ctx > 0 ? ctx : null,
                embedding_modalities: ["text"],
            };
        }

        setSaving(true);
        try {
            await addCloudEmbeddingProvider({
                id: trimmedId,
                name: trimmedName,
                api: trimmedApi,
                models: modelsMap,
                api_key: apiKey.trim() || undefined,
            });
            await onSaved();
            reset();
        } catch (e: any) {
            setError(e?.message || String(e));
        } finally {
            setSaving(false);
        }
    };

    return (
        <div
            className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay"
            onClick={handleClose}
        >
            <div
                className="w-[520px] max-h-[85vh] overflow-hidden rounded-md bg-modal-surface shadow-xl flex flex-col"
                onClick={(e) => e.stopPropagation()}
            >
                {/* Header */}
                <div className="shrink-0 flex items-center gap-2 px-6 pt-6 pb-3">
                    <h3 className="text-sm font-semibold">
                        {t("embedding.addCustomProvider")}
                    </h3>
                </div>

                {/* Scrollable content */}
                <OverlayScrollbar fill="flex" className="px-6 pb-2 space-y-3">
                    {/* Provider Name */}
                    <div>
                        <label className="mb-1 block text-xs text-text-tertiary">
                            {t("embedding.customProviderName")}
                        </label>
                        <input
                            type="text"
                            value={name}
                            onChange={(e) => handleNameChange(e.target.value)}
                            placeholder={t("embedding.customProviderNamePlaceholder")}
                            className="w-full rounded-md border border-input-border bg-input-bg px-3 py-2 text-xs"
                        />
                    </div>

                    {/* Provider ID */}
                    <div>
                        <label className="mb-1 block text-xs text-text-tertiary">
                            {t("embedding.customProviderId")}
                        </label>
                        <input
                            type="text"
                            value={id}
                            onChange={(e) => setId(e.target.value)}
                            placeholder={t("embedding.customProviderIdPlaceholder")}
                            className="w-full rounded-md border border-input-border bg-input-bg px-3 py-2 text-xs font-mono"
                        />
                    </div>

                    {/* Base URL */}
                    <div>
                        <label className="mb-1 block text-xs text-text-tertiary">
                            {t("embedding.customBaseUrl")}
                        </label>
                        <input
                            type="text"
                            value={api}
                            onChange={(e) => setApi(e.target.value)}
                            placeholder="https://api.example.com/v1"
                            className="w-full rounded-md border border-input-border bg-input-bg px-3 py-2 text-xs font-mono"
                        />
                    </div>

                    {/* API Key (optional) */}
                    <div>
                        <label className="mb-1 block text-xs text-text-tertiary">
                            {t("embedding.apiKey")}{" "}
                            <span className="text-text-tertiary">({t("embedding.optional")})</span>
                        </label>
                        <input
                            type="password"
                            value={apiKey}
                            onChange={(e) => setApiKey(e.target.value)}
                            placeholder="sk-..."
                            className="w-full rounded-md border border-input-border bg-input-bg px-3 py-2 text-xs"
                        />
                    </div>

                    {/* Models list */}
                    <div>
                        <div className="mb-1 flex items-center justify-between">
                            <label className="text-xs text-text-tertiary">
                                {t("embedding.customModels")}
                            </label>
                            <button
                                type="button"
                                onClick={addModel}
                                className="inline-flex items-center gap-1 text-11 text-text-tertiary hover:text-zinc-700  dark:hover:text-zinc-200"
                            >
                                <Plus className="h-3 w-3" />
                                {t("embedding.customAddModel")}
                            </button>
                        </div>
                        <div className="space-y-2">
                            {models.map((m, idx) => (
                                <div
                                    key={idx}
                                    className="rounded border border-border-outer bg-zinc-50 p-2  dark:bg-zinc-800"
                                >
                                    <div className="mb-1 grid grid-cols-2 gap-2">
                                        <input
                                            type="text"
                                            value={m.id}
                                            onChange={(e) =>
                                                updateModel(idx, { id: e.target.value })
                                            }
                                            placeholder={t("embedding.customModelIdPlaceholder")}
                                            className="rounded-md border border-input-border bg-input-bg px-2 py-1 text-11 font-mono"
                                        />
                                        <input
                                            type="text"
                                            value={m.name}
                                            onChange={(e) =>
                                                updateModel(idx, { name: e.target.value })
                                            }
                                            placeholder={t("embedding.customModelNamePlaceholder")}
                                            className="rounded-md border border-input-border bg-input-bg px-2 py-1 text-11"
                                        />
                                    </div>
                                    <div className="flex items-center gap-2">
                                        <input
                                            type="number"
                                            value={m.dimensions}
                                            onChange={(e) =>
                                                updateModel(idx, { dimensions: e.target.value })
                                            }
                                            placeholder={t("embedding.customModelDimensionsPlaceholder")}
                                            className="w-24 rounded-md border border-input-border bg-input-bg px-2 py-1 text-11"
                                        />
                                        <input
                                            type="number"
                                            value={m.context_length}
                                            onChange={(e) =>
                                                updateModel(idx, {
                                                    context_length: e.target.value,
                                                })
                                            }
                                            placeholder={t("embedding.customModelContextLengthPlaceholder")}
                                            className="w-28 rounded-md border border-input-border bg-input-bg px-2 py-1 text-11"
                                        />
                                        {models.length > 1 && (
                                            <button
                                                type="button"
                                                onClick={() => removeModel(idx)}
                                                className="ml-auto text-text-tertiary hover:text-red-500"
                                                title={t("embedding.customRemoveModel")}
                                            >
                                                <Trash2 className="h-3 w-3" />
                                            </button>
                                        )}
                                    </div>
                                </div>
                            ))}
                        </div>
                    </div>

                    {/* Inline error */}
                    {error && (
                        <div className="rounded border border-red-200 bg-red-50 px-2 py-1.5 text-11 text-red-700 dark:border-red-800 dark:bg-red-950 dark:text-red-300">
                            {error}
                        </div>
                    )}
                </OverlayScrollbar>

                {/* Footer */}
                <div className="shrink-0 flex items-center justify-end gap-2 border-t border-border-divider px-6 py-4">
                    <button
                        onClick={handleClose}
                        disabled={saving}
                        className="rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50  dark:hover:bg-zinc-700"
                    >
                        {t("embedding.cancel")}
                    </button>
                    <button
                        onClick={handleSave}
                        disabled={saving}
                        className="btn-accent rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
                    >
                        {saving ? t("embedding.saving") : t("embedding.save")}
                    </button>
                </div>
            </div>
        </div>
    );
}
