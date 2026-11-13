import { useEffect, useRef, useState } from "react";
import { ChevronDown, Globe, Wifi } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { probeGateways, type ProbeResult } from "../../lib/gateway-probe";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";
import { cn } from "../../lib/utils";
import { Tooltip } from "../common/Tooltip";
import { gatewayAddressOptions, isLoopbackGatewayUrl } from "../../lib/gatewayUrl";
import { titleBarMenuPanel, titleBarMenuTrigger, useDismissMenu } from "./titleBarMenu";

/** Probe verdict for one row. Derived from the raw `ProbeResult` rather
 *  than a separate enum: `undefined` = still in flight ("pending"), the
 *  rest is read straight off the result, so the row can never disagree
 *  with the probe that produced it. */
type Verdict = "pending" | "ok" | "fail";

/**
 * Title-bar gateway switcher — a minimized Settings ▸ Gateway, and the
 * app's single Gateway outage affordance.
 *
 * This component absorbed `GatewayStatusChip` (an amber pill that sat
 * next to the "ACowork" wordmark). Two controls answered overlapping
 * questions — which Gateway am I on, and how do I get out of an outage —
 * and the outage one lived in the opposite corner from the menu that
 * actually fixes it. One trigger now does both: a bare `Globe` glyph
 * while healthy, the pill on `gatewayUnreachable`.
 *
 * The merge is a move, not a rewrite. Every reason the pill existed is
 * preserved verbatim in place:
 *   - it is a *steady-state* indicator, not a toast — a drop after a
 *     sleep/wake network switch persists until the user acts, so the
 *     affordance must be permanent too. Nothing here auto-dismisses.
 *   - it stays in the title bar rather than a full-width banner so it
 *     is visible on every view without stealing content height (the old
 *     amber strip pushed 36px across the top of every view and
 *     out-shouted content carrying a one-sentence problem).
 *   - the solid amber fill + breathing icon is where the attention
 *     comes from; the pulse is pinned to the icon so the label doesn't
 *     flicker and read as a rendering bug.
 *   - visibility follows `gatewayUnreachable` — the connectivity
 *     module's user-facing "not CONNACKed for 5s, whatever the cause"
 *     (lib/connectivity/gatewayConnectivity.ts) — NOT `status === "error"`,
 *     because the death classifier stays silent on a black-holed path,
 *     which is exactly the outage the user must see.
 *
 * Answers the two questions the title bar was missing:
 *   1. WHICH gateway am I on right now? (the URL is otherwise only
 *      visible after opening Settings)
 *   2. what else is alive on this network?
 *
 * Reuses the pieces instead of re-deriving them:
 *   - the list is `gatewayAddressOptions(gatewayUrl, history, lastGoodUrl)`
 *     — `localhost` is ALWAYS the first row (an ordinary address, per
 *     ADR-115), then the last address that actually connected, then the
 *     current one, then the LRU of URLs that ACTUALLY connected
 *     (`App.tsx` records on the connection lifecycle, never on a keystroke),
 *     so a typo or a dead host can never show up here as a candidate.
 *   - probing is `probeGateways` (per-host AbortController, 1.5s budget,
 *     `onSettled` per host) driven through local state. Deliberately NOT
 *     `probeKnownCandidates()`: that one is the boot-time failover path —
 *     it returns 0 while recovered and writes into
 *     `gatewayStore.candidates`, which SplashScreen's chooser owns. This
 *     menu asks the same question in the opposite situation (connected,
 *     browsing) so it must not share that state.
 *   - switching is `switchGateway` — the whole pipeline (config push, local
 *     spawn when the address is loopback, MQTT rebuild, auth re-init). A
 *     bare URL write + health probe could never escape a wrong remote
 *     address. It reboots the app through the splash, which is the honest
 *     shape of "the backend changed".
 *   - the trigger glyph is `Globe`, the SAME icon SettingsPage gives its
 *     gateway tab. The title-bar button and that tab are two doors to one
 *     feature, so they must not learn two shapes — a user who finds `Globe`
 *     in Settings must meet the same mark in the title bar. `Server` was
 *     tried first and is wrong: it reads as "a machine running a service",
 *     not "which network am I on", and it was the only lucide glyph this
 *     app used exactly once.
 *   - row tints come from `listRowSelected` / `listRowHover`
 *     (lib/ui-styles.ts) — the app-wide accent-tint family every other
 *     list (popups, side panels, file tree, search results) uses, so the
 *     cursor passing over a row reads the same here as everywhere else.
 *
 * There is no mode control. ADR-115 deleted the local/remote/relay picker:
 * the mode is an inference from the address, so a "mode" row here would be a
 * second, disagreeing way to write the same fact. The header names the one
 * concept (server address) and the current row is the selected one.
 */
export function GatewaySwitcherMenu({
  onOpenGatewaySettings,
}: {
  /** Jump to SettingsPage with the Gateway tab focused. TitleBar
   *  wires this from AppLayout because `currentView` / `settingsInitialTab`
   *  are owned there — same reason the avatar click handler lives there. */
  onOpenGatewaySettings?: () => void;
}) {
  const { t } = useTranslation();
  const gatewayUrl = useSettingsStore((s) => s.gatewayUrl);
  const gatewayUrlHistory = useSettingsStore((s) => s.gatewayUrlHistory);
  const lastGoodUrl = useSettingsStore((s) => s.lastGoodUrl);
  const localState = useGatewayStore((s) => s.localState);
  const startLocalGateway = useGatewayStore((s) => s.startLocalGateway);
  const checkHealth = useGatewayStore((s) => s.checkHealth);
  const gatewayUnreachable = useGatewayStore((s) => s.gatewayUnreachable);

  const [open, setOpen] = useState(false);
  // Per-row probe outcome. `undefined` means "still in flight" — a
  // distinct pre-settle state so a row never renders as "offline"
  // while its probe is running (a black-holed address would otherwise
  // flash red for 1.5s then correct).
  const [probes, setProbes] = useState<Record<string, ProbeResult>>({});
  const wrapRef = useRef<HTMLDivElement>(null);
  // Bumped per open so a close/open cycle re-probes: liveness decays
  // over time and a stale "online" from the last visit is a lie.
  const [probeRun, setProbeRun] = useState(0);

  // Close on outside click / Escape — the popover must never trap pointer
  // events over the app behind it. The rule lives in the shared title-bar
  // menu shell so every popover here dismisses identically.
  useDismissMenu(wrapRef, open, () => setOpen(false));

  // ADR-115: one address list for every topology. `localhost` is always
  // present; the last connected address is pinned near the top. No mode
  // filter — the mode is derived from whichever row the user picks.
  const rows = gatewayAddressOptions(gatewayUrl, gatewayUrlHistory, lastGoodUrl);

  // Probe the browsed list. `onSettled` folds each host in as it lands
  // so one black-holed address can't hold a reachable one hostage for
  // the full budget.
  useEffect(() => {
    if (!open) return;
    const targets = rows;
    setProbes({});
    let cancelled = false;
    void probeGateways(targets, {
      onSettled: (r: ProbeResult) => {
        if (cancelled) return;
        setProbes((prev) => ({ ...prev, [r.url]: r }));
      },
    });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, probeRun, rows.join("|")]);

  const toggle = () => {
    setOpen((v) => {
      if (!v) setProbeRun((n) => n + 1);
      return !v;
    });
  };

  // Retry = re-probe health AND bump the panel's own probe. The two are
  // not redundant: `checkHealth` asks the *current* address (the
  // network may have just come back), while `probeRun` re-answers "which
  // OTHERS are alive" — that verdict decays, and a stale "online" from
  // the last visit is a lie. The bump happens even though the panel is
  // already open, which the normal open path only does on a transition.
  const onRetry = () => {
    void checkHealth();
    setProbeRun((n) => n + 1);
  };

  const switchTo = (url: string) => {
    if (url === gatewayUrl) return;
    setOpen(false);
    // Reboot the whole connection pipeline: config push, local spawn when
    // the address is loopback, MQTT rebuild and auth re-init. A bare URL
    // write + health probe could never escape a wrong remote address.
    void useSettingsStore.getState().switchGateway(url);
  };

  // Outage reason, mirroring the wording the pill always used. Kept as
  // three states because "which address is dead" and "is the process
  // even running" have different fixes, and only the second one is
  // answered by the Start button below.
  // Derived from the address — there is no independent mode flag to read,
  // and the copy no longer says "local Gateway": since ADR-115 the user has
  // one setting (a server address), not a local/remote mode to interpret.
  const isLocal = isLoopbackGatewayUrl(gatewayUrl);
  const isStarting = localState === "starting";
  const outageLabel = isLocal
    ? isStarting
      ? t("splashScreen.bannerStarting")
      : t("splashScreen.bannerNotRunning")
    : t("splashScreen.bannerRemoteDown");

  return (
    <div
      ref={wrapRef}
      className="relative"
      // The title bar is a native drag region; without this the mousedown
      // that opens the popover also starts a window drag.
      onMouseDown={(e) => e.stopPropagation()}
    >
      {/* One trigger, two shapes. Healthy: a bare 32px glyph that sits in
          the title-bar cluster like its neighbours. Unreachable: the
          amber pill from the old status chip, unchanged — the fill is
          what carries the attention, and swapping it for a tinted icon
          would under-report a problem that persists until acted on.
          Both shapes name themselves for screen readers: the pill's
          visible label flips with the outage reason, so it cannot also
          be the accessible name without lying about the state. */}
      {gatewayUnreachable ? (
        <button
          type="button"
          aria-expanded={open}
          aria-label={t("titleBar.gatewayStatus")}
          onClick={toggle}
          className="inline-flex h-6 shrink-0 items-center gap-1.5 rounded-full bg-amber-500 pr-2 pl-2.5 text-xs font-medium text-white transition-colors hover:bg-amber-600 dark:bg-amber-600 dark:hover:bg-amber-500"
        >
          {/* Breathing = the outage is live and unattended. Pinned to the
              icon, not the whole pill: pulsing the label would flicker the
              text and read as a rendering bug. */}
          <Wifi className="h-3.5 w-3.5 shrink-0 animate-pulse" />
          <span className="shrink-0">{outageLabel}</span>
          <ChevronDown
            className={cn("h-3 w-3 shrink-0 opacity-80 transition-transform", open && "rotate-180")}
          />
        </button>
      ) : (
        <Tooltip content={t("titleBar.gateway")} position="bottom">
          <button
            type="button"
            aria-expanded={open}
            aria-label={t("titleBar.gateway")}
            onClick={toggle}
            className={titleBarMenuTrigger(open)}
          >
            <Globe className="h-3.5 w-3.5" />
          </button>
        </Tooltip>
      )}

      {open && (
        <div className={titleBarMenuPanel}>
          {/* Outage banner — only while unreachable. It replaces the old
              chip popover's candidate block wholesale: the address list
              below already IS the candidate list, and it is better —
              it probes every recorded URL, marks each verdict in words,
              and refuses to hand the user an address that did not
              answer /health. Re-listing the reachable subset above it
              (which is what the chip did) would offer the same URLs in
              a second, differently-decorated list. So this block is
              reduced to the one thing the list cannot offer: the reason
              plus the two actions that need no address at all. */}
          {gatewayUnreachable && (
            <div className="mb-1.5 border-b border-border-divider pb-1.5">
              <p className="px-1 pb-1.5 text-xs font-medium text-text-secondary">{outageLabel}</p>
              <div className="flex justify-end gap-1.5">
                {isLocal && !isStarting && (
                  <button
                    type="button"
                    onClick={() => void startLocalGateway()}
                    className="btn-solid rounded-md px-2 py-1 text-xs font-medium"
                  >
                    {t("splashScreen.bannerStart")}
                  </button>
                )}
                <button
                  type="button"
                  onClick={onRetry}
                  className="btn-solid rounded-md px-2 py-1 text-xs font-medium"
                >
                  {t("splashScreen.bannerRetry")}
                </button>
              </div>
            </div>
          )}

          {/* ADR-115: no mode filter. One header row names the single
              concept (server address) and keeps the Details shortcut. */}
          <div className="flex items-center justify-between gap-2 border-b border-border-divider px-1 pb-2">
            <span className="shrink-0 text-xs text-text-tertiary">
              {t("connection.serverAddress")}
            </span>
            {onOpenGatewaySettings && (
              <button
                type="button"
                onClick={onOpenGatewaySettings}
                className="btn-solid shrink-0 rounded-md px-2 py-1 text-xs font-medium"
              >
                {t("titleBar.gatewayDetails")}
              </button>
            )}
          </div>

          <ul className="mt-1 flex max-h-64 flex-col gap-0.5 overflow-auto">
            {rows.map((url) => {
              const current = url === gatewayUrl;
              // A row is switchable ONLY once its probe has positively
              // answered `/health` — a whitelist, not a blacklist of the
              // failures. The one exemption is `localhost`: when no local
              // Gateway is online the boot pipeline spawns one, so an
              // offline loopback row is still a valid way out of a bad
              // remote address.
              const reachable =
                probes[url]?.ok === true || isLoopbackGatewayUrl(url);
              return (
                <li key={url}>
                  <button
                    type="button"
                    onClick={() => switchTo(url)}
                    disabled={current || !reachable}
                    title={url}
                    className={cn(
                      "flex w-full items-center justify-between gap-2 rounded px-2 py-1.5 text-left transition-colors",
                      current ? listRowSelected : listRowHover,
                      !reachable && "cursor-not-allowed opacity-50",
                    )}
                  >
                    <span className="truncate font-mono text-xs">{url}</span>
                    <RowVerdict
                      probe={probes[url]}
                      current={current}
                    />
                  </button>
                </li>
              );
            })}
          </ul>
        </div>
      )}
    </div>
  );
}

/** Probe verdict for one row. Green dot for the ONE state that makes a row
 *  clickable: a host that actually answered `/health`. Everything else is
 *  spelled out in words — "testing…" while the probe is still in flight,
 *  "Disconnected" once it is rejected — because both of those rows are
 *  disabled, and a disabled row carrying a bare dot that could also mean
 *  "green but I can't tell" leaves the user hunting for the difference.
 *  The current address shows its mode label instead — the dot's question
 *  is "which OTHERS are reachable".
 *
 *  A reachable row also carries its latency, which is what the old
 *  outage chip showed on its candidate rows. With more than one live
 *  host on the list the dot alone does not order them, and the whole
 *  point of probing on outage is to pick the right one. */
function RowVerdict({
  probe,
  current,
}: {
  probe: ProbeResult | undefined;
  current: boolean;
}) {
  const { t } = useTranslation();
  const verdict: Verdict = !probe ? "pending" : probe.ok ? "ok" : "fail";
  if (current) {
    // ADR-115: there is no user-visible mode to label any more. The row
    // the app is connected to just says so; the dot's question is
    // "which OTHERS are reachable".
    return (
      <span className="flex shrink-0 items-center gap-1.5 text-11 text-text-secondary">
        {t("settings.connected")}
        <span className="h-1.5 w-1.5 rounded-full bg-emerald-500" />
      </span>
    );
  }
  if (verdict === "fail") {
    return (
      <span className="shrink-0 text-11 text-text-tertiary">
        {t("settings.disconnected")}
      </span>
    );
  }
  if (verdict === "pending") {
    // Not disabled for a reason the user can see — the row is greyed out
    // because it is still being probed, and a bare dot next to it would
    // look like the "offline" mark the row can't undo yet.
    return (
      <span className="shrink-0 text-11 text-text-tertiary">
        {t("settings.testing")}
      </span>
    );
  }
  return (
    <span className="flex shrink-0 items-center gap-1.5">
      <span className="h-1.5 w-1.5 rounded-full bg-emerald-500" />
      <span className="text-11 text-text-tertiary tabular-nums">
        {Math.round(probe?.latencyMs ?? 0)}ms
      </span>
    </span>
  );
}
