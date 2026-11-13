/**
 * GatewayAddressPicker — the single server-address control (ADR-115).
 *
 * There is no longer a user-visible "local / remote" mode. `localhost` is
 * just one entry in the address list, and picking an address that is down
 * is never a dead end: this control is mounted on the login screen, the
 * first-boot setup screen, the splash timeout view and the settings page,
 * so the user can always get back to it.
 *
 * Selecting an address calls `onSelect(url)`. Callers decide whether that
 * means "reboot the connection pipeline" (login/setup/splash) or "apply in
 * place" (settings); the control itself never writes the store.
 *
 * Layout is one row — label, combobox, connect — plus a hint. The dropdown
 * is the ONLY list: each option row renders reachability dot + URL + tag
 * (current / this machine / last connected) via `UrlComboBox.renderOption`.
 */
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useSettingsStore } from "../../stores/settingsStore";
import { probeGateways, type ProbeResult } from "../../lib/gateway-probe";
import { listRowSelected } from "../../lib/ui-styles";
import { cn } from "../../lib/utils";
import {
  LOCAL_GATEWAY_URL,
  gatewayAddressOptions,
  hasHttpScheme,
  isLoopbackGatewayUrl,
  normalizeGatewayUrl,
} from "../../lib/gatewayUrl";
import { UrlComboBox } from "../settings/UrlComboBox";

export interface GatewayAddressPickerProps {
  /** Called with a normalized URL when the user commits an address. */
  onSelect: (url: string) => void;
  /** Disable every control while a connection attempt is in flight. */
  busy?: boolean;
  /** Optional one-click escape hatch back to factory connection settings. */
  onReset?: () => void;
  /** Compact layout for the splash timeout view. */
  compact?: boolean;
}

type Reachability = "checking" | "online" | "offline";

export function GatewayAddressPicker({
  onSelect,
  busy = false,
  onReset,
  compact = false,
}: GatewayAddressPickerProps) {
  const { t } = useTranslation();
  const gatewayUrl = useSettingsStore((s) => s.gatewayUrl);
  const history = useSettingsStore((s) => s.gatewayUrlHistory);
  const lastGoodUrl = useSettingsStore((s) => s.lastGoodUrl);

  const [draft, setDraft] = useState(gatewayUrl);
  const [status, setStatus] = useState<Record<string, Reachability>>({});
  const lastUrlRef = useRef(gatewayUrl);

  useEffect(() => {
    if (lastUrlRef.current !== gatewayUrl) {
      lastUrlRef.current = gatewayUrl;
      setDraft(gatewayUrl);
    }
  }, [gatewayUrl]);

  const options = useMemo(
    () => gatewayAddressOptions(gatewayUrl, history, lastGoodUrl),
    [gatewayUrl, history, lastGoodUrl],
  );
  const optionsKey = options.join("|");

  useEffect(() => {
    const urls = optionsKey ? optionsKey.split("|") : [];
    if (urls.length === 0) return;
    let alive = true;
    setStatus(Object.fromEntries(urls.map((u) => [u, "checking" as Reachability])));
    void probeGateways(urls, {
      onSettled: (r: ProbeResult) => {
        if (!alive) return;
        setStatus((prev) => ({ ...prev, [r.url]: r.ok ? "online" : "offline" }));
      },
    });
    return () => {
      alive = false;
    };
    // `options` is derived from the store; the joined key keeps the effect
    // stable across unrelated re-renders.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [optionsKey]);

  const invalidDraft = draft.trim() !== "" && !hasHttpScheme(draft);

  const commit = (raw: string) => {
    const url = normalizeGatewayUrl(raw);
    if (!url || busy || !hasHttpScheme(url)) return;
    setDraft(url);
    onSelect(url);
  };

  const tag = (url: string): string | null => {
    if (url === gatewayUrl) return t("connection.current");
    if (url === LOCAL_GATEWAY_URL) return t("connection.thisMachine");
    if (url === lastGoodUrl) return t("connection.lastGood");
    return null;
  };

  const dotClass = (state: Reachability | undefined): string => {
    if (state === "online") return "bg-emerald-500";
    if (state === "offline") return "bg-zinc-300 dark:bg-zinc-600";
    return "bg-amber-400 animate-pulse";
  };

  // One rich row per option — dot + URL + tag — rendered inside the
  // combobox dropdown. This is the single place all known addresses and
  // their state are shown.
  const renderOption = (url: string): ReactNode => {
    const isCurrent = url === gatewayUrl;
    const rowTag = tag(url);
    return (
      <span
        className={cn(
          "flex w-full items-center gap-2 rounded px-1 py-0.5 text-left",
          isCurrent && listRowSelected,
        )}
      >
        <span
          aria-hidden="true"
          className={`inline-block h-1.5 w-1.5 shrink-0 rounded-full ${dotClass(status[url])}`}
        />
        <span className="min-w-0 flex-1 truncate font-mono text-11 text-text-secondary">
          {url}
        </span>
        {rowTag && (
          <span className="shrink-0 text-11 text-text-tertiary">{rowTag}</span>
        )}
      </span>
    );
  };

  return (
    <div className={compact ? "flex flex-col gap-1.5" : "flex flex-col gap-2"}>
      {/* Compact (login / setup card) puts the label on its own line: the
          card is ~272px wide and an inline label squeezed the combobox to a
          ~100px sliver — its own dropdown was wider than the field it
          belonged to. The settings row keeps the inline label (plenty of
          width there). */}
      <div className="flex flex-wrap items-center gap-2">
        <label
          htmlFor="gateway-address-input"
          className={cn(
            "text-xs text-text-tertiary",
            compact ? "w-full" : "shrink-0",
          )}
        >
          {t("connection.serverAddress")}
        </label>
        <UrlComboBox
          ariaLabel={t("connection.serverAddress")}
          value={draft}
          onChange={setDraft}
          onCommit={(url) => commit(url ?? draft)}
          options={options}
          renderOption={renderOption}
          placeholder={LOCAL_GATEWAY_URL}
          disabled={busy}
        />
        <button
          type="button"
          disabled={busy || !normalizeGatewayUrl(draft) || invalidDraft}
          onClick={() => commit(draft)}
          className="btn-accent shrink-0 rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-40"
        >
          {t("connection.connect")}
        </button>
      </div>

      {invalidDraft ? (
        <p role="alert" className="text-11 leading-snug text-red-500">
          {t("connection.invalidUrl")}
        </p>
      ) : null}

      {/* One line, one message: the invalid-scheme alert above already owns
          the row when the draft cannot be used, so the standing hint yields
          instead of stacking two paragraphs under a one-field control. */}
      {!invalidDraft && (
        <p className="text-11 leading-snug text-text-tertiary">
          {isLoopbackGatewayUrl(draft) && status[draft] !== "online"
            ? t("connection.localAutoStart")
            : t("connection.hint")}
        </p>
      )}

      {onReset && (
        <button
          type="button"
          disabled={busy}
          onClick={onReset}
          className="self-start text-11 text-[var(--color-accent)] hover:underline disabled:opacity-50"
        >
          {t("connection.reset")}
        </button>
      )}
    </div>
  );
}
