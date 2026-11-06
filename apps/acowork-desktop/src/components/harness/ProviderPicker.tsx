import { useState, useMemo, useCallback } from "react";
import type { VaultKeyEntry, ProviderListEntry } from "../../lib/types";
import { isLocalProvider } from "../../lib/providers";
import { StyledInput } from "../common/StyledInput";
import { ListBox, ListRow, ExpandableRow } from "../common/list";
import { useTranslation } from "../../i18n/useTranslation";
import { getGatewayUrl } from "../../lib/config";
import { cn } from "../../lib/utils";
import { Search, Plus, RefreshCw, Check, AlertCircle } from "lucide-react";
import { ProviderLogo } from "../common/ProviderLogo";

/** Scroll cap for a provider group's body: three rows tall however many
 *  providers (or search hits) the group actually holds.
 *
 *  The alternative this replaces was a `slice(0, 5)` + "Show all (N)" button:
 *  clicking it expanded the group to the full catalog height, pushing the
 *  Local / Remote groups and the wizard content below it off screen — and the
 *  hits from a search landed under a different rule (no cap at all).
 *
 *  Bound to `--ui-font-size`, not a fixed px, because the rows themselves are
 *  text-sized: one two-line remote row is ~3.4 × the base (12px label + 10px
 *  model count + 12px of `py-1.5`), so 3 rows ≈ 10.2 × base. A frozen height
 *  would show 2 rows at XXL and 4 at S. */
const GROUP_BODY_MAX_HEIGHT = "calc(var(--ui-font-size, 0.875rem) * 10.2)";

interface ProviderPickerProps {
  providers: ProviderListEntry[];
  keys: VaultKeyEntry[];
  onConnect: (providerId: string, provider: ProviderListEntry) => void;
  onAddCustom: () => void;
  /**
   * Called after a successful catalog refresh so the parent can re-fetch
   * the provider list (the in-memory Gateway cache was swapped and the
   * next /api/models call returns the new set).
   */
  onCatalogRefreshed?: () => void | Promise<void>;
}

/** Reusable available-providers list. Renders custom / local / remote sections
 *  with "Connect" buttons and an "Add Custom Provider" button. Pure UI —
 *  caller handles the add flow. */
export function ProviderPicker({ providers, keys, onConnect, onAddCustom, onCatalogRefreshed }: ProviderPickerProps) {
  const { t } = useTranslation();
  const [providerSearchTerm, setProviderSearchTerm] = useState("");
  // Setup-panel style level-1 collapsible groups, default open
  const [customOpen, setCustomOpen] = useState(true);
  const [localOpen, setLocalOpen] = useState(true);
  const [remoteOpen, setRemoteOpen] = useState(true);

  // Catalog-refresh state machine. Drives the refresh-button icon, the
  // status pip next to the search box, the progress bar, and the timeout
  // guard. `downloading` carries cumulative bytes from the SSE `progress`
  // events so the user sees a live fill as the ~5MB body streams in.
  type RefreshStatus =
    | { kind: "idle" }
    | { kind: "loading"; bytes: number; total: number | null }
    | { kind: "ok"; providers: number; models: number; bytes: number }
    | { kind: "error"; message: string };
  const [refreshStatus, setRefreshStatus] = useState<RefreshStatus>({ kind: "idle" });

  const handleRefreshCatalog = useCallback(async () => {
    if (refreshStatus.kind === "loading") return; // re-entry guard
    setRefreshStatus({ kind: "loading", bytes: 0, total: null });

    // 60s hard timeout — the server's default is 30s but a slow CDN +
    // 5MB payload + cold TLS handshake can stretch further. We bound
    // it client-side too so a hung connection doesn't pin the spinner
    // forever. This is intentionally > server default so the server's
    // own 502 path triggers first on real network failures.
    const controller = new AbortController();
    const timer = window.setTimeout(() => controller.abort(), 60_000);

    // Parse one SSE event from a buffer. SSE format:
    //   event: <name>\n
    //   data: <json>\n
    //   \n
    // We scan for `\n\n` (end-of-event) and return the parsed event
    // plus the remainder. Multiple events can land in one chunk after
    // a long pause, so we loop until the buffer holds no full event.
    const readSseEvent = async (
      reader: ReadableStreamDefaultReader<Uint8Array>,
      decoder: TextDecoder,
      buffer: { value: string },
    ): Promise<{ event: string; data: string } | null> => {
      while (true) {
        const sep = buffer.value.indexOf("\n\n");
        if (sep >= 0) {
          const raw = buffer.value.slice(0, sep);
          buffer.value = buffer.value.slice(sep + 2);
          // Parse event: / data: lines.
          let event = "message";
          const dataLines: string[] = [];
          for (const line of raw.split("\n")) {
            if (line.startsWith("event:")) event = line.slice(6).trim();
            else if (line.startsWith("data:")) dataLines.push(line.slice(5).trim());
          }
          return { event, data: dataLines.join("\n") };
        }
        const { value, done } = await reader.read();
        if (done) return null;
        buffer.value += decoder.decode(value, { stream: true });
      }
    };

    try {
      const resp = await fetch(`${getGatewayUrl()}/api/models/refresh-catalog`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Accept: "text/event-stream",
        },
        body: JSON.stringify({ timeout_secs: 30 }),
        signal: controller.signal,
      });

      if (!resp.ok || !resp.body) {
        // Non-streaming error path: read body once, surface `error` field.
        let detail = `HTTP ${resp.status}`;
        try {
          const body = await resp.json();
          if (body && typeof body.error === "string") detail = body.error;
        } catch {
          /* body wasn't JSON; keep the HTTP status */
        }
        throw new Error(detail);
      }

      // Streaming SSE parse loop.
      const reader = resp.body.getReader();
      const decoder = new TextDecoder("utf-8");
      const buffer = { value: "" };
      let finalSummary: { providers: number; models: number; bytes: number } | null = null;
      let errorMessage: string | null = null;

      while (true) {
        const evt = await readSseEvent(reader, decoder, buffer);
        if (evt === null) break; // stream closed
        if (evt.event === "progress") {
          try {
            const payload = JSON.parse(evt.data) as { bytes: number; total: number | null };
            setRefreshStatus({ kind: "loading", bytes: payload.bytes, total: payload.total });
          } catch {
            /* malformed event — ignore */
          }
        } else if (evt.event === "done") {
          try {
            const payload = JSON.parse(evt.data) as {
              providers?: number;
              models?: number;
              bytes?: number;
              ok?: boolean;
            };
            if (payload.ok !== false && typeof payload.providers === "number") {
              finalSummary = {
                providers: payload.providers,
                models: payload.models ?? 0,
                bytes: payload.bytes ?? 0,
              };
            }
          } catch {
            /* malformed done — error event will follow if real failure */
          }
          break;
        } else if (evt.event === "error") {
          try {
            const payload = JSON.parse(evt.data) as { error?: string };
            if (typeof payload.error === "string") errorMessage = payload.error;
          } catch {
            /* malformed */
          }
          // Wait for the trailing `done` so we don't double-finalise.
        }
      }

      if (errorMessage) throw new Error(errorMessage);
      if (!finalSummary) throw new Error("stream ended without done event");

      setRefreshStatus({
        kind: "ok",
        providers: finalSummary.providers,
        models: finalSummary.models,
        bytes: finalSummary.bytes,
      });
      // Auto-clear the success pip after 4s so the row returns to a calm state.
      window.setTimeout(() => {
        setRefreshStatus((s) => (s.kind === "ok" ? { kind: "idle" } : s));
      }, 4000);
      if (onCatalogRefreshed) {
        await onCatalogRefreshed();
      }
    } catch (e) {
      const aborted = e instanceof DOMException && e.name === "AbortError";
      setRefreshStatus({
        kind: "error",
        message: aborted
          ? t("harness.refreshCatalogTimeout")
          : t("harness.refreshCatalogFailed", { error: e instanceof Error ? e.message : String(e) }),
      });
    } finally {
      window.clearTimeout(timer);
    }
  }, [refreshStatus.kind, onCatalogRefreshed, t]);

  // Split providers into local / custom / remote
  const { localProviders, remoteProviders, customProviders } = useMemo(() => {
    const local: ProviderListEntry[] = [];
    const remote: ProviderListEntry[] = [];
    const custom: ProviderListEntry[] = [];
    for (const p of providers) {
      if (p.custom) {
        custom.push(p);
      } else if (p.local || isLocalProvider(p.id)) {
        local.push(p);
      } else {
        remote.push(p);
      }
    }
    return { localProviders: local, remoteProviders: remote, customProviders: custom };
  }, [providers]);

  // Once a provider has an account it lives ONLY under "Configured Providers":
  // a picker row has no per-row edit/remove, so a leftover copy here would
  // drift out of sync with the configured list and offer no way to manage it.
  // Extra accounts for an already-configured provider are added from its row in
  // the configured list (edit dialog), not by re-clicking here.
  const configuredCount = (providerId: string) => keys.filter((k) => k.provider === providerId).length;
  const remoteAvailable = remoteProviders.filter((p) => configuredCount(p.id) === 0);
  const normalizedTerm = providerSearchTerm.trim().toLowerCase();
  const filteredRemoteProviders = normalizedTerm
    ? remoteAvailable.filter(
        (p) => p.name?.toLowerCase().includes(normalizedTerm) || p.id.toLowerCase().includes(normalizedTerm)
      )
    : remoteAvailable;

  // Providers per group. Every group body is capped at three rows and scrolls
  // (see GROUP_BODY_MAX_HEIGHT), so the full list is rendered — no display
  // window and no "show all" step.
  const availableCustom = customProviders.filter((p) => configuredCount(p.id) === 0);
  const availableLocal = localProviders.filter((p) => configuredCount(p.id) === 0);
  const availableRemote = remoteAvailable.length;

  if (providers.length === 0) {
    return (
      <div className="py-3 text-center text-xs text-text-tertiary">{t("harness.noProvidersAvailable")}</div>
    );
  }

  return (
    <div className="space-y-4">
      {/* ── Custom Providers — level-1 collapsible group (Setup-panel
          grammar: chevron + title + count badge, default open) ── */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={customOpen}
          onToggle={() => setCustomOpen((v) => !v)}
          title={<span className="flex items-center gap-1">🔧 {t("harness.customProviders", { count: availableCustom.length })}</span>}
          ariaLabel={t("harness.customProviders", { count: availableCustom.length })}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
        >
          <ListBox variant="plain" maxHeight={GROUP_BODY_MAX_HEIGHT}>
            {availableCustom.map((item) => {
              const providerId = item.id;
              const providerName = item.name || providerId;
              return (
                <ListRow
                  key={providerId}
                  surface="inset"
                  leading={<ProviderLogo providerId={providerId} size={16} />}
                  trailing={
                    <div className="flex items-center gap-2">
                      <button
                        type="button"
                        onClick={() => onConnect(providerId, item)}
                        className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-text-secondary hover:bg-zinc-200 dark:bg-zinc-700  dark:hover:bg-zinc-600"
                      >
                        {t("harness.connect")}
                      </button>
                    </div>
                  }
                >
                  <span className="block truncate text-xs font-medium text-text-secondary ">{providerName}</span>
                </ListRow>
              );
            })}
          </ListBox>
          {/* Add Custom Provider button — sits below the custom list */}
          <div className="px-3 pb-2 pt-1.5">
            <button
              type="button"
              onClick={onAddCustom}
              className="flex w-full items-center gap-2 rounded-md border-2 border-dashed border-zinc-300 px-3 py-2 text-xs font-medium text-text-secondary transition-colors hover:border-[var(--color-accent)] hover:text-[var(--color-accent)] dark:border-zinc-600  dark:hover:border-[var(--color-accent)] dark:hover:text-[var(--color-accent)]"
            >
              <Plus className="h-4 w-4" />
              {t("harness.addCustomProvider")}
            </button>
          </div>
        </ExpandableRow>
      </ListBox>

      {/* ── Local Providers — level-1 collapsible group ── */}
      {availableLocal.length > 0 && (
        <ListBox dividers={false}>
          <ExpandableRow
            open={localOpen}
            onToggle={() => setLocalOpen((v) => !v)}
            title={<span className="flex items-center gap-1">🏠 {t("harness.localProviders", { count: availableLocal.length })}</span>}
          ariaLabel={t("harness.localProviders", { count: availableLocal.length })}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
          >
            <ListBox variant="plain" maxHeight={GROUP_BODY_MAX_HEIGHT}>
              {availableLocal.map((item) => {
                const providerId = item.id;
                const providerName = item.name || providerId;
                return (
                  <ListRow
                    key={providerId}
                    surface="inset"
                    leading={<ProviderLogo providerId={providerId} size={16} />}
                    trailing={
                      <div className="flex items-center gap-2">
                        <button
                          type="button"
                          onClick={() => onConnect(providerId, item)}
                          className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-text-secondary hover:bg-zinc-200 dark:bg-zinc-700  dark:hover:bg-zinc-600"
                        >
                          {t("harness.connect")}
                        </button>
                      </div>
                    }
                  >
                    <span className="block truncate text-xs font-medium text-text-secondary ">{providerName}</span>
                  </ListRow>
                );
              })}
            </ListBox>
          </ExpandableRow>
        </ListBox>
      )}

      {/* ── Remote Providers — level-1 collapsible group with in-header search ── */}
      {remoteProviders.length > 0 && (
        <ListBox dividers={false}>
          <ExpandableRow
            open={remoteOpen}
            onToggle={() => setRemoteOpen((v) => !v)}
            title={<span className="flex items-center gap-1">☁️ {t("harness.remoteProviders", { count: providerSearchTerm.trim() ? `${filteredRemoteProviders.length}/${availableRemote}` : availableRemote })}</span>}
            ariaLabel={t("harness.remoteProviders", { count: providerSearchTerm.trim() ? `${filteredRemoteProviders.length}/${availableRemote}` : availableRemote })}
            trailing={
              <span onClick={(e) => e.stopPropagation()} className="flex items-center gap-1.5">
                {/* Catalog refresh — fetches the latest models.dev catalog,
                    persists it to data_dir/offline_providers.json, and
                    swaps the in-memory cache. 15s client-side timeout
                    (AbortController) prevents an indefinite spinner. */}
                <button
                  type="button"
                  onClick={handleRefreshCatalog}
                  disabled={refreshStatus.kind === "loading"}
                  title={
                    refreshStatus.kind === "error"
                      ? refreshStatus.message
                      : t("harness.refreshCatalog")
                  }
                  aria-label={t("harness.refreshCatalog")}
                  data-testid="refresh-catalog-button"
                  className={cn(
                    "flex h-7 w-7 items-center justify-center rounded-md border transition-colors",
                    refreshStatus.kind === "error"
                      ? "border-red-300 bg-red-50 text-red-600 hover:bg-red-100 dark:border-red-700 dark:bg-red-950 dark:text-red-400"
                      : refreshStatus.kind === "ok"
                      ? "border-emerald-300 bg-emerald-50 text-emerald-600 dark:border-emerald-700 dark:bg-emerald-950 dark:text-emerald-400"
                      : "border-zinc-300 bg-modal-surface text-text-secondary hover:bg-zinc-100 dark:border-zinc-600 dark:hover:bg-zinc-700",
                    refreshStatus.kind === "loading" && "cursor-wait opacity-70",
                  )}
                >
                  {refreshStatus.kind === "loading" ? (
                    <RefreshCw className="h-3.5 w-3.5 animate-spin" />
                  ) : refreshStatus.kind === "ok" ? (
                    <Check className="h-3.5 w-3.5" />
                  ) : refreshStatus.kind === "error" ? (
                    <AlertCircle className="h-3.5 w-3.5" />
                  ) : (
                    <RefreshCw className="h-3.5 w-3.5" />
                  )}
                </button>
                {/* Status pip — shows success counts, live progress, or error
                    text inline next to the search box. Idle state is silent. */}
                {refreshStatus.kind === "loading" && (
                  <span className="flex items-center gap-1.5 text-10 text-text-tertiary" data-testid="refresh-catalog-progress">
                    {refreshStatus.total != null ? (
                      <>
                        <span className="tabular-nums">
                          {t("harness.refreshCatalogProgress", {
                            bytes: formatBytes(refreshStatus.bytes),
                            total: formatBytes(refreshStatus.total),
                          })}
                        </span>
                        <progress
                          max={100}
                          value={Math.min(100, Math.round((refreshStatus.bytes / refreshStatus.total) * 100))}
                          className="h-1 w-16 appearance-none overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700 [&::-webkit-progress-bar]:bg-zinc-200 [&::-webkit-progress-bar]:dark:bg-zinc-700 [&::-webkit-progress-value]:bg-blue-500 [&::-moz-progress-bar]:bg-blue-500"
                        />
                      </>
                    ) : (
                      <span className="tabular-nums">{formatBytes(refreshStatus.bytes)}</span>
                    )}
                  </span>
                )}
                {refreshStatus.kind === "ok" && (
                  <span className="text-10 text-emerald-600 dark:text-emerald-400">
                    {t("harness.refreshCatalogSuccess", { providers: refreshStatus.providers, models: refreshStatus.models })}
                  </span>
                )}
                {refreshStatus.kind === "error" && (
                  <span className="max-w-[180px] truncate text-10 text-red-600 dark:text-red-400">
                    {refreshStatus.message}
                  </span>
                )}
                <div className="relative">
                  <StyledInput
                    type="text"
                    value={providerSearchTerm}
                    onChange={(e) => setProviderSearchTerm(e.target.value)}
                    placeholder={t("harness.searchProviders")}
                    className="w-[170px] pl-7 pr-2 placeholder-zinc-400 dark:placeholder-zinc-500"
                  />
                  <Search className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-text-tertiary" />
                </div>
              </span>
            }
            bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
          >
            {providerSearchTerm.trim() && filteredRemoteProviders.length === 0 ? (
              <div className="py-3 text-center text-xs text-text-tertiary">
                {t("harness.noProvidersMatch")}
              </div>
            ) : (
              <>
                <ListBox variant="plain" maxHeight={GROUP_BODY_MAX_HEIGHT}>
                  {filteredRemoteProviders.map((item) => {
                    const providerId = item.id;
                    const providerName = item.name || providerId;
                    const modelCount = item.model_count;
                    return (
                      <ListRow
                        key={providerId}
                        surface="inset"
                        leading={<ProviderLogo providerId={providerId} size={16} />}
                        trailing={
                          <div className="flex items-center gap-2">
                            <button
                              type="button"
                              onClick={() => onConnect(providerId, item)}
                              className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-text-secondary hover:bg-zinc-200 dark:bg-zinc-700  dark:hover:bg-zinc-600"
                            >
                              {t("harness.addKey")}
                            </button>
                          </div>
                        }
                      >
                        <span className="block truncate text-xs font-medium text-text-secondary ">{providerName}</span>
                        {modelCount != null && (
                          <span className="mt-0.5 block text-10 text-text-tertiary">{t("harness.modelsAvailable", { count: modelCount })}</span>
                        )}
                      </ListRow>
                    );
                  })}
                </ListBox>
              </>
            )}
          </ExpandableRow>
        </ListBox>
      )}
    </div>
  );
}

/** Human-readable byte count (1024-based). */
function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "0 B";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 10 ? 0 : 1)} ${units[i]}`;
}
