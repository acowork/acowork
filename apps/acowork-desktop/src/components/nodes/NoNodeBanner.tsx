/**
 * "No usable node" empty state — shared by the Agent library dialog and the
 * first-run onboarding step.
 *
 * ADR-113: "there is no node to install onto" is a protocol state, not a
 * Gateway-mode condition. The two recovery paths are therefore identical in
 * every topology and are never gated on `gatewayMode`:
 *
 *   1. Start a node on THIS machine — an ordinary Desktop capability, an
 *      idempotent action that does not wait for enrollment and never chains
 *      an install.
 *   2. Start a node on ANOTHER machine — a real, copy-pasteable command
 *      built from this Gateway's MQTT port and a freshly minted enrollment
 *      token. Nothing is executed or observed from here.
 *
 * The node list (`GET /api/nodes`) stays the single source of truth for when
 * installing becomes possible. This component only explains the empty state.
 */
import { useEffect, useRef, useState } from "react";
import { Check, Copy, Loader2 } from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledInput } from "../common/StyledInput";
import { copyText } from "../../lib/clipboard";
import { log } from "../../lib/logger";

/** Mirror of the Tauri `RemoteNodeCommand` struct (snake_case on the wire). */
type RemoteNodeCommand = {
  mqtt_port: number;
  token: string;
  token_ttl_seconds: number;
  suggested_host: string;
};

const isLoopback = (host: string): boolean =>
  /^(127\.|localhost$|::1$|\[::1\]$|0\.0\.0\.0$)/i.test(host.trim());

/** The host is pasted verbatim into a copyable shell command. Keep only
 *  characters that are legal in a DNS name / IP literal, so a stray `;`,
 *  `$`, backtick or space can never turn the command into something else. */
const sanitizeHost = (value: string): string =>
  value.replace(/[^A-Za-z0-9._:[\]-]/g, "");

/** IPv6 literals need brackets before `:port`, otherwise the command is
 *  ambiguous (`::1:1883`). `suggested_host` comes from a URL without them.
 *  A leading `-` / `.` would make clap read the host as a CLI option, so
 *  strip those; `:` and `[` are kept because IPv6 literals start with them. */
const hostForCommand = (host: string): string => {
  const safe = host.replace(/^[-.]+/, "");
  return safe.includes(":") && !safe.startsWith("[") ? `[${safe}]` : safe;
};

export function NoNodeBanner({
  starting,
  onStart,
  detail = null,
}: {
  starting: boolean;
  onStart: () => void;
  /** Extra protocol-state explanation (e.g. nodes exist but are not ready
   *  or not manageable). Never a Gateway-mode statement. */
  detail?: string | null;
}) {
  const { t } = useTranslation();
  // Owned here: unmounting when a node appears resets everything, so a
  // later empty state never reopens with a stale guide or a spent token.
  const [guideOpen, setGuideOpen] = useState(false);
  const [remote, setRemote] = useState<RemoteNodeCommand | null>(null);
  const [remoteError, setRemoteError] = useState<string | null>(null);
  const [loadingRemote, setLoadingRemote] = useState(false);
  const [host, setHost] = useState("");
  const [copied, setCopied] = useState(false);

  // Guards against setting state after the banner unmounts (a node may
  // appear while the token request is still in flight).
  const mountedRef = useRef(true);
  const copiedTimer = useRef<number | null>(null);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      if (copiedTimer.current !== null) window.clearTimeout(copiedTimer.current);
    };
  }, []);

  /**
   * Mint exactly one token per guide expansion. Doing this in the toggle
   * handler (rather than an effect keyed on loading state) avoids the
   * self-cancelling effect: setting `loadingRemote` would change the deps,
   * run the cleanup, and drop the in-flight response.
   */
  const mintRemoteCommand = () => {
    if (remote || loadingRemote) return;
    setLoadingRemote(true);
    setRemoteError(null);
    void invoke<RemoteNodeCommand>("build_remote_node_command")
      .then((res) => {
        if (!mountedRef.current) return;
        setRemote(res);
        setHost(res.suggested_host);
      })
      .catch((e) => {
        if (mountedRef.current) setRemoteError(String(e));
      })
      .finally(() => {
        if (mountedRef.current) setLoadingRemote(false);
      });
  };

  const toggleGuide = () => {
    if (guideOpen) {
      setGuideOpen(false);
      return;
    }
    setGuideOpen(true);
    mintRemoteCommand();
  };

  const effectiveHost = sanitizeHost(host.trim() || remote?.suggested_host || "");
  const command =
    remote && effectiveHost
      ? `acowork-node start --gateway ${hostForCommand(effectiveHost)}:${remote.mqtt_port} --token ${remote.token}`
      : null;

  const copy = async () => {
    if (!command) return;
    const ok = await copyText(command);
    if (ok) {
      setCopied(true);
      if (copiedTimer.current !== null) window.clearTimeout(copiedTimer.current);
      copiedTimer.current = window.setTimeout(() => {
        if (mountedRef.current) setCopied(false);
        copiedTimer.current = null;
      }, 2000);
    } else {
      log.warn("[NoNodeBanner] clipboard copy failed");
    }
  };

  return (
    <div className="mb-3 rounded-md border border-border-outer bg-panel-block px-3 py-2.5">
      <div className="flex items-center gap-2">
        <div className="min-w-0 flex-1">
          <p className="text-xs font-medium text-text">{t("agentHub.noAvailableNode")}</p>
          <p className="mt-0.5 text-10 text-text-tertiary">
            {detail ?? t("agentHub.noNodeHint")}
          </p>
        </div>
        <button
          type="button"
          onClick={toggleGuide}
          aria-expanded={guideOpen}
          className="shrink-0 rounded px-2 py-1 text-xs text-text-secondary hover:bg-zinc-100 dark:hover:bg-zinc-700"
        >
          {t("agentHub.remoteNodeGuide")}
        </button>
        <button
          type="button"
          onClick={onStart}
          disabled={starting}
          title={starting ? t("agentHub.startLocalNodeStarting") : undefined}
          className="btn-accent flex shrink-0 items-center gap-1.5 rounded px-2.5 py-1 text-xs font-medium disabled:opacity-50"
        >
          {starting && <Loader2 className="h-3 w-3 animate-spin" />}
          {starting ? t("agentHub.startLocalNodeStarting") : t("agentHub.startLocalNode")}
        </button>
      </div>

      {guideOpen && (
        <div className="mt-2 border-t border-border-divider pt-2">
          <p className="text-10 text-text-tertiary">{t("agentHub.remoteNodeGuideText")}</p>

          {loadingRemote && (
            <p className="mt-1.5 flex items-center gap-1.5 text-10 text-text-tertiary">
              <Loader2 className="h-3 w-3 animate-spin" />
              {t("agentHub.remoteNodeLoading")}
            </p>
          )}
          {remoteError && (
            <div className="mt-1.5 flex items-center gap-2">
              <p className="min-w-0 flex-1 text-10 text-error">
                {t("agentHub.remoteNodeLoadFailed", { error: remoteError })}
              </p>
              <button
                type="button"
                onClick={mintRemoteCommand}
                className="shrink-0 rounded border border-border-outer px-2 py-0.5 text-10 text-text-secondary hover:bg-zinc-100 dark:hover:bg-zinc-700"
              >
                {t("agentHub.retry")}
              </button>
            </div>
          )}

          {remote && (
            <div className="mt-2 flex flex-col gap-1.5">
              <label className="flex items-center gap-2 text-10 text-text-secondary">
                <span className="shrink-0">{t("agentHub.remoteNodeHost")}</span>
                <StyledInput
                  value={host}
                  onChange={(e) => setHost(sanitizeHost(e.target.value))}
                  aria-label={t("agentHub.remoteNodeHost")}
                  className="min-w-0 flex-1 text-10"
                />
              </label>
              {isLoopback(effectiveHost) && (
                <p className="text-10 text-text-tertiary">{t("agentHub.remoteNodeHostHint")}</p>
              )}
              <div className="flex items-start gap-2">
                <code className="min-w-0 flex-1 overflow-x-auto rounded bg-zinc-100 px-2 py-1.5 text-10 text-text-secondary dark:bg-zinc-700">
                  {command}
                </code>
                <button
                  type="button"
                  onClick={() => void copy()}
                  className="flex shrink-0 items-center gap-1 rounded border border-border-outer px-2 py-1 text-10 text-text-secondary hover:bg-zinc-100 dark:hover:bg-zinc-700"
                >
                  {copied ? <Check className="h-3 w-3" /> : <Copy className="h-3 w-3" />}
                  {copied ? t("agentHub.remoteNodeCopied") : t("agentHub.remoteNodeCopy")}
                </button>
              </div>
              <p className="text-10 text-text-tertiary">
                {t("agentHub.remoteNodeTokenHint", { hours: Math.round(remote.token_ttl_seconds / 3600) })}
              </p>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
