import { useEffect, useRef, useState } from "react";
import { ChevronDown, Globe, Wifi } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { probeGateways, type ProbeResult } from "../../lib/gateway-probe";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";
import { cn } from "../../lib/utils";
import { Tooltip } from "../common/Tooltip";
import { Dropdown } from "../common/Dropdown";
import type { GatewayMode } from "../../lib/types";

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
 *   - the list is `settingsStore.gatewayUrlHistory` — the LRU of URLs
 *     that ACTUALLY connected (SplashScreen/AppLayout record on the
 *     connection lifecycle, never on a keystroke), so a typo or a dead
 *     host can never show up here as a candidate.
 *   - probing is `probeGateways` (per-host AbortController, 1.5s budget,
 *     `onSettled` per host) driven through local state. Deliberately NOT
 *     `probeKnownCandidates()`: that one is the boot-time failover path
 *     — it returns 0 while connected, returns 0 in local mode, and
 *     writes into `gatewayStore.candidates`, which SplashScreen's chooser
 *     owns. This menu asks the same question in the opposite situation
 *     (connected, browsing) so it must not share that state.
 *   - switching is `applyGatewayUrl` + `checkHealth` — so the
 *     relay→remote http:// correction and the authStore re-validation
 *     happen here too. The pill's old candidate pick used the same pair.
 *   - the trigger glyph is `Globe`, the SAME icon SettingsPage gives its
 *     gateway tab (SettingsPage.tsx:38). The title-bar button and that
 *     tab are two doors to one feature, so they must not learn two
 *     shapes — a user who finds `Globe` in Settings must meet the same
 *     mark in the title bar. `Server` was tried first and is wrong:
 *     it reads as "a machine running a service", not "which network am I
 *     on", and it was the only lucide glyph this app used exactly once.
 *   - row tints come from `listRowSelected` / `listRowHover`
 *     (lib/ui-styles.ts) — the app-wide accent-tint family every other
 *     list (popups, side panels, file tree, search results) uses, so the
 *     cursor passing over a row reads the same here as everywhere else.
 *
 * The mode control is a label + `<select>` listbox, not three radios.
 * Two reasons, both measured against the real widget:
 *   - the three labels ("Local" / "Remote (LAN)" / "Relay (WAN)")
 *     wrapped onto two rows inside the popover, which reads as a
 *     broken layout rather than a compact mode picker;
 *   - a native listbox already has the keyboard and screen-reader
 *     contract (`role=listbox`, arrow keys, type-ahead) a hand-rolled
 *     popover list would have to re-implement. The shared `Dropdown` is
 *     the app's one `<select>` wrapper — the same one the settings tabs
 *     use, so the control looks identical everywhere.
 *
 * The mode listbox is a FILTER, not a config editor: picking a row is
 * what commits. `setGatewayMode` pushes to Rust and rebuilds MQTT, and
 * firing that from a stray selection inside a title-bar popover would
 * switch the connection out from under the user with no address chosen.
 * The live configuration's mode still marks the current row.
 */
export function GatewaySwitcherMenu() {
  const { t } = useTranslation();
  const gatewayUrl = useSettingsStore((s) => s.gatewayUrl);
  const gatewayUrlHistory = useSettingsStore((s) => s.gatewayUrlHistory);
  const configuredMode = useSettingsStore((s) => s.gatewayMode);
  const applyGatewayUrl = useSettingsStore((s) => s.applyGatewayUrl);
  const status = useGatewayStore((s) => s.status);
  const localState = useGatewayStore((s) => s.localState);
  const startLocalGateway = useGatewayStore((s) => s.startLocalGateway);
  const checkHealth = useGatewayStore((s) => s.checkHealth);
  const gatewayUnreachable = useGatewayStore((s) => s.gatewayUnreachable);

  const [open, setOpen] = useState(false);
  // Starts on the live mode so the panel opens on the relevant list
  // instead of defaulting to a tab the user never uses.
  const [tab, setTab] = useState<GatewayMode>(configuredMode);
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
  // events over the app behind it.
  useEffect(() => {
    if (!open) return;
    const onDocDown = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDocDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDocDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  // Remote/relay browse the recorded history; local is pinned to one
  // address (there is no second address to discover on the loopback).
  // The current URL leads the list even if history is empty — a
  // first-run install has a URL but no history yet, and showing an empty
  // panel would leave the user with no address to look at.
  const rows =
    tab === "local"
      ? [gatewayUrl]
      : Array.from(new Set([gatewayUrl, ...gatewayUrlHistory]));

  // Probe the browsed list. `onSettled` folds each host in as it lands
  // so one black-holed address can't hold a reachable one hostage for
  // the full budget.
  useEffect(() => {
    if (!open || tab === "local") return;
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
  }, [open, tab, probeRun]);

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
    // Same entry point as the outage chip's candidate pick: persists the
    // URL, pushes it to Rust (rebuilds MQTT), persists the mode, and
    // re-validates the auth token against the new Gateway.
    applyGatewayUrl(url);
    void checkHealth();
  };

  const isLocalTab = tab === "local";

  // Outage reason, mirroring the wording the pill always used. Kept as
  // three states because "which address is dead" and "is the process
  // even running" have different fixes, and only the second one is
  // answered by the Start button below.
  const isLocal = configuredMode === "local";
  const isStarting = localState === "starting";
  const outageLabel = isLocal
    ? isStarting
      ? t("splashScreen.bannerLocalStarting")
      : t("splashScreen.bannerLocalDown")
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
            className={cn(
              "flex h-8 w-8 items-center justify-center rounded transition-colors",
              open
                ? "bg-zinc-300 text-text-primary dark:bg-zinc-700"
                : "text-text-secondary hover:bg-zinc-300 dark:hover:bg-zinc-700",
            )}
          >
            <Globe className="h-3.5 w-3.5" />
          </button>
        </Tooltip>
      )}

      {open && (
        <div className="absolute right-0 z-30 mt-1 w-80 rounded-lg border border-border-outer bg-page-bg p-2 shadow-lg">
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

          {/* Label left, listbox right — one row at any language width.
              The label is the settings tab's own wording so the two
              screens name the concept identically. */}
          <div className="flex items-center gap-2 border-b border-border-divider px-1 pb-2">
            <span className="shrink-0 text-xs text-text-tertiary">
              {t("settings.gatewayMode")}
            </span>
            <Dropdown
              size="small"
              value={tab}
              onChange={(v) => setTab(v as GatewayMode)}
              options={[
                { value: "local", label: t("settings.local") },
                { value: "remote", label: t("settings.remote") },
                { value: "relay", label: t("settings.relay") },
              ]}
              aria-label={t("settings.gatewayMode")}
              className="ml-auto min-w-0 flex-1"
            />
          </div>

          {/* Local: one address, no discovery. The lifecycle row is the
              useful part (a local Gateway that isn't running is the
              common failure), so it gets Start — the same action
              SettingsPage's local card offers. */}
          {isLocalTab ? (
            <div className="pt-2">
              <p className="truncate px-1 font-mono text-xs" title={gatewayUrl}>
                {gatewayUrl}
              </p>
              <div className="mt-1.5 flex items-center justify-between gap-2 px-1">
                <StatusRow status={status} localState={localState} />
                {localState !== "running" && localState !== "starting" && (
                  <button
                    type="button"
                    onClick={() => void startLocalGateway()}
                    className="btn-solid shrink-0 rounded-md px-2 py-1 text-xs font-medium"
                  >
                    {t("settings.startGateway")}
                  </button>
                )}
              </div>
            </div>
          ) : (
            <ul className="mt-1 flex max-h-64 flex-col gap-0.5 overflow-auto">
              {rows.map((url) => {
                const current = url === gatewayUrl;
                // A row is switchable ONLY once its probe has positively
                // answered `/health` — a whitelist, not a blacklist of
                // the failures. Anything else (still in flight, rejected,
                // never probed) is disabled, because clicking it can only
                // do one thing: swap a working Gateway for a connection
                // that then fails `checkHealth`, stranding the user.
                //
                // Gating on `fail` alone left the exact window an attacker
                // — or a user — reaches for: open the menu, click before
                // the first probe settles. A few hundred ms is all it takes,
                // and "the row greyed out a moment ago" is the whole answer
                // to why it now can't be clicked.
                //
                // No local-mode exemption is needed: that tab renders
                // `rows = [gatewayUrl]`, i.e. the current address alone,
                // which `current` already disables.
                const reachable = probes[url]?.ok === true;
                return (
                  <li key={url}>
                    <button
                      type="button"
                      onClick={() => switchTo(url)}
                      disabled={current || !reachable}
                      title={url}
                      className={cn(
                        "flex w-full items-center justify-between gap-2 rounded px-2 py-1.5 text-left transition-colors",
                        // `listRowSelected` bundles its own `hover:` arm —
                        // without it the hover tint would win over the
                        // selection while the cursor sits on the row
                        // (Tailwind emits hover variants after plain
                        // utilities), and the active row would grey out
                        // under the cursor exactly when you click it.
                        current ? listRowSelected : listRowHover,
                        // A disabled row must not light up under the
                        // cursor while refusing the click (`listRowHover`
                        // is a bare `hover:` utility with no disabled arm).
                        !reachable && "cursor-not-allowed opacity-50",
                      )}
                    >
                      <span className="truncate font-mono text-xs">{url}</span>
                      <RowVerdict
                        probe={probes[url]}
                        current={current}
                        configuredMode={configuredMode}
                      />
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
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
  configuredMode,
}: {
  probe: ProbeResult | undefined;
  current: boolean;
  configuredMode: GatewayMode;
}) {
  const { t } = useTranslation();
  const verdict: Verdict = !probe ? "pending" : probe.ok ? "ok" : "fail";
  if (current) {
    // Relay mode's own label matters here: an https:// relay address and
    // a plain remote address look alike in the list, and picking one
    // silently flips the mode (applyGatewayUrl's relay→remote fix).
    return (
      <span className="flex shrink-0 items-center gap-1.5 text-11 text-text-secondary">
        {t(`settings.${configuredMode}`)}
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

/** Connection status line — the same wording as SettingsPage's row so
 *  the two never disagree. localState wins in local mode because that's
 *  where the Gateway actually lives (a remote address can be answered
 *  by an externally-owned process). */
function StatusRow({
  status,
  localState,
}: {
  status: string;
  localState: string;
}) {
  const { t } = useTranslation();
  const local = localState === "running";
  const starting = localState === "starting";
  const tone = local || status === "connected"
    ? "text-emerald-600 dark:text-emerald-400"
    : status === "error"
      ? "text-red-600 dark:text-red-400"
      : "text-text-tertiary";
  const label = starting
    ? t("settings.starting")
    : local || status === "connected"
      ? t("settings.connected")
      : status === "error"
        ? t("settings.error")
        : t("settings.disconnected");
  return (
    <span className={cn("flex items-center gap-2 text-xs", tone)}>
      <span
        className={cn(
          "h-2 w-2 rounded-full",
          local || status === "connected"
            ? "bg-emerald-500"
            : status === "error"
              ? "bg-red-500"
              : "bg-zinc-400",
        )}
      />
      {label}
    </span>
  );
}