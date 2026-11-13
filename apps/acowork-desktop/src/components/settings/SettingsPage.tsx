import { useState, useEffect, useCallback, type ReactNode } from "react";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useChatStore } from "../../stores/chatStore";
import { useServicesStore } from "../../stores/servicesStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { AgentListResponse, GatewayConfig, NodeInfo } from "../../lib/types";
import { fetchNodes } from "../../lib/gateway-api";
import { cn } from "../../lib/utils";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { ExpandableRow, ListBox, ListRow } from "../common/list";
import { RadioGroup } from "../common/RadioGroup";
import { Switch } from "../common/Switch";
import { getGatewayUrl, DEFAULT_THEME, DEFAULT_FONT_SIZE, DEFAULT_CONTENT_WIDTH, DEFAULT_OPACITY, DEFAULT_ACCENT_COLOR } from "../../lib/config";
import { AccentSwatches } from "../common/AccentSwatches";
import { Bug, HelpCircle, Monitor, User, SlidersHorizontal, Palette, Globe } from "lucide-react";
import { inputReadonly } from "../../lib/ui-styles";
import { StyledInput } from "../common/StyledInput";
import { Dropdown } from "../common/Dropdown";
import { GatewayAddressPicker } from "../connection/GatewayAddressPicker";
import { isHttpsUrl } from "../../lib/gatewayUrl";
import { ProfileTab } from "./ProfileTab";
import { ServicesPanel } from "./ServicesPanel";
import { SectionPane } from "../common/SectionPane";
import { OutlineSettingsIcon as SettingsIcon } from "../common/SettingsIcon";
import { Tooltip } from "../common/Tooltip";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { log } from "../../lib/logger";

type SettingsTab = "gateway" | "appearance" | "general" | "profile";

export function SettingsPage({ initialTab = "profile" }: { initialTab?: SettingsTab }) {
  const { t } = useTranslation();
  const [activeTab, setActiveTab] = useState<SettingsTab>(initialTab);

  const tabs: { id: SettingsTab; label: string; icon: ReactNode }[] = [
    { id: "profile", label: t("settings.tabProfile"), icon: <User className="h-3.5 w-3.5" /> },
    { id: "general", label: t("settings.tabGeneral"), icon: <SlidersHorizontal className="h-3.5 w-3.5" /> },
    { id: "appearance", label: t("settings.tabAppearance"), icon: <Palette className="h-3.5 w-3.5" /> },
    { id: "gateway", label: t("settings.tabGateway"), icon: <Globe className="h-3.5 w-3.5" /> },
  ];

  return (
    <SectionPane
      title={t("navBar.settings")}
      icon={<SettingsIcon className="h-3.5 w-3.5" />}
      items={tabs}
      selected={activeTab}
      onSelect={(id) => setActiveTab(id as SettingsTab)}
      storageKey="acowork-settings-list-width"
    >
      {/* Tab content — CSS visibility preserves component state across tab switches */}
      <div style={{ display: activeTab === "gateway" ? "block" : "none" }}><GatewayTab /></div>
      <div style={{ display: activeTab === "appearance" ? "block" : "none" }}><AppearanceTab /></div>
      <div style={{ display: activeTab === "general" ? "block" : "none" }}><GeneralTab /></div>
      <div style={{ display: activeTab === "profile" ? "block" : "none" }}><ProfileTab /></div>
    </SectionPane>
  );
}

/**
 * Gateway connection settings
 */
export function GatewayTab() {
  const { t } = useTranslation();
  const { status, health, localState, localOwnership, checkHealth, checkLocalStatus, startLocalGateway, stopLocalGateway } = useGatewayStore();
  const gatewayUrl = useSettingsStore((s) => s.gatewayUrl);
  const gatewayMode = useSettingsStore((s) => s.gatewayMode);
  const autoStartLocalNode = useSettingsStore((s) => s.autoStartLocalNode);
  const setAutoStartLocalNode = useSettingsStore((s) => s.setAutoStartLocalNode);
  const [testing, setTesting] = useState(false);
  const [agents, setAgents] = useState<AgentListResponse[]>([]);
  const [agentsLoading, setAgentsLoading] = useState(false);
  const [nodes, setNodes] = useState<NodeInfo[]>([]);
  const [nodesLoading, setNodesLoading] = useState(false);
  // The dedicated relay entry is the ONLY explicit topology switch. It is
  // kept separate from the server-address list because relay URLs are TLS
  // device domains, not alternative Gateway hosts.
  const [relayDraft, setRelayDraft] = useState("");
  const [starting, setStarting] = useState(false);
  const [stopping, setStopping] = useState(false);
  // Gateway's relay device id. A property of the Gateway we are TALKED to,
  // not of the section we are in — see the gw-id line in the relay section.
  const [relayGwId, setRelayGwId] = useState<string | null>(null);
  // Level-1 collapsible cards (same grammar as the Setup panel), default open
  const [gatewayConnectionOpen, setGatewayConnectionOpen] = useState(true);
  const [localGatewayOpen, setLocalGatewayOpen] = useState(true);
  const [gatewayConnOpen, setGatewayConnOpen] = useState(true);
  const [nodesOpen, setNodesOpen] = useState(true);
  // Radio = transport KIND, never local vs remote: direct covers localhost
  // and LAN addresses alike in ONE merged address list (that merge is the
  // whole point of ADR-115 — splitting them back into two sections would
  // resurrect the "two knobs" UI). Relay is the only other transport.
  // The radio is UI state only: the mode stays DERIVED from the committed
  // address, and expanding a section never switches anything by itself —
  // the store changes only through an address commit inside the section.
  const [section, setSection] = useState<"direct" | "relay">(
    gatewayMode === "relay" ? "relay" : "direct",
  );
  useEffect(() => {
    setSection(gatewayMode === "relay" ? "relay" : "direct");
  }, [gatewayMode]);

  const handleSectionChange = useCallback((s: "direct" | "relay") => {
    setSection(s);
  }, []);
  // P1: collapse state for the new diagnostic cards.
  const [servicesOpen, setServicesOpen] = useState(true);
  const [eventsOpen, setEventsOpen] = useState(true);
  // Per-node level-1 collapse state inside the Nodes section.
  const [openNodeIds, setOpenNodeIds] = useState<Record<string, boolean>>({});

  // On mount, sync both the HTTP health status and the local process
  // handle from the Rust side. This covers the case where the Gateway
  // was spawned by the SplashScreen boot path (which bypasses the store
  // action) — without this, `localState` would stay "idle" and the UI
  // would show a spurious "Start Gateway" button next to "Running".
  useEffect(() => {
    checkHealth();
    checkLocalStatus();
  }, [checkHealth, checkLocalStatus]);

  // The relay device id, read from the Gateway we are pointed at.
  //
  // Fetched once per (connection, address) rather than polled: the id never
  // changes while a Gateway is running, and the row renders in the relay
  // section of the connection card — polling here would put a 5 s timer on
  // every local-section user who has no tunnel at all. `RelayTunnelPanel`
  // keeps the 5 s poll for the one place where tunnel liveness actually
  // matters.
  //
  // `authFetch`'s global interceptor attaches the bearer token, so a bare
  // fetch is correct here: `/api/relay/status` is not a public path
  // (core http/auth_middleware.rs `is_public_path`) and the always-on account system (ADR-114)
  // rejects an anonymous caller.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const resp = await fetch(`${gatewayUrl}/api/relay/status`);
        if (cancelled) return;
        setRelayGwId(resp.ok ? ((await resp.json()) as RelayStatus).gw_id : null);
      } catch {
        if (!cancelled) setRelayGwId(null);
      }
    })();
    return () => { cancelled = true; };
  }, [gatewayUrl, status]);

  // A relay address must be the TLS device domain (`https://<gw-id>...`).
  // `relay_mqtt_wss_url` in chat_mqtt.rs refuses every other scheme, and a
  // plain-http relay address still answers /health — so validate before the
  // user commits instead of surfacing a red MQTT row later.
  const relayDraftInvalid = relayDraft.trim() !== "" && !isHttpsUrl(relayDraft);

  const handleTest = useCallback(async () => {
    // P1-5 (a): "测试连接" 按钮语义升级为"运行诊断"——
    // 同时刷新 health (banner 用) 和跑一遍 servicesStore 的全量探针。
    // 两者并行，避免 health 探活串行阻塞诊断。
    setTesting(true);
    await Promise.all([checkHealth(), useServicesStore.getState().diagnose()]);
    setTesting(false);
  }, [checkHealth]);

  const handleStartLocal = useCallback(async () => {
    setStarting(true);
    try {
      await startLocalGateway();
    } catch {
      // Error handled by store
    } finally {
      setStarting(false);
    }
  }, [startLocalGateway]);

  const handleStopLocal = useCallback(async () => {
    setStopping(true);
    try {
      await stopLocalGateway();
    } catch {
      // Error handled by store
    } finally {
      setStopping(false);
    }
  }, [stopLocalGateway]);

  const handleRestartLocal = useCallback(async () => {
    setStarting(true);
    try {
      await stopLocalGateway();
      await startLocalGateway();
    } catch {
      // Error handled by store
    } finally {
      setStarting(false);
    }
  }, [startLocalGateway, stopLocalGateway]);

  const fetchAgents = useCallback(async () => {
    setAgentsLoading(true);
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents`);
      if (resp.ok) {
        const data: AgentListResponse[] = await resp.json();
        setAgents(data.filter(a => a.alive));
      }
    } catch {
      // Gateway not reachable
    } finally {
      setAgentsLoading(false);
    }
  }, []);

  const fetchAll = useCallback(async () => {
    await Promise.all([
      fetchAgents(),
      (async () => {
        setNodesLoading(true);
        try {
          setNodes(await fetchNodes());
        } catch {
          // Gateway unreachable — empty topology, matches the prior
          // NodesTab behaviour.
          setNodes([]);
        } finally {
          setNodesLoading(false);
        }
      })(),
    ]);
  }, [fetchAgents]);

  useEffect(() => {
    if (status === "connected") {
      void fetchAll();
    }
  }, [status, fetchAll]);

  const toggleNode = useCallback((nodeId: string) => {
    setOpenNodeIds((prev) => ({ ...prev, [nodeId]: !prev[nodeId] }));
  }, []);

  const localIsRunning = gatewayMode === "local" && (status === "connected" || localState === "running");
  const localIsStarting = gatewayMode === "local" && (localState === "starting" || starting) && status !== "connected";
  // Whether Tauri itself owns the local Gateway process. Only in that case can
  // the user stop/restart it via the UI. If a Gateway is reachable but was
  // started outside of Tauri, we still show "Running" but no stop/restart buttons.
  const localIsTauriManaged = localState === "running";
  // A Gateway that answers at the configured URL but was NOT spawned by this
  // Desktop session (manual start, another machine, or kept running after a
  // previous quit). Desktop must never force-stop it — and the exit dialog
  // only appears for owned processes (tray quit handler).
  const localIsForeign = localOwnership === "foreign" && localIsRunning;

  return (
    <div className="max-w-lg space-y-4">
      {/* Connection card — radio box + sectioned controls (ADR-115). The
          radio picks a transport KIND, not a mode: direct is one address
          list with localhost as an ordinary entry; the mode stays derived
          from the committed address. */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={gatewayConnectionOpen}
          onToggle={() => setGatewayConnectionOpen((v) => !v)}
          title={t("settings.gatewayConnection")}
          ariaLabel={t("settings.gatewayConnection")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <RadioGroup
            name="gatewayConnectionKind"
            value={section}
            options={[
              { label: t("connection.direct"), value: "direct" as const },
              { label: t("connection.viaRelay"), value: "relay" as const },
            ]}
            onChange={handleSectionChange}
          />

          {section === "direct" && (
            <div className="mt-3">
              {/* THE address list — localhost, LAN and history entries all
                  live here together; picking any row is a switch. Status /
                  start-stop for the local machine stays in the Local
                  Gateway card below. */}
              <GatewayAddressPicker
                onSelect={(url) => void useSettingsStore.getState().switchGateway(url)}
                onReset={() => void useSettingsStore.getState().resetConnectionSettings()}
              />

              {/* ADR-087: only a non-loopback address needs this machine's
                  already-enrolled local Node resumed on launch. */}
              {gatewayMode === "remote" && (
                <div className="mt-3 border-t border-border-divider pt-3">
                  <div className="flex items-center justify-between gap-2">
                    <span className="flex flex-1 items-center gap-1.5 text-xs">
                      {t("settings.autoStartLocalNode")}
                      <HelpHint content={t("settings.autoStartLocalNodeHelp")} />
                    </span>
                    <Switch checked={autoStartLocalNode} onChange={setAutoStartLocalNode} />
                  </div>
                </div>
              )}
            </div>
          )}

          {section === "relay" && (
            <div className="mt-3">
              {/* Dedicated relay entry. Relay stays its own path (WSS MQTT +
                  access-token CONNECT), so it is not mixed into the address
                  list. It is the only way to set the derived `relay` mode. */}
              <label
                htmlFor="gateway-relay-input"
                className="mb-1 block text-xs text-text-tertiary"
              >
                {t("connection.relayEntry")}
              </label>
              <div className="flex gap-2">
                <StyledInput
                  id="gateway-relay-input"
                  type="text"
                  value={relayDraft}
                  onChange={(e) => setRelayDraft(e.target.value)}
                  placeholder="https://<gw-id>.relay.example.com"
                  spellCheck={false}
                  fontMono
                  className="w-full min-w-0"
                />
                <button
                  type="button"
                  onClick={() => void useSettingsStore.getState().setRelayGateway(relayDraft)}
                  disabled={!relayDraft.trim() || relayDraftInvalid}
                  className="btn-accent shrink-0 rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
                >
                  {t("connection.connect")}
                </button>
              </div>
              {relayDraftInvalid && (
                <p role="alert" className="mt-1 text-11 text-red-500">
                  {t("settings.relayUrlMustBeHttps")}
                </p>
              )}
              {/* The relay device id is a property of the Gateway, not of a
                  mode, and is needed BEFORE the relay address can be filled. */}
              <div className="mt-2 flex items-center gap-1.5 text-xs text-text-tertiary">
                <span className="truncate">
                  {t("settings.relayGwId")}:{" "}
                  <span className="font-mono">{relayGwId ?? "—"}</span>
                </span>
                <GatewayGwIdHelp gwId={relayGwId} />
              </div>
            </div>
          )}
        </ExpandableRow>
      </ListBox>

      {/* Local mode: status + controls */}
      {gatewayMode === "local" && (
        <ListBox dividers={false}>
          <ExpandableRow
            open={localGatewayOpen}
            onToggle={() => setLocalGatewayOpen((v) => !v)}
            title={t("settings.localGateway")}
            ariaLabel={t("settings.localGateway")}
            bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
          >

          <div className="flex items-center gap-2 text-xs">
            <span className="text-text-tertiary">{t("settings.status")}</span>
            {/* Connection state, not branding: the "on" dot uses the same
                green as the node rows below (NodesTree) and the service
                rows in ServicesPanel, so every online marker in this tab
                reads alike. Deliberately NOT `--color-accent`, which would
                make the state follow the user's highlight color and blend
                into buttons/switches. */}
            <span
              className={cn(
                "h-2 w-2 rounded-full",
                localIsRunning ? "bg-emerald-500" : localIsStarting ? "bg-amber-500" : "bg-zinc-400",
              )}
            />
            <span className={cn(
              localIsRunning ? "text-emerald-600 dark:text-emerald-400" :
                localIsStarting ? "text-amber-600 dark:text-amber-400" :
                  "text-text-tertiary"
            )}>
              {localIsRunning ? t("settings.running") : localIsStarting ? t("settings.starting") : t("settings.stopped")}
            </span>
          </div>

          {health && localIsRunning && (
            <div className="mt-2 flex items-center gap-2 text-xs">
              <span className="text-text-tertiary">{t("settings.version")}</span>
              <span>{health.version}</span>
            </div>
          )}

          <div className="mt-3 flex gap-2">
            {!localIsTauriManaged && !localIsForeign && !localIsStarting && (
              <button
                onClick={handleStartLocal}
                disabled={starting}
                className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
              >
                {starting ? t("settings.starting") : t("settings.startGateway")}
              </button>
            )}
            {localIsTauriManaged && (
              <>
                <button
                  onClick={handleRestartLocal}
                  disabled={starting}
                  className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
                >
                  {starting ? t("settings.restarting") : t("settings.restart")}
                </button>
                <button
                  onClick={handleStopLocal}
                  disabled={stopping}
                  className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
                >
                  {stopping ? t("settings.stopping") : t("settings.stop")}
                </button>
              </>
            )}
          </div>
          {localIsForeign && (
            <p className="mt-2 text-10 text-text-tertiary ">
              {t("settings.gatewayRunningExternal")}
            </p>
          )}
          </ExpandableRow>
        </ListBox>
      )}

      {/* Connection diagnostics — always available, all topologies. */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={gatewayConnOpen}
          onToggle={() => setGatewayConnOpen((v) => !v)}
          title={t("settings.connectionDiagnostics")}
          ariaLabel={t("settings.connectionDiagnostics")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <div className="space-y-3">
            <div className="flex items-center gap-2 text-xs">
              <span className="text-text-tertiary">{t("settings.gatewayUrl")}</span>
              <span className="truncate font-mono">{gatewayUrl}</span>
            </div>
            <div className="flex items-center gap-2 text-xs">
              <span className="text-text-tertiary">{t("settings.status")}</span>
              <span
                className={cn(
                  "h-2 w-2 rounded-full",
                  status === "connected" ? "bg-emerald-500" : status === "error" ? "bg-red-500" : "bg-zinc-400",
                )}
              />
              <span className={cn(
                status === "connected" ? "text-emerald-600 dark:text-emerald-400" :
                  status === "error" ? "text-red-600 dark:text-red-400" :
                    "text-text-tertiary"
              )}>
                {status === "connected" ? t("settings.connected") : status === "error" ? t("settings.error") : t("settings.disconnected")}
              </span>
            </div>
            {health && (
              <div className="flex items-center gap-2 text-xs">
                <span className="text-text-tertiary">{t("settings.version")}</span>
                <span>{health.version}</span>
              </div>
            )}
            <button
              onClick={handleTest}
              disabled={testing}
              className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
            >
              {testing ? t("settings.testing") : t("settings.testConnection")}
            </button>
          </div>
        </ExpandableRow>
      </ListBox>

      {/* Relay mode: tunnel status panel (polls GET /api/relay/status —
          the Gateway's relay client snapshot, forwarded through the
          tunnel itself). Read-only: enable/disable is Gateway-side
          admin (the remote ACL blocks the mutating endpoints, §7.2). */}
      {gatewayMode === "relay" && <RelayTunnelPanel />}

      {/* Nodes + their agents (every address — no topology gate) */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={nodesOpen}
          onToggle={() => setNodesOpen((v) => !v)}
          title={t("settings.nodesTitle", { count: nodes.length })}
          ariaLabel={t("settings.nodesTitle", { count: nodes.length })}
          trailing={
            <span onClick={(e) => e.stopPropagation()}>
              <button
                onClick={() => void fetchAll()}
                disabled={nodesLoading || agentsLoading}
                className="rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
              >
                {t("settings.nodesRefresh")}
              </button>
            </span>
          }
          bodyClassName="overflow-hidden rounded-b-md border-t border-border-divider bg-panel-inset"
        >
          {status !== "connected" ? (
            <div className="px-3 py-3 text-xs text-text-tertiary">{t("settings.connectToSeeAgents")}</div>
          ) : nodesLoading && nodes.length === 0 ? (
            <div className="px-3 py-3 text-xs text-text-tertiary">{t("settings.loading")}</div>
          ) : nodes.length === 0 ? (
            <div className="px-3 py-3 text-xs text-text-tertiary">{t("settings.nodesEmpty")}</div>
          ) : (
            <NodesTree
              nodes={nodes}
              agents={agents}
              openNodeIds={openNodeIds}
              onToggleNode={toggleNode}
            />
          )}
        </ExpandableRow>
      </ListBox>

      {/* P1-5 (b): Full-stack service diagnostics. Self-contained
          card; `ServicesPanel` subscribes to servicesStore directly so
          this parent stays free of probe state. */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={servicesOpen}
          onToggle={() => setServicesOpen((v) => !v)}
          title={t("settings.servicesTitle")}
          ariaLabel={t("settings.servicesTitle")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <ServicesPanel />
        </ExpandableRow>
      </ListBox>

      {/* P1-5 (d): Recent MQTT transition history (consumes
          `chatStore.transitionLog` from P0-4). Helps the user answer
          "why did the chat just drop?" without opening DevTools. */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={eventsOpen}
          onToggle={() => setEventsOpen((v) => !v)}
          title={t("settings.services.eventsTitle")}
          ariaLabel={t("settings.services.eventsTitle")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
        >
          <RecentEventsLog />
        </ExpandableRow>
      </ListBox>
    </div>
  );
}

/** A "?" affordance that explains the row it sits next to, on hover.
 *
 *  Stock [`Tooltip`] on a focusable trigger: hover (or keyboard focus) opens
 *  it, positioned and styled by the component every other hint in the app
 *  already uses. Lives here rather than in `common/` because the only two
 *  callers are the two label/value pairs in the mode card.
 *
 *  `label` is the accessible name and defaults to the hint text, which reads
 *  fine to a screen reader ("? In Local mode, Desktop probes this address
 *  first...") and saves a second i18n key per call site. Pass a short `label`
 *  when the content is long or already says something else. */
function HelpHint({ content, label }: { content: string; label?: string }) {
  return (
    <Tooltip content={content} maxWidth="320px">
      <button
        type="button"
        aria-label={label ?? content}
        className="shrink-0 rounded p-0.5 text-text-tertiary hover:text-text-secondary"
      >
        <HelpCircle size={12} />
      </button>
    </Tooltip>
  );
}

/** "What is this, and how do I get one?" help for the gw-id row.
 *
 *  Three hand-rolled predecessors are documented here so nobody rebuilds one:
 *    - inline expander: reflowed the two label/value lines into a ragged wrap
 *      and grew the card on every click,
 *    - bottom-right toast: right content, wrong semantics AND wrong place —
 *      a toast is a global notification, this is a hint about the row you are
 *      pointing at,
 *    - hand-positioned `createPortal` popup: opened at {top:0,left:0} on the
 *      first frame because the coordinates were only measured in an effect, so
 *      it flashed in the top-left corner. The stock tooltip measures
 *      synchronously in the same tick it is asked to show.
 *
 *  The `<relay-domain>` placeholder is deliberate: the suffix is a
 *  relay-SERVER deployment setting (`device_domain_suffix` in
 *  acowork-relay's config) that the Gateway's status snapshot does not carry,
 *  so the Desktop has nothing authoritative to substitute. Same placeholder
 *  the address field shows.
 *
 *  `gatewayUrl` is passed rather than re-read from the store so the command
 *  points at the Gateway this row was fetched from.
 *
 *  320px, not the 200px default: the pairing address and the config snippet
 *  are both long, and a 200px box would shred them into an unreadable
 *  column. */
function GatewayGwIdHelp({ gwId }: { gwId: string | null }) {
  const { t } = useTranslation();
  const content = gwId
    ? t("settings.relayGwIdHelp", { gwId })
    : t("settings.relayGwIdHowTo");
  return <HelpHint content={content} label={t("settings.relayGwIdHelpLabel")} />;
}

/** Mirror of the Gateway's `RelayClientStatus`
 *  (`GET /api/relay/status`, core `relay/client.rs`). Only the fields
 *  the panel renders — unknown fields are ignored by design so a
 *  Gateway-side addition doesn't break older Desktops. */
interface RelayStatus {
  enabled: boolean;
  relay_url: string | null;
  gw_id: string | null;
  connected: boolean;
  session_id: string | null;
  last_error: string | null;
  connected_at: string | null;
}

/** Relay-mode tunnel status panel (design doc 24 §8.1 远程访问面板).
 *
 *  Self-contained card: polls the Gateway's relay tunnel snapshot every
 *  5 s while mounted (the request itself travels through the tunnel,
 *  so a successful poll with `connected: false` is meaningful — the
 *  Gateway is reachable but its outbound tunnel to the relay is down).
 *  Read-only by design: `POST /api/relay/enable|disable` are blocked
 *  for relay-originated requests by the remote ACL (§7.2), so the
 *  panel never offers them. */
function RelayTunnelPanel() {
  const { t } = useTranslation();
  const [relayStatus, setRelayStatus] = useState<RelayStatus | null>(null);
  const [relayError, setRelayError] = useState(false);
  const [relayOpen, setRelayOpen] = useState(true);

  const refreshRelay = useCallback(async () => {
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/relay/status`);
      if (resp.ok) {
        setRelayStatus((await resp.json()) as RelayStatus);
        setRelayError(false);
      } else {
        setRelayError(true);
      }
    } catch {
      // Gateway unreachable through the relay — keep the last snapshot
      // and flag the fetch failure.
      setRelayError(true);
    }
  }, []);

  useEffect(() => {
    void refreshRelay();
    const id = setInterval(() => void refreshRelay(), 5000);
    return () => clearInterval(id);
  }, [refreshRelay]);

  const connected = relayStatus?.connected ?? false;
  const gwId = relayStatus?.gw_id ?? "—";
  const relayUrl = relayStatus?.relay_url ?? "—";
  const session = relayStatus?.session_id ?? "—";
  const connectedAt = relayStatus?.connected_at
    ? new Date(relayStatus.connected_at).toLocaleString()
    : "—";

  return (
    <ListBox dividers={false}>
      <ExpandableRow
        open={relayOpen}
        onToggle={() => setRelayOpen((v) => !v)}
        title={t("settings.relayTunnel")}
        ariaLabel={t("settings.relayTunnel")}
        trailing={
          <span onClick={(e) => e.stopPropagation()}>
            <button
              onClick={() => void refreshRelay()}
              className="rounded btn-solid px-2 py-1 text-11 font-medium"
            >
              {t("settings.nodesRefresh")}
            </button>
          </span>
        }
        bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
      >
        <div className="space-y-2 text-xs">
          <div className="flex items-center gap-2">
            <span className="text-text-tertiary">{t("settings.status")}</span>
            <span
              className={cn(
                "h-2 w-2 rounded-full",
                connected ? "bg-emerald-500" : relayError ? "bg-red-500" : "bg-zinc-400",
              )}
            />
            <span
              className={cn(
                connected
                  ? "text-emerald-600 dark:text-emerald-400"
                  : relayError
                    ? "text-red-600 dark:text-red-400"
                    : "text-text-tertiary",
              )}
            >
              {connected
                ? t("settings.relayTunnelOnline")
                : relayError
                  ? t("settings.error")
                  : t("settings.relayTunnelOffline")}
            </span>
          </div>

          <div className="flex items-center gap-2">
            <span className="text-text-tertiary">{t("settings.relayGwId")}</span>
            <span className="font-mono">{gwId}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-text-tertiary">{t("settings.relayUrl")}</span>
            <span className="font-mono">{relayUrl}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-text-tertiary">{t("settings.relaySession")}</span>
            <span className="font-mono">{session}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-text-tertiary">{t("settings.relayConnectedAt")}</span>
            <span className="font-mono">{connectedAt}</span>
          </div>
          {/* Which address this snapshot came from. The panel polls the
              *configured* Gateway URL, so "在线" only proves THAT address
              reached the Gateway — with a LAN address configured it reports
              the local Gateway's own tunnel and says nothing about whether
              the relay path works. Naming the source stops that misread. */}
          <div className="flex items-start gap-2">
            <span className="shrink-0 text-text-tertiary">{t("settings.relayReadFrom")}</span>
            <span className="break-all font-mono">{getGatewayUrl()}</span>
          </div>

          {relayStatus?.last_error && (
            <div className="flex items-center gap-2">
              <span className="text-text-tertiary">{t("settings.relayLastError")}</span>
              <span className="text-red-600 dark:text-red-400">{relayStatus.last_error}</span>
            </div>
          )}
        </div>
      </ExpandableRow>
    </ListBox>
  );
}

/** Recent transition log (P1-5 d). Reads `chatStore.transitionLog`
 *  (max 20 entries, ring-buffered in P0-4) and renders them newest
 *  first. */
function RecentEventsLog() {
  const { t } = useTranslation();
  // I-1: named `entries` — a local `log` shadowed the module logger.
  const entries = useChatStore((s) => s.transitionLog);
  if (entries.length === 0) {
    return (
      <div className="px-3 py-3 text-xs text-text-tertiary ">
        {t("settings.services.eventsEmpty")}
      </div>
    );
  }
  return (
    <OverlayScrollbar maxHeight={192}>
      <ul className="divide-y divide-border-divider">
      {entries
        .slice()
        .reverse()
        .map((entry, idx) => {
          const ts = new Date(entry.timestamp).toLocaleTimeString();
          const isErr = entry.to !== "connected";
          return (
            <li
              key={`${entry.timestamp}-${idx}`}
              className="flex items-center gap-2 px-3 py-1.5 text-11 font-mono"
            >
              <span className="text-text-tertiary ">{ts}</span>
              <span className="text-text-secondary ">
                {entry.from} → {entry.to}
              </span>
              {entry.reason && (
                <span
                  className={cn(
                    "flex-1 truncate",
                    isErr
                      ? "text-red-600 dark:text-red-300"
                      : "text-text-tertiary ",
                  )}
                  title={entry.reason}
                >
                  {entry.reason}
                </span>
              )}
              {!entry.reason && <span className="flex-1" />}
            </li>
          );
        })}
      </ul>
    </OverlayScrollbar>
  );
}

/** Synthetic node_id bucket for agents whose node doesn't appear in
 *  `/api/nodes` (gateway-local agents before the node registry sees the
 *  local node, or stale rows after a node leaves). Keeps them visible
 *  instead of dropping them silently. */
const UNASSIGNED_NODE_ID = "__unassigned__";

/** Two-level collapsible list: each node row expands to reveal its agents. */
function NodesTree({
  nodes,
  agents,
  openNodeIds,
  onToggleNode,
}: {
  nodes: NodeInfo[];
  agents: AgentListResponse[];
  openNodeIds: Record<string, boolean>;
  onToggleNode: (nodeId: string) => void;
}) {
  const { t } = useTranslation();
  // Bucket agents by node_id; agents without a matching node fall into the
  // UNASSIGNED bucket so they never disappear from the view.
  const buckets = new Map<string, AgentListResponse[]>();
  for (const node of nodes) buckets.set(node.node_id, []);
  const orphan: AgentListResponse[] = [];
  for (const agent of agents) {
    const bucket = buckets.get(agent.node_id);
    if (bucket) bucket.push(agent);
    else orphan.push(agent);
  }
  const showUnassigned = orphan.length > 0;
  const renderedNodes: Array<{ id: string; node?: NodeInfo }> = [
    ...nodes.map((n) => ({ id: n.node_id, node: n })),
    ...(showUnassigned ? [{ id: UNASSIGNED_NODE_ID }] : []),
  ];

  return (
    <ListBox variant="plain">
      {renderedNodes.map(({ id, node }) => {
        const nodeAgents = id === UNASSIGNED_NODE_ID ? orphan : (buckets.get(id) ?? []);
        return (
          <ExpandableRow
            key={id}
            open={!!openNodeIds[id]}
            onToggle={() => onToggleNode(id)}
            surface="inset"
            title={node?.node_name ?? node?.hostname ?? node?.node_id ?? t("settings.nodesUnassigned")}
            meta={
              <span className="inline-flex items-center gap-1 text-10">
                <span
                  className={cn(
                    "h-1.5 w-1.5 rounded-full",
                    node ? (node.online ? "bg-emerald-500" : "bg-zinc-400 dark:bg-zinc-500") : "bg-zinc-400 dark:bg-zinc-500",
                  )}
                />
                <span className={node?.online ? "text-emerald-600 dark:text-emerald-400" : "text-text-tertiary"}>
                  {node ? (node.online ? t("settings.nodesOnline") : t("settings.nodesOffline")) : t("settings.nodesOffline")}
                </span>
              </span>
            }
            description={buildNodeDescription(node, t)}
            trailing={
              <span className="text-xs text-text-tertiary">
                {nodeAgents.length}
                {node?.max_agents !== undefined && <span className="text-text-tertiary">/{node.max_agents}</span>}
              </span>
            }
            bodyClassName="bg-zinc-50 dark:bg-zinc-900/60"
          >
            {nodeAgents.length === 0 ? (
              <div className="px-3 py-3 text-xs text-text-tertiary">{t("settings.noAgentsRunning")}</div>
            ) : (
              <ListBox variant="plain">
                {nodeAgents.map((agent) => (
                  <RuntimeRow key={agent.instance_id} agent={agent} padding="nested" />
                ))}
              </ListBox>
            )}
          </ExpandableRow>
        );
      })}
    </ListBox>
  );
}

/** Build the small description line for a node row: hostname · os/arch. */
function buildNodeDescription(node: NodeInfo | undefined, t: (key: string) => string) {
  if (!node) return t("settings.nodesUnassignedDesc");
  const parts: string[] = [];
  if (node.hostname) parts.push(node.hostname);
  if (node.os && node.arch) parts.push(`${node.os}/${node.arch}`);
  else if (node.os) parts.push(node.os);
  else if (node.arch) parts.push(node.arch);
  if (node.node_version) parts.push(`v${node.node_version}`);
  return parts.join(" · ") || "—";
}

/** Single runtime row component — fetches model info independently */
function RuntimeRow({ agent, padding }: { agent: AgentListResponse; padding?: "default" | "nested" }) {
  const { t } = useTranslation();
  const [modelInfo, setModelInfo] = useState<{ provider: string; model: string } | null>(null);

  useEffect(() => {
    let cancelled = false;
    // ADR-073: `/api/agents/{id}` addresses the INSTANCE — using the
    // package `agent_id` would misroute in multi-instance deployments.
    fetch(`${getGatewayUrl()}/api/agents/${agent.instance_id}/model`)
      .then(r => r.ok ? r.json() : null)
      .then(data => {
        if (!cancelled && data) {
          setModelInfo({ provider: data.provider, model: data.model });
        }
      })
      .catch(() => { });
    return () => { cancelled = true; };
  }, [agent.instance_id]);

  return (
    <ListRow
      surface="inset"
      padding={padding ?? "default"}
      leading={<Monitor className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />}
      trailing={
        <div className="flex items-center gap-2 shrink-0">
          {modelInfo ? (
            <span className="text-xs text-text-tertiary">{modelInfo.provider}/{modelInfo.model}</span>
          ) : (
            <span className="text-xs text-text-tertiary">—</span>
          )}
        </div>
      }
    >
      <div className="flex items-center gap-2 min-w-0">
        <span className="truncate text-xs font-medium">{agent.name}</span>
        {/* ADR-048 follow-up: badge reflects current DevMode capability
            (debug_state), not startup intent (dev_mode) — an agent can be
            flipped into DevMode at runtime without restart. */}
        {agent.debug_state === "enabled" && (
          <span className="inline-flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-10 text-amber-700 dark:bg-amber-900/30 dark:text-amber-400">
            <Bug className="h-3 w-3" />
            {t("settings.debug")}
          </span>
        )}
      </div>
    </ListRow>
  );
}

/** Appearance settings */
function AppearanceTab() {
  const { t } = useTranslation();
  const { theme, setTheme, fontSize, setFontSize, contentWidth, setContentWidth, opacity, setOpacity, accentColor, customAccent, setAccentColor } = useSettingsStore();
  const [showResetConfirm, setShowResetConfirm] = useState(false);
  // Level-1 collapsible cards (same grammar as the Setup panel), default open
  const [themeOpen, setThemeOpen] = useState(true);
  const [accentOpen, setAccentOpen] = useState(true);
  const [contentWidthOpen, setContentWidthOpen] = useState(true);
  const [fontSizeOpen, setFontSizeOpen] = useState(true);
  const [opacityOpen, setOpacityOpen] = useState(true);
  const [resetOpen, setResetOpen] = useState(true);

  // Content width options: 40-100%, step 10
  const contentWidths = [
    { label: "40%", value: 40 },
    { label: "50%", value: 50 },
    { label: "60%", value: 60 },
    { label: "70%", value: 70 },
    { label: "80%", value: 80 },
    { label: "90%", value: 90 },
    { label: "100%", value: 100 },
  ];

  // Font size options: M = previous default (text-sm = 0.875rem)
  const fontSizes = [
    { label: "S", value: 0.75 },
    { label: "M", value: 0.875 },
    { label: "L", value: 1.0 },
    { label: "XL", value: 1.125 },
    { label: "XXL", value: 1.25 },
  ];

  return (
    <div className="w-fit space-y-4">
      <ListBox dividers={false}>
        <ExpandableRow
          open={themeOpen}
          onToggle={() => setThemeOpen((v) => !v)}
          title={t("settings.theme")}
          ariaLabel={t("settings.theme")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <RadioGroup
            name="theme"
            value={theme}
            options={[
              { label: t("settings.light"), value: "light" as const },
              { label: t("settings.dark"), value: "dark" as const },
              { label: t("settings.system"), value: "system" as const },
            ]}
            onChange={setTheme}
          />
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={accentOpen}
          onToggle={() => setAccentOpen((v) => !v)}
          title={t("settings.accentColor")}
          ariaLabel={t("settings.accentColor")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <p className="mb-3 text-xs text-text-tertiary">{t("settings.accentColor")}</p>
          <AccentSwatches value={accentColor} customColor={customAccent} onChange={setAccentColor} />
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={contentWidthOpen}
          onToggle={() => setContentWidthOpen((v) => !v)}
          title={t("settings.contentWidth")}
          ariaLabel={t("settings.contentWidth")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <p className="mb-2 text-xs text-text-tertiary">{t("settings.contentWidthHint")}</p>
          <RadioGroup
            name="contentWidth"
            value={contentWidth}
            options={contentWidths}
            onChange={setContentWidth}
            noWrap
          />
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={fontSizeOpen}
          onToggle={() => setFontSizeOpen((v) => !v)}
          title={t("settings.fontSize")}
          ariaLabel={t("settings.fontSize")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <RadioGroup
            name="fontSize"
            value={fontSize}
            options={fontSizes}
            onChange={setFontSize}
          />
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={opacityOpen}
          onToggle={() => setOpacityOpen((v) => !v)}
          title={t("settings.opacity")}
          ariaLabel={t("settings.opacity")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <p className="mb-2 text-xs text-text-tertiary">{t("settings.opacityHint")}</p>
          <div className="flex items-center gap-3">
            <input
              type="range"
              min="0"
              max="1.0"
              step="0.01"
              value={opacity}
              onChange={(e) => setOpacity(parseFloat(e.target.value))}
              className="flex-1"
              style={{ "--progress": `${opacity * 100}%` } as React.CSSProperties}
            />
            <span className="w-10 text-right text-xs text-text-secondary ">
              {Math.round(opacity * 100)}%
            </span>
          </div>
        </ExpandableRow>
      </ListBox>

      {/* Reset appearance to defaults */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={resetOpen}
          onToggle={() => setResetOpen((v) => !v)}
          title={t("settings.resetAppearance")}
          ariaLabel={t("settings.resetAppearance")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <button
            onClick={() => setShowResetConfirm(true)}
            className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs"
          >
            {t("settings.resetToDefaults")}
          </button>

          <ConfirmDialog
            open={showResetConfirm}
            title={t("settings.resetAppearance")}
            message={t("settings.resetAppearanceConfirm")}
            confirmLabel={t("settings.reset")}
            destructive
            onConfirm={() => {
              setTheme(DEFAULT_THEME); setFontSize(DEFAULT_FONT_SIZE); setContentWidth(DEFAULT_CONTENT_WIDTH); setOpacity(DEFAULT_OPACITY); setAccentColor(DEFAULT_ACCENT_COLOR);
              setShowResetConfirm(false);
            }}
            onCancel={() => setShowResetConfirm(false)}
          />
        </ExpandableRow>
      </ListBox>
    </div>
  );
}

/** General settings */
function GeneralTab() {
  const { t } = useTranslation();
  const [config, setConfig] = useState<GatewayConfig | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [showDeleteConfirm, setShowDeleteConfirm] = useState(false);
  const [showResetOnboardingConfirm, setShowResetOnboardingConfirm] = useState(false);
  // Level-1 collapsible cards (same grammar as the Setup panel), default open
  const [logSetupOpen, setLogSetupOpen] = useState(true);
  const [dataDirectoryOpen, setDataDirectoryOpen] = useState(true);
  const [aboutOpen, setAboutOpen] = useState(true);
  const [resetOnboardingOpen, setResetOnboardingOpen] = useState(true);
  const { logLevel, setLogLevel, logFileSizeMb, setLogFileSizeMb, logFileCount, setLogFileCount, frontendLogLevel, setFrontendLogLevel } = useSettingsStore();

  useEffect(() => {
    fetch(`${getGatewayUrl()}/api/config`)
      .then((r) => { if (!r.ok) throw new Error(`HTTP ${r.status}`); return r.json(); })
      .then((cfg: GatewayConfig) => {
        setConfig(cfg);
        // Gateway value takes precedence over localStorage
        setLogLevel(cfg.log_level);
        if (cfg.log_file_size_mb !== undefined) {
          setLogFileSizeMb(cfg.log_file_size_mb);
        }
        if (cfg.log_file_count !== undefined) {
          setLogFileCount(cfg.log_file_count);
        }
      })
      .catch(() => { });
  }, [setLogLevel, setLogFileSizeMb, setLogFileCount]);

  const currentLogLevel = config?.log_level || logLevel || "info";
  const currentLogFileSize = config?.log_file_size_mb ?? logFileSizeMb;
  const currentLogFileCount = config?.log_file_count ?? logFileCount;

  const handleDeleteLogs = async () => {
    setShowDeleteConfirm(false);
    setDeleting(true);
    try {
      await fetch(`${getGatewayUrl()}/api/logs`, { method: "DELETE" });
    } catch { /* ignore */ }
    finally { setDeleting(false); }
  };

  return (
    <div className="max-w-lg space-y-4">
      <ListBox dividers={false}>
        <ExpandableRow
          open={logSetupOpen}
          onToggle={() => setLogSetupOpen((v) => !v)}
          title={t("settings.logSetup")}
          ariaLabel={t("settings.logSetup")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >

        {/* Log level */}
        <div className="mb-3">
          <label className="block mb-1.5 text-xs text-text-tertiary ">
            {t("settings.logLevel")}
          </label>
          <div>
            <Dropdown
              className="w-[5.5rem]"
              value={currentLogLevel}
              onChange={async (val) => {
                try {
                  await fetch(`${getGatewayUrl()}/api/config`, {
                    method: "PUT",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({ log_level: val }),
                  });
                  setConfig((prev) => (prev ? { ...prev, log_level: val } : prev));
                  setLogLevel(val);
                } catch { /* ignore */ }
              }}
              options={[
                { value: "trace", label: "trace" },
                { value: "debug", label: "debug" },
                { value: "info", label: "info" },
                { value: "warn", label: "warn" },
                { value: "error", label: "error" },
              ]}
            />
          </div>
        </div>

        {/* Frontend log level (DevTools console) */}
        <div className="mb-3">
          <label className="block mb-1.5 text-xs text-text-tertiary ">
            {t("settings.frontendLogLevel")}
          </label>
          <div>
            <Dropdown
              className="w-[5.5rem]"
              value={frontendLogLevel}
              onChange={(v) => setFrontendLogLevel(v as "trace" | "debug" | "info" | "warn" | "error" | "off")}
              options={[
                { value: "trace", label: "trace" },
                { value: "debug", label: "debug" },
                { value: "info", label: "info" },
                { value: "warn", label: "warn" },
                { value: "error", label: "error" },
                { value: "off", label: "off" },
              ]}
            />
          </div>
          <p className="mt-1 text-10 text-text-tertiary">
            {t("settings.frontendLogLevelHint")}
          </p>
        </div>

        {/* Log file size */}
        <div className="mb-3">
          <label className="block mb-1.5 text-xs text-text-tertiary ">
            {t("settings.logFileSize")}
          </label>
          <div className="flex items-center gap-2">
            <StyledInput
              type="number"
              min={0}
              max={1024}
              value={currentLogFileSize}
              onChange={async (e) => {
                const val = Math.max(0, parseInt(e.target.value, 10) || 0);
                setLogFileSizeMb(val);
                try {
                  await fetch(`${getGatewayUrl()}/api/config`, {
                    method: "PUT",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({ log_file_size_mb: val }),
                  });
                  setConfig((prev) => (prev ? { ...prev, log_file_size_mb: val } : prev));
                } catch { /* ignore */ }
              }}
              className="w-16"
            />
            <span className="text-xs text-text-tertiary">
              {currentLogFileSize === 0 ? t("settings.noSplit") : t("settings.autoSplit", { size: currentLogFileSize })}
            </span>
          </div>
          <p className="mt-1 text-10 text-text-tertiary">
            {t("settings.logFileSizeHint")}
          </p>
        </div>

        {/* Max log file count */}
        <div className="mb-3">
          <label className="block mb-1.5 text-xs text-text-tertiary ">
            {t("settings.maxLogFiles")}
          </label>
          <div className="flex items-center gap-2">
            <StyledInput
              type="number"
              min={0}
              max={999}
              value={currentLogFileCount}
              onChange={async (e) => {
                const val = Math.max(0, parseInt(e.target.value, 10) || 0);
                setLogFileCount(val);
                try {
                  await fetch(`${getGatewayUrl()}/api/config`, {
                    method: "PUT",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({ log_file_count: val }),
                  });
                  setConfig((prev) => (prev ? { ...prev, log_file_count: val } : prev));
                } catch { /* ignore */ }
              }}
              className="w-16"
            />
            <span className="text-xs text-text-tertiary">
              {currentLogFileCount === 0 ? t("settings.unlimited") : t("settings.keepFiles", { count: currentLogFileCount })}
            </span>
          </div>
          <p className="mt-1 text-10 text-text-tertiary">
            {t("settings.maxLogFilesHint")}
          </p>
        </div>

        {/* Delete all logs */}
        <button
          onClick={() => setShowDeleteConfirm(true)}
          disabled={deleting}
          className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
        >
          {deleting ? t("settings.deleting") : t("settings.deleteAllLogs")}
        </button>

        {/* Delete confirmation dialog */}
        {showDeleteConfirm && (
          <div className="fixed inset-0 z-50 flex items-cell justify-center bg-modal-overlay">
            <div className="w-[380px] rounded-md bg-modal-surface p-6 shadow-xl">
              <h3 className="mb-2 text-sm font-semibold">{t("settings.deleteLogsConfirmTitle")}</h3>
              <p className="mb-4 text-xs text-text-tertiary ">
                {t("settings.deleteLogsConfirmMsg")}
              </p>
              <div className="flex justify-end gap-2">
                <button
                  onClick={() => setShowDeleteConfirm(false)}
                  className="btn-solid rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium"
                >
                  {t("common.cancel")}
                </button>
                <button
                  onClick={handleDeleteLogs}
                  className="btn-accent rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium"
                >
                  {t("settings.confirmDelete")}
                </button>
              </div>
            </div>
          </div>
        )}
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={dataDirectoryOpen}
          onToggle={() => setDataDirectoryOpen((v) => !v)}
          title={t("settings.dataDirectory")}
          ariaLabel={t("settings.dataDirectory")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <input
            type="text"
            value={config?.data_dir ?? "\u2014"}
            readOnly
            className={`w-full ${inputReadonly}`}
          />
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={aboutOpen}
          onToggle={() => setAboutOpen((v) => !v)}
          title={t("settings.about")}
          ariaLabel={t("settings.about")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <div className="text-xs text-text-tertiary ">
            <p>ACowork Desktop v0.1.0</p>
            <p className="mt-1">Built with Tauri v2 + React 19</p>
          </div>
        </ExpandableRow>
      </ListBox>

      {/* Reset Onboarding */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={resetOnboardingOpen}
          onToggle={() => setResetOnboardingOpen((v) => !v)}
          title={t("settings.resetOnboarding")}
          ariaLabel={t("settings.resetOnboarding")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >
          <p className="text-xs text-text-tertiary ">
            {t("settings.resetOnboardingDesc")}
          </p>
          <button
            onClick={() => setShowResetOnboardingConfirm(true)}
            className="mt-3 rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium"
          >
            {t("settings.resetOnboardingBtn")}
          </button>

          <ConfirmDialog
            open={showResetOnboardingConfirm}
            title={t("settings.resetOnboarding")}
            message={t("settings.resetOnboardingConfirm")}
            confirmLabel={t("settings.reset")}
            destructive
            onConfirm={async () => {
              setShowResetOnboardingConfirm(false);
              try {
                const { resetOnboarding } = await import("../../lib/gateway-api");
                await resetOnboarding();
              } catch (e) {
                log.error("Failed to reset onboarding:", e);
              }
              window.location.reload();
            }}
            onCancel={() => setShowResetOnboardingConfirm(false)}
          />
        </ExpandableRow>
      </ListBox>
    </div>
  );
}
