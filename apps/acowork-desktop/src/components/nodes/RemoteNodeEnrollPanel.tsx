/**
 * Mint + show a one-shot remote-node enroll payload.
 *
 * Layout mirrors Settings → PairingCard: centered QR, short copy, one
 * primary action. CLI stays behind a disclosure.
 */
import { useEffect, useRef, useState } from "react";
import { Check, ChevronDown, Copy, Loader2, RefreshCw } from "lucide-react";
import QRCode from "qrcode";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledInput } from "../common/StyledInput";
import { copyText } from "../../lib/clipboard";
import { log } from "../../lib/logger";
import { resolvePairingBaseUrl } from "../../lib/pairing";

/** Mirror of the Tauri `RemoteNodeCommand` struct (snake_case on the wire). */
export type RemoteNodeCommand = {
  mqtt_port: number;
  http_port: number;
  token: string;
  token_ttl_seconds: number;
  suggested_host: string;
};

const isLoopback = (host: string): boolean =>
  /^(127\.|localhost$|::1$|\[::1\]$|0\.0\.0\.0$)/i.test(host.trim());

const sanitizeHost = (value: string): string =>
  value.replace(/[^A-Za-z0-9._:[\]-]/g, "");

const hostForCommand = (host: string): string => {
  const safe = host.replace(/^[-.]+/, "");
  return safe.includes(":") && !safe.startsWith("[") ? `[${safe}]` : safe;
};

const httpGatewayUrl = (host: string, httpPort: number): string =>
  `http://${hostForCommand(host)}:${httpPort}`;

export function buildEnrollCode(
  host: string,
  httpPort: number,
  token: string,
): string {
  return `acowork://enroll?gateway=${encodeURIComponent(httpGatewayUrl(host, httpPort))}&token=${encodeURIComponent(token)}`;
}

async function preferReachableHost(suggested: string): Promise<string> {
  const clean = sanitizeHost(suggested);
  if (clean && !isLoopback(clean)) return clean;
  try {
    const base = await resolvePairingBaseUrl();
    const host = new URL(base).hostname;
    return sanitizeHost(host) || clean;
  } catch {
    return clean;
  }
}

export function RemoteNodeEnrollPanel({ autoMint = true }: { autoMint?: boolean }) {
  const { t } = useTranslation();
  const [remote, setRemote] = useState<RemoteNodeCommand | null>(null);
  const [remoteError, setRemoteError] = useState<string | null>(null);
  const [loadingRemote, setLoadingRemote] = useState(false);
  const [host, setHost] = useState("");
  const [copied, setCopied] = useState<"enroll" | "cli" | null>(null);
  const [qrDataUrl, setQrDataUrl] = useState<string | null>(null);
  const [cliOpen, setCliOpen] = useState(false);
  const [editingHost, setEditingHost] = useState(false);

  const mountedRef = useRef(true);
  const copiedTimer = useRef<number | null>(null);
  const mintedOnce = useRef(false);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      if (copiedTimer.current !== null) window.clearTimeout(copiedTimer.current);
    };
  }, []);

  const mintRemoteCommand = (force = false) => {
    if (!force && (remote || loadingRemote)) return;
    if (force) {
      setRemote(null);
      setQrDataUrl(null);
      setCliOpen(false);
      setEditingHost(false);
    }
    setLoadingRemote(true);
    setRemoteError(null);
    void invoke<RemoteNodeCommand>("build_remote_node_command")
      .then(async (res) => {
        if (!mountedRef.current) return;
        const reachable = await preferReachableHost(res.suggested_host);
        if (!mountedRef.current) return;
        setRemote(res);
        setHost(reachable);
      })
      .catch((e) => {
        if (mountedRef.current) setRemoteError(String(e));
      })
      .finally(() => {
        if (mountedRef.current) setLoadingRemote(false);
      });
  };

  useEffect(() => {
    if (!autoMint || mintedOnce.current) return;
    mintedOnce.current = true;
    mintRemoteCommand();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- mint once on mount
  }, [autoMint]);

  const effectiveHost = sanitizeHost(host.trim() || remote?.suggested_host || "");
  const httpPort = remote?.http_port ?? 19876;
  const enrollCode =
    remote && effectiveHost
      ? buildEnrollCode(effectiveHost, httpPort, remote.token)
      : null;
  const command =
    remote && effectiveHost
      ? `acowork-node start --gateway ${hostForCommand(effectiveHost)}:${remote.mqtt_port} --token ${remote.token}`
      : null;
  const gatewayDisplay =
    remote && effectiveHost ? httpGatewayUrl(effectiveHost, httpPort) : null;

  useEffect(() => {
    if (!enrollCode) {
      setQrDataUrl(null);
      return;
    }
    let cancelled = false;
    void QRCode.toDataURL(enrollCode, {
      margin: 1,
      width: 208,
      errorCorrectionLevel: "M",
    })
      .then((url) => {
        if (!cancelled && mountedRef.current) setQrDataUrl(url);
      })
      .catch((e) => {
        log.warn(`[RemoteNodeEnrollPanel] QR encode failed: ${String(e)}`);
        if (!cancelled && mountedRef.current) setQrDataUrl(null);
      });
    return () => {
      cancelled = true;
    };
  }, [enrollCode]);

  const markCopied = (kind: "enroll" | "cli") => {
    setCopied(kind);
    if (copiedTimer.current !== null) window.clearTimeout(copiedTimer.current);
    copiedTimer.current = window.setTimeout(() => {
      if (mountedRef.current) setCopied(null);
      copiedTimer.current = null;
    }, 2000);
  };

  const copyEnroll = async () => {
    if (!enrollCode) return;
    const ok = await copyText(enrollCode);
    if (ok) markCopied("enroll");
    else log.warn("[RemoteNodeEnrollPanel] clipboard copy failed");
  };

  const copyCli = async () => {
    if (!command) return;
    const ok = await copyText(command);
    if (ok) markCopied("cli");
    else log.warn("[RemoteNodeEnrollPanel] clipboard copy failed");
  };

  if (loadingRemote && !remote) {
    return (
      <div className="flex flex-col items-center gap-3 py-8">
        <Loader2 className="h-5 w-5 animate-spin text-text-tertiary" />
        <p className="text-xs text-text-tertiary">{t("agentHub.remoteNodeLoading")}</p>
      </div>
    );
  }

  if (remoteError && !remote) {
    return (
      <div className="flex flex-col items-center gap-3 py-6 text-center">
        <p className="text-xs text-error">
          {t("agentHub.remoteNodeLoadFailed", { error: remoteError })}
        </p>
        <button
          type="button"
          onClick={() => mintRemoteCommand(true)}
          className="btn-solid flex items-center gap-1.5 rounded px-3 py-1.5 text-xs font-medium"
        >
          <RefreshCw className="h-3.5 w-3.5" />
          {t("agentHub.retry")}
        </button>
      </div>
    );
  }

  if (!remote) return null;

  return (
    <div className="flex flex-col items-center gap-3">
      <p className="max-w-[22rem] text-center text-xs leading-relaxed text-text-tertiary">
        {t("agentHub.remoteNodeGuideText")}
      </p>

      {qrDataUrl ? (
        <img
          src={qrDataUrl}
          alt={t("agentHub.remoteNodeQrAlt")}
          className="h-52 w-52 rounded-md bg-white p-2 shadow-sm"
        />
      ) : (
        <div className="flex h-52 w-52 items-center justify-center rounded-md border border-border-divider bg-panel-inset">
          <Loader2 className="h-5 w-5 animate-spin text-text-tertiary" />
        </div>
      )}

      <div className="flex w-full max-w-[22rem] flex-col items-center gap-1.5">
        <p className="text-xs text-text-secondary">
          {t("agentHub.remoteNodeTokenHint", {
            hours: Math.round(remote.token_ttl_seconds / 3600),
          })}
        </p>

        {editingHost ? (
          <label className="flex w-full items-center gap-2 text-xs text-text-secondary">
            <span className="shrink-0">{t("agentHub.remoteNodeHost")}</span>
            <StyledInput
              value={host}
              autoFocus
              onChange={(e) => setHost(sanitizeHost(e.target.value))}
              onBlur={() => setEditingHost(false)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === "Escape") setEditingHost(false);
              }}
              aria-label={t("agentHub.remoteNodeHost")}
              className="min-w-0 flex-1 text-xs"
            />
          </label>
        ) : (
          <button
            type="button"
            onClick={() => setEditingHost(true)}
            className="max-w-full truncate text-center text-10 text-text-tertiary hover:text-text-secondary"
            title={t("agentHub.remoteNodeHostEdit")}
          >
            {gatewayDisplay}
          </button>
        )}

        {isLoopback(effectiveHost) && (
          <p className="text-center text-10 text-warning-quiet">
            {t("agentHub.remoteNodeHostHint")}
          </p>
        )}
      </div>

      <div className="flex w-full max-w-[22rem] flex-col gap-2">
        <button
          type="button"
          onClick={() => void copyEnroll()}
          disabled={!enrollCode}
          className="btn-solid flex w-full items-center justify-center gap-1.5 rounded px-3 py-2 text-xs font-medium disabled:opacity-50"
        >
          {copied === "enroll" ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
          {copied === "enroll"
            ? t("agentHub.remoteNodeCopied")
            : t("agentHub.remoteNodeCopyEnroll")}
        </button>

        {/* Keep full enroll string in DOM for tests / accessibility / paste fallback. */}
        {enrollCode && (
          <p className="sr-only" aria-hidden="false">
            {enrollCode}
          </p>
        )}

        <button
          type="button"
          onClick={() => setCliOpen((v) => !v)}
          aria-expanded={cliOpen}
          className="flex w-full items-center justify-center gap-1 text-10 text-text-tertiary hover:text-text-secondary"
        >
          <ChevronDown
            className={`h-3 w-3 transition-transform ${cliOpen ? "rotate-180" : ""}`}
          />
          {t("agentHub.remoteNodeCliToggle")}
        </button>

        {cliOpen && command && (
          <div className="flex flex-col gap-2 rounded-md border border-border-divider bg-panel-inset p-2">
            <code className="block max-h-20 overflow-auto break-all text-10 leading-relaxed text-text-tertiary">
              {command}
            </code>
            <button
              type="button"
              onClick={() => void copyCli()}
              className="flex items-center justify-center gap-1 self-end rounded border border-border-outer px-2 py-1 text-10 text-text-secondary hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15"
            >
              {copied === "cli" ? <Check className="h-3 w-3" /> : <Copy className="h-3 w-3" />}
              {copied === "cli"
                ? t("agentHub.remoteNodeCopied")
                : t("agentHub.remoteNodeCopyCli")}
            </button>
          </div>
        )}
      </div>

      <button
        type="button"
        onClick={() => mintRemoteCommand(true)}
        disabled={loadingRemote}
        className="flex items-center gap-1 text-10 text-text-tertiary hover:text-text-secondary disabled:opacity-50"
      >
        <RefreshCw className={`h-3 w-3 ${loadingRemote ? "animate-spin" : ""}`} />
        {t("agentHub.remoteNodeRefresh")}
      </button>
    </div>
  );
}
