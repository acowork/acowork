import { useState, useEffect, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useAgentStore } from "../../stores/agentStore";
import { useAuthStore } from "../../stores/authStore";
import { cn } from "../../lib/utils";
import { fetchProviders } from "../../lib/gateway-api";
import { saveOnboardedIdentity } from "../../lib/selfProfile";
import { getGatewayUrl } from "../../lib/config";
import type { ProviderListEntry, VaultKeyEntry, BootstrapStateView, OperationAck } from "../../lib/types";
import { StyledInput } from "../common/StyledInput";
import { GatewayAddressPicker } from "../connection/GatewayAddressPicker";
import { ErrorBox } from "../common/ErrorBox";
import { NoNodeBanner } from "../nodes/NoNodeBanner";
import { useNodeTopologyRefresh } from "../nodes/useNodeTopologyRefresh";
import { usableNodesOf, noNodeDetailOf, noNodeDetailKey } from "../nodes/nodeAvailability";
import { useToast } from "../common/ToastProvider";
import { Dropdown } from "../common/Dropdown";
import { ProviderLogo } from "../common/ProviderLogo";
import { useTranslation } from "../../i18n/useTranslation";
import { ProviderPicker } from "../harness/ProviderPicker";
import { AddProviderFlow } from "../harness/AddProviderFlow";
import brandMark from "../../../../../assets/brand-mark.svg";
import { log } from "../../lib/logger";

const TOTAL_STEPS = 5;

const RECOMMENDED_AGENTS = [
  {
    resourceName: "software-architect-agent",
    name: "Architect",
    role: "Software Architect",
    description: "System design, architecture review, technical planning, and risk assessment",
  },
  {
    resourceName: "senior-engineer-agent",
    name: "SSE",
    role: "Senior Software Engineer",
    description: "Code review, architecture design, debugging, refactoring, testing, and documentation",
  },
  {
    resourceName: "quality-assurance-agent",
    name: "QA",
    role: "Quality Assurance Manager",
    description: "Quality strategy, test planning, defect management, and release acceptance",
  },
  {
    resourceName: "project-manager-agent",
    name: "PM",
    role: "Project Manager",
    description: "Requirements analysis, task decomposition, progress tracking, and risk management",
  },
  {
    resourceName: "product-manager-agent",
    name: "Product",
    role: "Product Manager",
    description: "Product strategy, user research, PRD writing, roadmap, and launch planning",
  },
  {
    resourceName: "document-manager-agent",
    name: "Docs",
    role: "Document Manager",
    description: "Document collection, organization, writing, conversion, and knowledge base maintenance",
  },
];

interface OnboardingState {
  completed: boolean;
  currentStep: number;
  // Step 4: identity
  name: string;
  language: string;
  timezone: string;
  city: string;
  occupation: string;
}

export function OnboardingFlow({ onComplete }: { onComplete?: () => void }) {
  const { t } = useTranslation();
  const [state, setState] = useState<OnboardingState>({
    completed: false,
    currentStep: 1,
    name: "",
    language: "zh-CN",
    timezone: "Asia/Shanghai",
    city: "",
    occupation: "",
  });

  // Check if onboarding was already completed
  useEffect(() => {
    const saved = localStorage.getItem("acowork_onboarding");
    if (saved === "completed") {
      setState((prev) => ({ ...prev, completed: true }));
    }
  }, []);

  // Prefill the identity step from the signed-in account.
  //
  // The wizard *patches* the logged-in account
  // (`saveOnboardedIdentity`), so starting from its current values keeps a
  // re-run of the wizard (Settings → reset onboarding) from overwriting the
  // stored identity with blanks. In `local` mode there is no account yet and
  // this is a no-op.
  useEffect(() => {
    const account = useAuthStore.getState().account;
    if (!account) return;
    setState((prev) => ({
      ...prev,
      name: prev.name || account.display_name || "",
      language: account.language || prev.language,
      timezone: account.timezone || prev.timezone,
      city: account.city ?? prev.city,
      occupation: account.occupation ?? prev.occupation,
    }));
  }, []);

  const completeOnboarding = useCallback(() => {
    // Persist the identity collected in Step 4.
    //
    // Fire-and-forget — don't block onboarding completion on the API result.
    //
    // (`lib/selfProfile.ts`): the user is
    // already signed in (login-then-onboarding boot order), so this PUTs the
    // logged-in account; the legacy `POST /api/users` created a *second*
    // account nobody was signed in as, which is why the identity entered here
    // never showed up in the profile tab afterwards. `local` keeps the
    // create path.
    //
    // ADR-059 §7.3: `createUser` answers an `OperationAck` (operation_id /
    // state / resource_version / terminal_error) instead of the legacy
    // `{ user, version }` envelope. We intentionally ignore the ack —
    // ProfileTab re-fetches the profile when it mounts, which picks up the
    // just-written identity.
    if (state.name.trim()) {
      saveOnboardedIdentity({
        display_name: state.name.trim(),
        language: state.language,
        timezone: state.timezone,
        city: state.city.trim() || undefined,
        occupation: state.occupation.trim() || undefined,
      }).catch((err) => {
        log.warn("Failed to persist the user profile during onboarding:", err);
      });
      // Sync the display name to the local profile store so the avatar
      // picker in ProfileTab immediately shows the name just entered.
      useUserProfileStore.getState().setProfile({ displayName: state.name.trim() });
    }
    // Assign a random builtin avatar if the user doesn't have one yet,
    // so the freshly-onboarded user shows a real icon (not the legacy
    // letter/gradient fallback) from the very first session.
    useUserProfileStore.getState().assignRandomAvatarIfMissing();
    localStorage.setItem("acowork_onboarding", "completed");
    setState((prev) => ({ ...prev, completed: true }));
    onComplete?.();
  }, [onComplete, state.name, state.language, state.timezone, state.city, state.occupation]);

  const nextStep = () => setState((prev) => ({ ...prev, currentStep: Math.min(prev.currentStep + 1, TOTAL_STEPS) }));
  const prevStep = () => setState((prev) => ({ ...prev, currentStep: Math.max(prev.currentStep - 1, 1) }));

  if (state.completed) return null;

  return (
    // `overflow-y-auto` is load-bearing: the steps are content-sized, and
    // step 3 (providers) and step 5 (agent list) exceed the viewport at the
    // larger font steps. Without it the fixed shell clips the bottom of the
    // card — including the Next/Save buttons.
    <div className="fixed inset-0 z-50 flex items-center justify-center overflow-y-auto bg-page-bg py-8">
      <div className="w-full max-w-lg px-8">
        {/* Progress bar */}
        <div className="mb-6">
          <div className="flex items-center gap-1">
            {Array.from({ length: TOTAL_STEPS }, (_, i) => (
              <div
                key={i}
                className={cn(
                  "h-1 flex-1 rounded-full transition-colors",
                  i < state.currentStep ? "bg-zinc-800 dark:bg-zinc-200" : "bg-zinc-200 dark:bg-zinc-700",
                )}
              />
            ))}
          </div>
          <p className="mt-2 text-xs text-text-tertiary">{t("onboarding.step", { current: state.currentStep, total: TOTAL_STEPS })}</p>
        </div>

        {/* Step content */}
        {state.currentStep === 1 && <WelcomeStep onNext={nextStep} onSkip={completeOnboarding} />}
        {state.currentStep === 2 && <GatewayStep onNext={nextStep} onPrev={prevStep} />}
        {state.currentStep === 3 && <ApiKeyStep onNext={nextStep} onPrev={prevStep} />}
        {state.currentStep === 4 && (
          <IdentityStep
            name={state.name}
            language={state.language}
            timezone={state.timezone}
            city={state.city}
            occupation={state.occupation}
            onUpdate={(updates) => setState((prev) => ({ ...prev, ...updates }))}
            onNext={nextStep}
            onPrev={prevStep}
          />
        )}
        {state.currentStep === 5 && <InstallAgentStep onComplete={completeOnboarding} onPrev={prevStep} />}
      </div>
    </div>
  );
}

/** Step 1: Welcome */
function WelcomeStep({ onNext, onSkip }: { onNext: () => void; onSkip: () => void }) {
  const { t } = useTranslation();
  return (
    <div className="text-center">
      <div className="text-xl">🎉🎉🎉</div>
      <h1 className="mt-3 text-lg font-semibold flex items-center justify-center gap-2"><span>{t("onboarding.welcome.title")}</span><img src={brandMark} alt="ACowork" className="h-7" /></h1>
      <p className="mt-2 text-xs text-text-tertiary">{t("onboarding.welcome.subtitle")}</p>
      <div className="mt-6 space-y-2">
        <button
          onClick={onNext}
          className="w-full rounded btn-solid py-[var(--ui-btn-py)] text-xs font-medium"
        >
          {t("onboarding.welcome.startSetup")}
        </button>
        <button
          onClick={onSkip}
          className="w-full py-1.5 text-xs text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-300"
        >
          {t("onboarding.welcome.skip")}
        </button>
      </div>
    </div>
  );
}

/** Step 2: Gateway Connection — a single server address (ADR-115).
 *
 * The boot pipeline has already run by the time this wizard is mounted, so
 * this step only reports reachability. Changing the address restarts that
 * pipeline (including auto-starting a local Gateway when localhost is
 * picked); the wizard is re-entered automatically afterwards. */
function GatewayStep({ onNext, onPrev }: { onNext: () => void; onPrev: () => void }) {
  const { t } = useTranslation();
  const { status, checkHealth } = useGatewayStore();
  const gatewayUrl = useSettingsStore((s) => s.gatewayUrl);
  const [checking, setChecking] = useState(false);

  // Preload provider list as soon as Gateway is connected.
  useEffect(() => {
    if (status === "connected") {
      fetchProviders().catch(() => { });
    }
  }, [status]);

  // Re-check whenever the address changes (the boot pipeline re-runs too).
  useEffect(() => {
    checkHealth();
  }, [gatewayUrl, checkHealth]);

  const handleTest = async () => {
    setChecking(true);
    await checkHealth();
    setChecking(false);
  };

  const canProceed = status === "connected";

  return (
    <div>
      <h2 className="text-sm font-semibold">{t("onboarding.gateway.title")}</h2>
      <p className="mt-1 text-xs text-text-tertiary">{t("onboarding.gateway.subtitle")}</p>

      <div className="mt-4 space-y-3">
        <div className="rounded-md border border-border-outer p-3">
          <GatewayAddressPicker
            busy={checking}
            onSelect={(url) => void useSettingsStore.getState().switchGateway(url)}
          />
        </div>

        <div className="rounded-md border border-border-outer p-3">
          <div className="flex items-center gap-2 text-xs">
            <span className="text-text-tertiary">{t("onboarding.gateway.status")}</span>
            {checking ? (
              <span className="text-text-tertiary">{t("onboarding.gateway.checking")}</span>
            ) : status === "connected" ? (
              <>
                <span className="h-2 w-2 rounded-full bg-green-500" />
                <span className="text-green-600 dark:text-green-400">{t("onboarding.gateway.connectedShort")}</span>
              </>
            ) : (
              <>
                <span className="h-2 w-2 rounded-full bg-red-500" />
                <span className="text-red-600 dark:text-red-400">{t("onboarding.gateway.cannotConnect")}</span>
              </>
            )}
          </div>
          {status !== "connected" && (
            <button
              onClick={handleTest}
              disabled={checking}
              className="mt-3 rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
            >
              {checking ? t("onboarding.gateway.startingShort") : t("onboarding.gateway.testConnection")}
            </button>
          )}
        </div>
      </div>

      <div className="mt-6 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-3 py-[var(--ui-btn-py)] text-xs text-text-tertiary hover:text-zinc-700 dark:hover:text-zinc-300">
          {t("onboarding.gateway.back")}
        </button>
        <button
          onClick={onNext}
          disabled={!canProceed}
          className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
        >
          {t("onboarding.gateway.next")}
        </button>
      </div>
    </div>
  );
}

/** Step 3: API Key
 *
 * Provider setup reuses the Harness pair — `ProviderPicker` (Custom / Local /
 * Remote groups, each with a working Connect button) plus the `AddProviderFlow`
 * dialog — instead of a wizard-private form.
 *
 * The hand-rolled copy that used to live here was a partial re-implementation
 * of that flow and could not do the two things a first-run user needs most:
 * the "Local Providers" card was read-only text (no `onConnect`, so Ollama /
 * LM Studio were not selectable) and there was no custom-provider path at all,
 * let alone model discovery, per-account aliases or a compact model. Keeping
 * one implementation also means the wizard picks up provider-catalog work for
 * free rather than drifting behind Settings.
 */
function ApiKeyStep({ onNext, onPrev }: { onNext: () => void; onPrev: () => void }) {
  const { t } = useTranslation();
  const [providers, setProviders] = useState<ProviderListEntry[]>([]);
  const [keys, setKeys] = useState<VaultKeyEntry[]>([]);
  const [showAddFlow, setShowAddFlow] = useState(false);
  const [addFlowProvider, setAddFlowProvider] = useState<string | undefined>(undefined);
  const [addFlowEntry, setAddFlowEntry] = useState<ProviderListEntry | undefined>(undefined);

  const loadProviders = useCallback(async () => {
    try {
      setProviders(await fetchProviders());
    } catch {
      setProviders([]);
    }
  }, []);

  const loadKeys = useCallback(async () => {
    try {
      setKeys(await invoke<VaultKeyEntry[]>("list_keys"));
    } catch {
      /* Gateway may not be reachable yet — step 2 gates on `connected`, but
         the vault call can still fail while it is warming up. */
    }
  }, []);

  useEffect(() => {
    loadProviders();
    loadKeys();
  }, [loadProviders, loadKeys]);

  // One entry per provider that has at least one account. The picker hides
  // configured providers, so without this strip the step would look untouched
  // right after a successful connection — and `keys` is also what the Next
  // affordance reasons about.
  const configuredProviders = useMemo(
    () => Array.from(new Set(keys.map((k) => k.provider))),
    [keys],
  );

  const openAddFlow = (providerId?: string, entry?: ProviderListEntry) => {
    setAddFlowProvider(providerId);
    setAddFlowEntry(entry);
    setShowAddFlow(true);
  };

  const handleAddFlowSuccess = async () => {
    await loadKeys();
    await loadProviders();
    // Same shared signal Harness dispatches — the AppLayout shell listens
    // for it and reloads the shared model inventory immediately.
    window.dispatchEvent(new CustomEvent("models-added"));
  };

  return (
    <div>
      <h2 className="text-sm font-semibold">{t("onboarding.apiKey.title")}</h2>
      <p className="mt-1 text-xs text-text-tertiary">{t("onboarding.apiKey.subtitle")}</p>

      <div className="mt-4 space-y-3">
        {configuredProviders.length > 0 && (
          <div className="rounded-md border border-border-outer p-3">
            <p className="text-11 font-medium">{
              t("harness.configuredProviders", { count: configuredProviders.length })
            }</p>
            <div className="mt-1.5 flex flex-wrap gap-1.5">
              {configuredProviders.map((id) => (
                <span
                  key={id}
                  className="flex items-center gap-1 rounded bg-zinc-100 px-1.5 py-0.5 text-10 text-text-secondary dark:bg-zinc-700"
                >
                  <ProviderLogo providerId={id} size={12} />
                  {providers.find((p) => p.id === id)?.name ?? id}
                </span>
              ))}
            </div>
          </div>
        )}

        <div className="rounded-md border border-border-outer p-3">
          <ProviderPicker
            providers={providers}
            keys={keys}
            onConnect={(providerId, entry) => openAddFlow(providerId, entry)}
            onAddCustom={() => openAddFlow(undefined, undefined)}
            onCatalogRefreshed={async () => {
              // A catalog refresh re-derives the capabilities of already-
              // configured providers too, so both lists are re-read.
              await loadProviders();
              await loadKeys();
            }}
          />
        </div>
      </div>

      {/* Add / connect dialog — `initialStep` skips the picker when the user
          clicked a specific provider (same contract Harness uses). */}
      <AddProviderFlow
        open={showAddFlow}
        initialStep={addFlowProvider ? "add" : "custom"}
        initialProvider={addFlowProvider}
        initialProviderEntry={addFlowEntry}
        onClose={() => {
          setShowAddFlow(false);
          setAddFlowProvider(undefined);
          setAddFlowEntry(undefined);
        }}
        onSuccess={handleAddFlowSuccess}
      />

      <div className="mt-6 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-3 py-[var(--ui-btn-py)] text-xs text-text-tertiary hover:text-zinc-700 dark:hover:text-zinc-300">
          {t("onboarding.apiKey.back")}
        </button>
        <button
          onClick={onNext}
          className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium"
        >
          {t("onboarding.apiKey.next")}
        </button>
      </div>
    </div>
  );
}

/** Step 4: Identity */
function IdentityStep({
  name, language, timezone, city, occupation,
  onUpdate, onNext, onPrev,
}: {
  name: string; language: string; timezone: string; city: string; occupation: string;
  onUpdate: (updates: Partial<OnboardingState>) => void;
  onNext: () => void; onPrev: () => void;
}) {
  const { t } = useTranslation();
  const requiredFilled = name.trim() && language && timezone;

  return (
    <div>
      <h2 className="text-sm font-semibold">{t("onboarding.identity.title")}</h2>
      <p className="mt-1 text-xs text-text-tertiary">{t("onboarding.identity.subtitle")}</p>

      <div className="mt-4 space-y-3">
        <div>
          <label className="mb-1 block text-xs text-text-tertiary">{t("onboarding.identity.nameLabel")}</label>
          <StyledInput
            type="text"
            value={name}
            onChange={(e) => onUpdate({ name: e.target.value })}
            placeholder={t("onboarding.identity.namePlaceholder")}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-text-tertiary">{t("onboarding.identity.languageLabel")}</label>
          <Dropdown
            value={language}
            onChange={(v) => onUpdate({ language: v })}
            options={[
              { value: "zh-CN", label: t("onboarding.identity.languages.zh-CN") },
              { value: "zh-TW", label: t("onboarding.identity.languages.zh-TW") },
              { value: "en", label: t("onboarding.identity.languages.en") },
              { value: "ja", label: t("onboarding.identity.languages.ja") },
              { value: "ko", label: t("onboarding.identity.languages.ko") },
            ]}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-text-tertiary">{t("onboarding.identity.timezoneLabel")}</label>
          <Dropdown
            value={timezone}
            onChange={(v) => onUpdate({ timezone: v })}
            options={[
              { value: "Asia/Shanghai", label: "Asia/Shanghai" },
              { value: "Asia/Tokyo", label: "Asia/Tokyo" },
              { value: "America/New_York", label: "America/New_York" },
              { value: "America/Los_Angeles", label: "America/Los_Angeles" },
              { value: "Europe/London", label: "Europe/London" },
              { value: "UTC", label: "UTC" },
            ]}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-text-tertiary">{t("onboarding.identity.cityLabel")}</label>
          <StyledInput
            type="text"
            value={city}
            onChange={(e) => onUpdate({ city: e.target.value })}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-text-tertiary">{t("onboarding.identity.occupationLabel")}</label>
          <StyledInput
            type="text"
            value={occupation}
            onChange={(e) => onUpdate({ occupation: e.target.value })}
          />
        </div>
      </div>

      <div className="mt-6 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-3 py-[var(--ui-btn-py)] text-xs text-text-tertiary hover:text-zinc-700 dark:hover:text-zinc-300">
          {t("onboarding.identity.back")}
        </button>
        <button
          onClick={onNext}
          disabled={!requiredFilled}
          className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
        >
          {t("onboarding.identity.next")}
        </button>
      </div>
    </div>
  );
}

/**
 * Run `worker` over `items` with at most `limit` concurrent executions.
 *
 * ADR-059 §5.2: the onboarding flow submits install operations with a
 * bounded concurrency of 3 (the node control plane installs packages in
 * parallel, but unbounded fan-out would overload a cold start).
 */
async function runBounded<T>(
  items: T[],
  limit: number,
  worker: (item: T) => Promise<void>,
): Promise<void> {
  let idx = 0;
  const runners = Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (idx < items.length) {
      const item = items[idx++];
      await worker(item);
    }
  });
  await Promise.all(runners);
}

/** Map a bundled agent resource name to its package id (ADR-059 §6).
 *
 * The id is the `agent_id` inside each package's manifest.toml — NOT the
 * `.agent` filename. Used for display / dedup only since ADR-073: the
 * Gateway inventory is keyed by the gateway-generated instance id (the
 * install ack's `instance_id`), which is what `wait_agent_installed`
 * must poll. */
function bundledAgentId(resourceName: string): string {
  switch (resourceName) {
    case "software-architect-agent": return "com.acowork.software-architect";
    case "senior-engineer-agent": return "com.acowork.senior-engineer";
    case "quality-assurance-agent": return "com.acowork.quality-assurance";
    case "project-manager-agent": return "com.acowork.project-manager";
    case "product-manager-agent": return "com.acowork.product-manager";
    case "document-manager-agent": return "com.acowork.document-manager";
    default: return resourceName;
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Step 5: Install first Agent */
function InstallAgentStep({ onComplete, onPrev }: { onComplete: () => void; onPrev: () => void }) {
  const { t } = useTranslation();
  const [installing, setInstalling] = useState<string | null>(null);
  const [installError, setInstallError] = useState<string | null>(null);
  // ADR-059 §5.2: live bootstrap snapshot while installs are running —
  // surfaces the Gateway's own phase_detail ("3/5 required ready" etc.)
  // instead of a generic "waiting for node" message.
  const [bootstrap, setBootstrap] = useState<BootstrapStateView | null>(null);
  const [selectedAgents, setSelectedAgents] = useState<string[]>(() => RECOMMENDED_AGENTS.map((agent) => agent.resourceName));

  interface InstallOp {
    resourceName: string;
    name: string;
    agentId: string;
    state: "pending" | "submitted" | "completed" | "failed";
    operationId?: string;
    error?: string;
  }

  const [ops, setOps] = useState<Record<string, InstallOp>>({});
  // ADR-113: the install target comes from protocol state, never from a
  // Gateway-mode default. Both this step and the Agent library dialog use
  // the same `usableNodesOf` predicate and the same empty-state banner.
  const nodes = useAgentStore((st) => st.nodes);
  const [startingNode, setStartingNode] = useState(false);
  const { addToast } = useToast();
  const usableNodes = useMemo(() => usableNodesOf(nodes), [nodes]);
  const noAvailableNode = usableNodes.length === 0;
  // The node shown to the user must be the one the click targets. Pin it so
  // a topology refresh between render and submit cannot silently move the
  // install to a different machine.
  const displayedNodeId = usableNodes[0]?.node_id ?? null;
  const noNodeDetail = useMemo(() => {
    const key = noNodeDetailKey(noNodeDetailOf(nodes));
    return key ? t(key) : null;
  }, [nodes, t]);

  // ADR-113 D6: shared replayable topology subscription. Onboarding runs
  // before the main layout is mounted, so it owns this while it is visible.
  useNodeTopologyRefresh(true);

  const startLocalNode = useCallback(async () => {
    if (startingNode) return;
    setStartingNode(true);
    try {
      const res = await invoke<{ already_running?: boolean }>("create_local_node");
      addToast({
        type: "info",
        message: res.already_running
          ? t("agentHub.startLocalNodeAlreadyRunning")
          : t("agentHub.startLocalNodeStarted"),
      });
      void useAgentStore.getState().fetchNodes().catch((e) => {
        log.warn("[onboarding] node list refresh failed:", e);
      });
    } catch (e) {
      addToast({
        type: "error",
        message: t("agentHub.startLocalNodeFailed", { error: String(e) }),
      });
    } finally {
      setStartingNode(false);
    }
  }, [addToast, startingNode, t]);

  /** Resolve an explicit node_id right before submitting. Never infers a
   *  default; if the topology changed under us, briefly re-read it. */
  const resolveTargetNode = useCallback(async (): Promise<string> => {
    const deadline = Date.now() + 8_000;
    while (Date.now() < deadline) {
      const candidates = usableNodesOf(useAgentStore.getState().nodes);
      const target = candidates.find((n) => n.node_id === displayedNodeId) ?? candidates[0];
      if (target) return target.node_id;
      await sleep(400);
      await useAgentStore.getState().fetchNodes().catch((e) => {
        log.warn("[onboarding] node list refresh failed while resolving target:", e);
      });
    }
    throw new Error(t("agentHub.noAvailableNode"));
  }, [displayedNodeId, t]);

  const updateOp = (resourceName: string, patch: Partial<InstallOp>) => {
    setOps((prev) => ({
      ...prev,
      [resourceName]: { ...prev[resourceName], ...patch },
    }));
  };

  const toggleRecommendedAgent = (resourceName: string) => {
    setSelectedAgents((prev) => prev.includes(resourceName)
      ? prev.filter((name) => name !== resourceName)
      : [...prev, resourceName]);
  };

  /**
   * ADR-059 §5.1: wait for the Gateway bootstrap phase to reach READY.
   * Polls `GET /api/bootstrap` directly (same channel SplashScreen uses)
   * and streams `phase_detail` into the UI. The Tauri `get_bootstrap`
   * command (MQTT retained snapshot cache) can serve a stale/none
   * snapshot while the MQTT subscribe loop is still settling, which
   * used to burn the whole 30 s window with every agent stuck at
   * "pending" — the direct HTTP poll is authoritative and race-free.
   * Replaces the old "503 / never enrolled" text-match retry loop — the
   * Gateway now answers installs with structured `dependency_not_ready`
   * until its subsystems (vault / mqtt / node.local / system_agent /
   * publisher) are all up.
   */
  const waitBootstrapReady = async (): Promise<BootstrapStateView> => {
    const deadline = Date.now() + 30_000;
    while (Date.now() < deadline) {
      let view: BootstrapStateView | null = null;
      try {
        const resp = await fetch(`${getGatewayUrl()}/api/bootstrap`);
        if (resp.ok) {
          view = (await resp.json()) as BootstrapStateView;
        }
      } catch {
        view = null;
      }
      if (view) {
        setBootstrap(view);
        if (view.phase === "READY" || view.phase === "DEGRADED") return view;
        if (view.phase === "FAILED" || view.phase === "SHUTTING_DOWN") {
          throw new Error(`Gateway ${view.phase}: ${view.phase_detail}`);
        }
      }
      await sleep(400);
    }
    throw new Error("Bootstrap did not reach READY in time");
  };

  const handleInstallRecommended = async () => {
    setInstallError(null);
    setInstalling("");
    const items: InstallOp[] = selectedAgents.map((resourceName) => {
      const agent = RECOMMENDED_AGENTS.find((item) => item.resourceName === resourceName);
      return {
        resourceName,
        name: agent?.name ?? resourceName,
        agentId: bundledAgentId(resourceName),
        state: "pending",
      };
    });
    setOps(Object.fromEntries(items.map((op) => [op.resourceName, op])));
    // Failures are collected per-operation (workers never reject, so all
    // concurrent installs run to completion and every op gets a final
    // state) and surfaced as a combined error at the end.
    const failedOps: string[] = [];
    try {
      // 1) Wait for bootstrap READY — installs are rejected with the
      // structured `dependency_not_ready` code before that.
      await waitBootstrapReady();
      // 2) ADR-113: name the target explicitly. The Gateway never
      // infers a landing node from deployment state.
      const nodeId = await resolveTargetNode();
      // 3) Submit install operations with bounded concurrency (3 in
      // flight). Each ack carries an operation_id; completion is
      // correlated by polling the Gateway inventory for the agent id.
      await runBounded(items, 3, async (op) => {
        try {
          const ack = await invoke<OperationAck>("install_bundled_agent", {
            resourceName: op.resourceName,
            nodeId,
            devMode: true,
          });
          updateOp(op.resourceName, {
            state: "submitted",
            operationId: ack.operation_id,
          });
          await invoke("wait_agent_installed", {
            // ADR-073: the inventory is keyed by the gateway-generated
            // instance id carried in the ack; the package id 404s forever.
            agentId: ack.instance_id ?? op.agentId,
            timeoutSecs: 120,
          });
          updateOp(op.resourceName, { state: "completed" });
        } catch (err) {
          failedOps.push(op.resourceName);
          updateOp(op.resourceName, {
            state: "failed",
            error: err instanceof Error ? err.message : String(err),
          });
        }
      });
      if (failedOps.length > 0) {
        const names = items.filter((op) => failedOps.includes(op.resourceName)).map((op) => op.name);
        setInstallError(
          `${failedOps.length}/${items.length} agent(s) failed to install: ${names.join(", ")}`,
        );
        return;
      }
      // All installs completed — refresh the agent list so newly
      // installed agents appear. ADR-017: Avatar assignment is now
      // server-side (agent_config.json seeded from manifest on first
      // start). No frontend backfill needed.
      await useAgentStore.getState().fetchAgents();
    } catch (err) {
      // waitBootstrapReady failed (bootstrap FAILED / timeout).
      setInstallError(err instanceof Error ? err.message : String(err));
    } finally {
      setInstalling(null);
    }
  };

  const handleInstallFromFile = async () => {
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: "Agent Package", extensions: ["agent"] }],
      });
      if (selected) {
        setInstallError(null);
        setInstalling(selected);
        // Wait for bootstrap READY first: the Gateway rejects installs
        // with `dependency_not_ready` until its subsystems are up.
        await waitBootstrapReady();
        const nodeId = await resolveTargetNode();
        await invoke("install_agent", { packagePath: selected, nodeId });
        // Refresh the agent list so the backfill runs for the new agent.
        await useAgentStore.getState().fetchAgents();
        setInstalling(null);
      }
    } catch (err) {
      setInstalling(null);
      setInstallError(err instanceof Error ? err.message : String(err));
    }
  };

  const completedCount = Object.values(ops).filter((op) => op.state === "completed").length;
  // `installing` is "" (not null) while the recommended batch is in flight.
  // Every busy check must test against null, never the string's truthiness.
  const installInFlight = installing !== null;
  const totalCount = Object.keys(ops).length;

  return (
    <div>
      <h2 className="text-sm font-semibold">{t("onboarding.installAgent.title")}</h2>
      <p className="mt-1 text-xs text-text-tertiary">{t("onboarding.installAgent.subtitle")}</p>

      <div className="mt-4 space-y-3">
        {noAvailableNode && (
          <NoNodeBanner
            starting={startingNode}
            onStart={() => void startLocalNode()}
            detail={noNodeDetail}
          />
        )}
        {!noAvailableNode && (
          <p className="text-10 text-text-tertiary">
            {t("onboarding.installAgent.targetNode", {
              node: usableNodes[0].node_name ?? usableNodes[0].hostname ?? usableNodes[0].node_id,
            })}
          </p>
        )}
        {/* Bootstrap readiness status (ADR-059 §5.2) */}
        {installing !== null && bootstrap && bootstrap.phase !== "READY" && bootstrap.phase !== "DEGRADED" && (
          <p className="text-xs text-text-tertiary">
            {t("onboarding.installAgent.preparing", { detail: bootstrap.phase_detail })}
          </p>
        )}

        <div className="rounded-md border border-border-outer p-3">
          <div className="mb-2 flex items-center justify-between">
            <div>
              <h3 className="text-11 font-medium">{t("onboarding.installAgent.recommendedTitle")}</h3>
              <p className="mt-0.5 text-10 text-text-tertiary">{t("onboarding.installAgent.recommendedSubtitle")}</p>
            </div>
            <button
              onClick={() => setSelectedAgents(selectedAgents.length === RECOMMENDED_AGENTS.length ? [] : RECOMMENDED_AGENTS.map((agent) => agent.resourceName))}
              className="text-xs text-text-tertiary hover:text-zinc-800 dark:hover:text-zinc-200"
            >
              {selectedAgents.length === RECOMMENDED_AGENTS.length ? t("onboarding.installAgent.clear") : t("onboarding.installAgent.selectAll")}
            </button>
          </div>
          <div className="max-h-56 space-y-2 overflow-y-auto pr-1">
            {RECOMMENDED_AGENTS.map((agent) => {
              const op = ops[agent.resourceName];
              return (
                <label
                  key={agent.resourceName}
                  className="flex cursor-pointer gap-2 rounded-md border border-zinc-100 p-2.5 hover:bg-zinc-50 dark:border-zinc-800 dark:hover:bg-zinc-800"
                >
                  <input
                    type="checkbox"
                    checked={selectedAgents.includes(agent.resourceName)}
                    onChange={() => toggleRecommendedAgent(agent.resourceName)}
                    disabled={installInFlight}
                    className="mt-0.5 h-4 w-4 rounded border-zinc-300"
                  />
                  <span className="flex-1">
                    <span className="block text-xs font-medium">{agent.name} · {agent.role}</span>
                    <span className="mt-0.5 block text-10 text-text-tertiary">{agent.description}</span>
                  </span>
                  {/* Per-operation status badge (ADR-059 §6: operation_id tracking) */}
                  {op && (
                    <span className="self-center text-10">
                      {op.state === "pending" && <span className="text-text-tertiary">{t("onboarding.installAgent.opPending")}</span>}
                      {op.state === "submitted" && op.operationId && (
                        <span className="text-blue-600 dark:text-blue-400">
                          {t("onboarding.installAgent.opSubmitted", { opId: op.operationId.slice(0, 8) })}
                        </span>
                      )}
                      {op.state === "completed" && <span className="text-green-600 dark:text-green-400">{t("onboarding.installAgent.opCompleted")}</span>}
                      {op.state === "failed" && <span className="text-red-600 dark:text-red-400">{t("onboarding.installAgent.opFailed")}</span>}
                    </span>
                  )}
                </label>
              );
            })}
          </div>
          <button
            onClick={handleInstallRecommended}
            disabled={installInFlight || selectedAgents.length === 0 || noAvailableNode}
            title={noAvailableNode ? t("agentHub.noAvailableNode") : undefined}
            className="mt-3 w-full rounded btn-solid py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
          >
            {installInFlight
              ? totalCount > 0 && completedCount > 0
                ? t("onboarding.installAgent.installProgress", { done: completedCount, total: totalCount })
                : t("onboarding.installAgent.waitingReady")
              : t("onboarding.installAgent.installSelected", { count: selectedAgents.length })}
          </button>
        </div>

        <button
          onClick={handleInstallFromFile}
          disabled={installInFlight || noAvailableNode}
          title={noAvailableNode ? t("agentHub.noAvailableNode") : undefined}
          className="w-full rounded-md border border-zinc-200 p-3 text-left transition-colors hover:bg-zinc-50 dark:border-zinc-700 dark:hover:bg-zinc-800"
        >
          <span className="text-11 font-medium">{t("onboarding.installAgent.fromFileTitle")}</span>
          <p className="mt-0.5 text-10 text-text-tertiary">{t("onboarding.installAgent.fromFileSubtitle")}</p>
        </button>

        {installInFlight && !bootstrap && (
          <p className="text-xs text-text-tertiary">{t("onboarding.installAgent.waitingReady")}</p>
        )}
        {installError && (
          <ErrorBox message={installError} />
        )}
      </div>

      <div className="mt-6 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-3 py-[var(--ui-btn-py)] text-xs text-text-tertiary hover:text-zinc-700 dark:hover:text-zinc-300">
          {t("onboarding.installAgent.back")}
        </button>
        <button
          onClick={onComplete}
          className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium"
        >
          {t("onboarding.installAgent.complete")}
        </button>
      </div>
    </div>
  );
}
