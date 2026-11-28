import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AlertTriangle, Loader } from "lucide-react";
import { useChatStore, STUCK_FORCE_RECONNECT_MS } from "../../stores/chatStore";
import { useTranslation } from "../../i18n/useTranslation";
import { log } from "../../lib/logger";
import { cn } from "../../lib/utils";
import { Tooltip } from "../common/Tooltip";
import { useToast } from "../common/ToastProvider";

/**
 * MQTT/agent liveness as a title-bar pill, next to the Gateway switcher and
 * `StatusPill` (same family: "something about the connection", same corner,
 * visible on every view without stealing content height).
 *
 * Moved out of the composer, where it was a full-width amber strip inside the
 * input capsule and shoved the text field down on every reconnect. The input's
 * placeholder still says the state (`inputMessageConnecting` /
 * `inputGatewayDisconnected`), so nothing was lost by leaving the box.
 *
 * State comes straight from the watchdog (`effectiveConnection` +
 * `staleSince`), so the pill needs no props and cannot disagree with the
 * composer. Renders ONLY on a non-`connected` state:
 *   - `connecting` / `reconnecting` / `stale` — muted pill + spinner, with a
 *     countdown to the watchdog's forced reconnect
 *     (`staleSince + STUCK_FORCE_RECONNECT_MS`).
 *   - `disconnected` — amber, manual `force_reconnect_mqtt` (terminal state:
 *     the Rust session will not come back on its own).
 *   - `idle` — as above, but `connect_mqtt` (no client slot exists yet;
 *     `force_reconnect_mqtt` errors out without one, and `connect_mqtt` is
 *     idempotent when one does).
 *
 * The 1 Hz tick only runs while visible, so a healthy connection costs nothing.
 */
export function ConnectionStatusPill(): React.ReactElement | null {
  const { t } = useTranslation();
  const { addToast } = useToast();
  const effectiveConnection = useChatStore((s) => s.effectiveConnection);
  const staleSince = useChatStore((s) => s.staleSince);

  const [busy, setBusy] = useState(false);
  const [now, setNow] = useState(() => Date.now());

  const isBannerWorthy =
    effectiveConnection === "connecting" ||
    effectiveConnection === "reconnecting" ||
    effectiveConnection === "stale" ||
    effectiveConnection === "disconnected" ||
    effectiveConnection === "idle";

  useEffect(() => {
    if (!isBannerWorthy) return;
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, [isBannerWorthy]);

  const reconnect = useCallback(async () => {
    setBusy(true);
    try {
      await invoke("force_reconnect_mqtt");
    } catch (err) {
      log.warn("[connection-pill] force_reconnect_mqtt failed:", err);
    } finally {
      setBusy(false);
    }
  }, []);

  // `connect_mqtt` targets the bootstrap path (no Rust client to rebuild).
  // Surface the reason on failure: a silent log made the button look dead.
  const connect = useCallback(async () => {
    setBusy(true);
    try {
      await invoke("connect_mqtt");
    } catch (err) {
      log.warn("[connection-pill] connect_mqtt failed:", err);
      addToast({ type: "error", message: String(err) });
    } finally {
      setBusy(false);
    }
  }, [addToast]);

  if (!isBannerWorthy) return null;

  const stateLabel: Record<
    "connecting" | "reconnecting" | "stale" | "disconnected" | "idle",
    string
  > = {
    connecting: t("chatPanel.connectionBanner.connecting"),
    reconnecting: t("chatPanel.connectionBanner.reconnecting"),
    stale: t("chatPanel.connectionBanner.stale"),
    disconnected: t("chatPanel.connectionBanner.disconnected"),
    idle: t("chatPanel.connectionBanner.idle"),
  };
  const label = stateLabel[effectiveConnection as keyof typeof stateLabel] ?? "";

  const isWarn =
    effectiveConnection === "stale" ||
    effectiveConnection === "disconnected" ||
    effectiveConnection === "idle";

  const deadlineAt = staleSince !== null ? staleSince + STUCK_FORCE_RECONNECT_MS : null;
  const secondsRemaining =
    deadlineAt !== null ? Math.max(0, Math.ceil((deadlineAt - now) / 1000)) : null;
  const retryText =
    secondsRemaining === null
      ? null
      : secondsRemaining === 1
        ? t("chatPanel.connectionBanner.retryInOneSecond")
        : t("chatPanel.connectionBanner.retryIn", { seconds: secondsRemaining });

  const action =
    effectiveConnection === "disconnected"
      ? { label: t("chatPanel.connectionBanner.reconnectNow"), run: reconnect }
      : effectiveConnection === "idle"
        ? { label: t("chatPanel.connectionBanner.connectNow"), run: connect }
        : null;

  const pill = (
    <div
      role="status"
      aria-live="polite"
      data-testid="chat-connection-banner"
      className={cn(
        "inline-flex h-6 max-w-[min(60vw,32rem)] shrink-0 items-center gap-1 rounded-md border px-2 text-xs",
        isWarn
          ? "border-warning-border/70 bg-warning-surface-hover text-warning-quiet"
          : "border-border-outer/50 bg-surface/80 text-text-tertiary",
      )}
    >
      {isWarn ? (
        <AlertTriangle className="h-3.5 w-3.5 shrink-0 text-warning" aria-hidden="true" />
      ) : (
        <Loader className="h-3 w-3 shrink-0 animate-spin text-info" aria-hidden="true" />
      )}
      <span className="truncate">{label}</span>
      {retryText !== null && !action && (
        <span className="shrink-0 tabular-nums" data-testid="chat-connection-banner-countdown">
          {retryText}
        </span>
      )}
      {action && (
        <button
          type="button"
          data-testid={
            effectiveConnection === "idle"
              ? "chat-connection-banner-connect"
              : "chat-connection-banner-reconnect"
          }
          onClick={() => void action.run()}
          disabled={busy}
          aria-label={action.label}
          className={cn(
            "shrink-0 rounded border px-1.5 text-[11px] font-medium transition-colors",
            "border-warning-border/70 text-warning-quiet hover:bg-warning-surface",
            "dark:border-amber-600/70 dark:text-amber-200 dark:hover:bg-amber-900/40",
            busy && "opacity-60",
          )}
        >
          {action.label}
        </button>
      )}
    </div>
  );

  // The pill is too narrow for the full sentence — hover carries it.
  return action || retryText !== null ? (
    pill
  ) : (
    <Tooltip content={t("chatPanel.connectionBanner.recoveringHint")} position="bottom">
      {pill}
    </Tooltip>
  );
}