// ADR-123 §4: the cache-management card.
//
// One section per service the Gateway's aggregate knows about, one row per
// storage category, and a delete button **only** where the owner said
// `cleanable && kind === "cache"`. Data-class rows (chat attachments, the
// document library, the workspace) render a size and nothing else — ADR-123 D4
// keeps them out of every delete path, and the Gateway refuses a forwarded
// cleanup for them anyway.
//
// The component holds no policy: sizes, ids and the `cleanable` flag all come
// from the report, and a cleanup call carries ids only. A service that answers
// nothing still gets a row (`reachable: false`) so the user can tell "0 bytes"
// from "not running" — a distinction that matters when the disk is full.

import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Loader, RefreshCw, Trash2 } from "lucide-react";

import { cn } from "../../lib/utils";
import { log } from "../../lib/logger";
import { formatSizeBytes } from "../../lib/agentHub";
import { useToast } from "../common/ToastProvider";
import { cleanupCache, fetchCacheReport, type CacheReport, type CacheServiceRow } from "../../lib/cacheApi";

/** i18n label for a service key; unknown keys fall back to the raw id so a new
 *  service can appear without a frontend release (ADR-123 D2). */
function serviceLabel(t: (k: string) => string, row: CacheServiceRow): string {
  const key = `settings.cache.service.${row.node_id ? "node" : row.service}`;
  const label = t(key);
  return label === key ? row.service : label;
}

export function CacheStorageCard() {
  const { t } = useTranslation();
  const { addToast } = useToast();
  const [report, setReport] = useState<CacheReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setReport(await fetchCacheReport());
      setError(null);
    } catch (e) {
      log.warn("[CacheStorageCard] report failed:", e);
      const message = e instanceof Error ? e.message : String(e);
      setError(message);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  /** Clean one category, then re-read so the number the user sees is the
   *  owner's post-sweep measurement, not our guess. */
  const clean = useCallback(
    async (row: CacheServiceRow, id: string) => {
      const key = `${row.service}:${id}`;
      setPending(key);
      try {
        const result = await cleanupCache(row.service, [id]);
        addToast({
          type: "success",
          message: t("settings.cache.cleaned", { size: formatSizeBytes(result.freed_bytes) }),
        });
        await load();
      } catch (e) {
        log.warn("[CacheStorageCard] cleanup failed:", e);
        addToast({ type: "error", message: e instanceof Error ? e.message : String(e) });
      } finally {
        setPending(null);
      }
    },
    [addToast, load, t],
  );

  const disk = report?.disk;
  const diskPct = disk ? Math.round(disk.used_percent) : 0;

  // Rows arrive gateway-first; keep the server order so the card reads the same
  // way in the log and on screen.
  const services = useMemo(() => report?.services ?? [], [report]);

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="min-w-0">
          <p className="truncate text-xs text-text-secondary">
            {disk
              ? t("settings.cache.volumeLine", {
                  used: formatSizeBytes(disk.total_bytes - disk.available_bytes),
                  total: formatSizeBytes(disk.total_bytes),
                  percent: diskPct,
                })
              : t("settings.cache.volumeUnknown")}
          </p>
          {disk?.alarm && (
            <p className="mt-0.5 text-10 text-warning">
              {t("settings.cache.volumeAlarm", { percent: Math.round(disk.threshold_percent) })}
            </p>
          )}
        </div>
        <button
          type="button"
          onClick={() => void load()}
          disabled={loading}
          className="btn-solid inline-flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded-md px-3 py-1.5 text-xs font-medium disabled:opacity-50"
          aria-label={t("settings.cache.refresh")}
        >
          {loading ? (
            <Loader className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
          ) : (
            <RefreshCw className="h-3.5 w-3.5" aria-hidden="true" />
          )}
          {t("settings.cache.refresh")}
        </button>
      </div>

      {error && !report && (
        <div className="rounded-md border border-danger-border/60 bg-danger-surface px-3 py-2 text-xs text-danger-text-strong">
          {t("settings.cache.reportFailed", { message: error })}
        </div>
      )}

      {services.map((row) => (
        <section key={row.service} className="space-y-1">
          <h4 className="text-10 font-medium uppercase tracking-wider text-text-tertiary">
            {serviceLabel(t, row)}
            {!row.reachable && (
              <span className="ml-2 text-text-quaternary">
                {t("settings.cache.unreachable")}
              </span>
            )}
          </h4>
          <ul className="divide-y divide-border-divider overflow-hidden rounded-md border border-border-outer">
            {row.items.length === 0 && (
              <li className="px-3 py-2 text-xs text-text-tertiary">
                {row.reachable ? t("settings.cache.emptyRow") : (row.error ?? t("settings.cache.unreachable"))}
              </li>
            )}
            {row.items.map((item) => (
              <li key={item.id} className="flex items-center gap-3 px-3 py-2">
                <span className="min-w-0 flex-1 truncate text-xs text-text">
                  {(() => {
                    const key = `settings.cache.item.${item.id}`;
                    const label = t(key);
                    return label === key ? item.id : label;
                  })()}
                </span>
                {/* Data rows say so explicitly: the user's mental model is
                    "everything here is cleanable", and one unlabelled
                    non-cleanable row reads like a broken button. */}
                {item.kind === "data" && (
                  <span className="shrink-0 rounded-full border border-border-divider px-2 py-0.5 text-10 text-text-tertiary">
                    {t("settings.cache.kindData")}
                  </span>
                )}
                <span className="shrink-0 font-mono text-xs text-text-secondary">
                  {formatSizeBytes(item.bytes)}
                </span>
                {item.cleanable && item.kind === "cache" ? (
                  <button
                    type="button"
                    onClick={() => void clean(row, item.id)}
                    disabled={pending !== null}
                    className={cn(
                      "inline-flex shrink-0 items-center gap-1 rounded px-2 py-1 text-xs transition-colors text-danger-text hover:bg-danger-surface disabled:cursor-not-allowed disabled:opacity-50",
                    )}
                    aria-label={t("settings.cache.clean")}
                  >
                    {pending === `${row.service}:${item.id}` ? (
                      <Loader className="h-3 w-3 animate-spin" aria-hidden="true" />
                    ) : (
                      <Trash2 className="h-3 w-3" aria-hidden="true" />
                    )}
                    {t("settings.cache.clean")}
                  </button>
                ) : (
                  // Reserved button-width spacer keeps sizes on one column.
                  <span className="w-[68px] shrink-0" aria-hidden="true" />
                )}
              </li>
            ))}
          </ul>
        </section>
      ))}

      {!report && !loading && (
        <div className="rounded-md border border-dashed border-border-strong px-3 py-6 text-center text-xs text-text-tertiary">
          {t("settings.cache.emptyHint")}
        </div>
      )}
    </div>
  );
}