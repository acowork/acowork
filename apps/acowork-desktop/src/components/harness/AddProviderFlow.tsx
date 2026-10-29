import { useState, useEffect, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { VaultKeyEntry, ModelInfo, ModelCapabilitiesInfo, ProviderListEntry } from "../../lib/types";
import { StyledInput } from "../common/StyledInput";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { needsApiKey, keyPlaceholder, isLocalProvider } from "../../lib/providers";
import { fetchProviderModels, discoverModels, fetchProviders } from "../../lib/gateway-api";
import { ModelMultiSelect } from "./ModelMultiSelect";
import { useModelProtocolState } from "./useModelProtocolState";
import { ProviderPicker } from "./ProviderPicker";
import { parseOfflineJson, OFFLINE_JSON_EXAMPLE } from "./offlineJson";
import { useTranslation } from "../../i18n/useTranslation";
import { ChevronLeft, Minus, Plus, Plug } from "lucide-react";
import { ErrorBox } from "../common/ErrorBox";

interface AddProviderFlowProps {
  open: boolean;
  onClose: () => void;
  onSuccess: () => void;
  /** Skip picker and go directly to add/custom step. */
  initialStep?: "picker" | "add" | "custom";
  /** Provider ID when initialStep="add". */
  initialProvider?: string;
  /** Provider list entry for baseUrl default when initialStep="add". */
  initialProviderEntry?: ProviderListEntry;
}

type Step = "picker" | "add" | "custom";

/**
 * ADR-103 D1 wire fields for the next `add_key` call.
 *
 * A brand-new provider has no stored selection, so every Responses row counts
 * as "changed" — which is exactly what makes the first save probe them (D12).
 * Empty maps are omitted so the Gateway keeps its "untouched" semantics.
 */
function protocolPayload(
  protocols: ReturnType<typeof useModelProtocolState>,
  models: ModelInfo[],
): { modelProtocols?: Record<string, string | null>; modelBaseUrls?: Record<string, string> } {
  const intent = protocols.buildIntent(models);
  const payload: {
    modelProtocols?: Record<string, string | null>;
    modelBaseUrls?: Record<string, string>;
  } = {};
  if (Object.keys(intent.model_protocols).length > 0) {
    payload.modelProtocols = intent.model_protocols;
  }
  if (Object.keys(intent.model_base_urls).length > 0) {
    payload.modelBaseUrls = intent.model_base_urls;
  }
  return payload;
}

/** Self-contained dialog that encapsulates the entire provider-add flow:
 *  picker → add dialog (local/remote) or custom-provider dialog. */
export function AddProviderFlow({
  open,
  onClose,
  onSuccess,
  initialStep = "picker",
  initialProvider,
  initialProviderEntry,
}: AddProviderFlowProps) {
  const { t } = useTranslation();

  // ── Dialog-level state ──
  const [step, setStep] = useState<Step>(initialStep);
  const [dynamicProviders, setDynamicProviders] = useState<ProviderListEntry[]>([]);
  const [keys, setKeys] = useState<VaultKeyEntry[]>([]);

  // ── Add-dialog state ──
  const [selectedProvider, setSelectedProvider] = useState<string>(initialProvider ?? "");
  /** Multi-key entries: each row = {alias, key} pair. Always at least one
   *  row. New rows are appended on `+`, the last row can never be
   *  removed so we always keep at least one editable input. Hard cap
   *  protects the dialog from runaway state — beyond this the list
   *  becomes scrollable. */
  const [newKeyEntries, setNewKeyEntries] = useState<{ alias: string; key: string }[]>([
    { alias: "", key: "" },
  ]);
  const [newBaseUrl, setNewBaseUrl] = useState(initialProviderEntry?.api ?? "");
  const [newModels, setNewModels] = useState<string[]>([]);
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [newModelCaps, setNewModelCaps] = useState<Record<string, ModelCapabilitiesInfo>>({});
  // ADR-103 D11: protocol selection state for the "add provider" step and the
  // custom-provider step (two independent lists, so two independent states).
  const newProtocols = useModelProtocolState();
  const customProtocols = useModelProtocolState();
  const [newExpandedModels, setNewExpandedModels] = useState<Set<string>>(new Set());
  const [newCompactModel, setNewCompactModel] = useState("");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ success: boolean; message: string } | null>(null);

  /** Upper bound on the number of key entries the dialog will accept.
   *  `ponytail: ceiling for UX, not enforced on the backend. Lift if
   *  power users legitimately need > 256 accounts per provider. */
  const MAX_KEY_ENTRIES = 256;

  // ── Custom-provider dialog state ──
  const [customProviderName, setCustomProviderName] = useState("");
  const [customProviderId, setCustomProviderId] = useState("");
  const [customBaseUrl, setCustomBaseUrl] = useState("");
  /** Same multi-entry shape as `newKeyEntries` — the custom-provider
   *  step uses the same UI primitive. */
  const [customKeyEntries, setCustomKeyEntries] = useState<{ alias: string; key: string }[]>([
    { alias: "", key: "" },
  ]);
  const [customModels, setCustomModels] = useState<string[]>([]);
  const [customAvailableModels, setCustomAvailableModels] = useState<ModelInfo[]>([]);
  const [customModelsLoading, setCustomModelsLoading] = useState(false);
  const [customDiscoverError, setCustomDiscoverError] = useState<string | null>(null);
  const [customTesting, setCustomTesting] = useState(false);
  const [customModelCaps, setCustomModelCaps] = useState<Record<string, ModelCapabilitiesInfo>>({});
  const [customExpandedModels, setCustomExpandedModels] = useState<Set<string>>(new Set());
  /** Compact (summarization) model for this custom provider. Mirrors
   *  `newCompactModel` on the add step — separate state so stepping
   *  picker → add → back → custom never leaks a pick across steps. */
  const [customCompactModel, setCustomCompactModel] = useState("");
  // Manual JSON import — escape hatch when the upstream base URL has no
  // /models endpoint. Mirrors the offline_providers.json model-spec shape
  // so the user can paste entries from the model vendor's docs verbatim.
  const [customJsonInput, setCustomJsonInput] = useState("");
  const [customJsonError, setCustomJsonError] = useState<string | null>(null);

  // ── Derived ──
  const selectedProviderIsLocal = useMemo(
    () => isLocalProvider(selectedProvider),
    [selectedProvider],
  );
  // Custom providers must keep their `custom` flag through the Connect path,
  // otherwise re-adding one (e.g. a custom provider whose last account was
  // removed) would silently drop it out of the Custom group into Remote.
  const selectedProviderIsCustom = useMemo(
    () => dynamicProviders.find((p) => p.id === selectedProvider)?.custom ?? false,
    [dynamicProviders, selectedProvider],
  );

  // ── Data fetching ──
  const fetchKeys = useCallback(async () => {
    try {
      const result = await invoke<VaultKeyEntry[]>("list_keys");
      setKeys(result);
    } catch { /* Gateway may not be running */ }
  }, []);

  const loadProviders = useCallback(async () => {
    try {
      const providers = await fetchProviders();
      setDynamicProviders(providers);
    } catch { /* Gateway may not be running */ }
  }, []);

  const fetchModels = useCallback(async (providerId: string): Promise<ModelInfo[]> => {
    try {
      const data = await fetchProviderModels(providerId);
      return data.models ?? [];
    } catch {
      return [];
    }
  }, []);

  // ── Effects ──
  // On open: wipe every form field, then re-apply the initial step.
  // Without the wipe, re-opening the dialog (or switching tabs away and
  // back) leaks the last session's state into the new one — e.g. typing a
  // custom provider name, closing, and re-opening would show the old
  // name pre-filled. Mirrors `if (!open) return null` below, which only
  // unmounts the visual shell; React keeps the hook state alive.
  useEffect(() => {
    if (!open) return;
    // Wipe add-step state.
    setSelectedProvider(initialProvider ?? "");
    setNewBaseUrl(initialProviderEntry?.api ?? "");
    setNewKeyEntries([{ alias: "", key: "" }]);
    setNewModels([]);
    setAvailableModels([]);
    setNewModelCaps({});
    setNewExpandedModels(new Set());
    setNewCompactModel("");
    setTesting(false);
    setTestResult(null);
    // Wipe custom-step state.
    setCustomProviderName("");
    setCustomProviderId("");
    setCustomBaseUrl("");
    setCustomKeyEntries([{ alias: "", key: "" }]);
    setCustomModels([]);
    setCustomAvailableModels([]);
    setCustomModelsLoading(false);
    setCustomDiscoverError(null);
    setCustomTesting(false);
    setCustomModelCaps({});
    setCustomExpandedModels(new Set());
    setCustomCompactModel("");
    setCustomJsonInput("");
    setCustomJsonError(null);

    fetchKeys();
    loadProviders();
    setStep(initialStep);
    if (initialProvider) {
      setModelsLoading(true);
      fetchModels(initialProvider).then((models) => {
        setAvailableModels(models);
        setModelsLoading(false);
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  // ── Helpers ──
  const slugifyProviderId = (name: string): string => {
    return "custom-" + name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
  };

  // ── Picker → add transition ──
  const handleConnect = (providerId: string, entry: ProviderListEntry) => {
    setSelectedProvider(providerId);
    setNewBaseUrl(entry.api ?? "");
    setNewKeyEntries([{ alias: "", key: "" }]);
    setNewModels([]);
    setNewModelCaps({});
    setNewExpandedModels(new Set());
    setNewCompactModel("");
    setTestResult(null);
    setStep("add");
    setModelsLoading(true);
    fetchModels(providerId).then((models) => {
      setAvailableModels(models);
      newProtocols.hydrate(models);
      setModelsLoading(false);
    });
  };

  // ── Picker → custom transition ──
  const handleStartCustom = () => {
    setCustomProviderName("");
    setCustomProviderId("");
    setCustomBaseUrl("");
    setCustomKeyEntries([{ alias: "", key: "" }]);
    setCustomModels([]);
    setCustomAvailableModels([]);
    setCustomDiscoverError(null);
    setCustomModelCaps({});
    setCustomExpandedModels(new Set());
    setCustomCompactModel("");
    setCustomJsonInput("");
    setCustomJsonError(null);
    setStep("custom");
  };

  // ── Save handlers ──
  const handleAdd = async () => {
    // Strip empty trailing rows; require at least one non-empty key.
    const validEntries = newKeyEntries
      .map((e) => ({ alias: e.alias.trim(), key: e.key.trim() }))
      .filter((e) => e.key.length > 0);
    if (!selectedProviderIsLocal && needsApiKey(selectedProvider) && validEntries.length === 0) {
      setTestResult({ success: false, message: t("harness.pleaseEnterApiKey") });
      return;
    }
    const keysPayload = validEntries.length > 0
      ? validEntries
      : [{ alias: "", key: "" }]; // local providers send one empty entry

    // Local providers: skip key test, save directly
    if (selectedProviderIsLocal) {
      setTesting(true);
      try {
        await invoke("add_key", {
          provider: selectedProvider,
          keys: keysPayload,
          baseUrl: newBaseUrl || undefined,
          defaultModel: undefined,
          models: newModels.length > 0 ? newModels : undefined,
          modelCapabilities: newModels.length > 0 ? newModelCaps : undefined,
          compactModel: newCompactModel || undefined,
          ...protocolPayload(newProtocols, availableModels),
        });
        window.dispatchEvent(new CustomEvent('models-added'));
        onSuccess();
        onClose();
      } catch (e) {
        alert(`${t("harness.failedConnectLocal")}: ${e}`);
      }
      setTesting(false);
      return;
    }

    // Remote providers: test the first non-empty key, then save all.
    // The single-key test only validates connectivity — additional keys
    // are saved verbatim and assumed to be equivalent. This matches the
    // legacy single-key UX where one test round-trip covers the whole
    // provider.
    const firstKey = validEntries[0].key;
    setTesting(true);
    setTestResult(null);

    // Snapshot this provider's existing accounts so the cleanup below removes
    // exactly what this test added. A bare `remove_key(provider)` would wipe
    // the user's already-configured keys of the same provider (and the error
    // path would leave the test key behind).
    const before = await invoke<VaultKeyEntry[]>("list_keys").catch(() => []);
    const knownAccountIds = new Set(
      before.filter((k) => k.provider === selectedProvider).map((k) => k.account_id),
    );
    const cleanupTestKeys = async () => {
      const after = await invoke<VaultKeyEntry[]>("list_keys").catch(() => []);
      for (const k of after) {
        if (k.provider === selectedProvider && k.account_id && !knownAccountIds.has(k.account_id)) {
          await invoke("remove_key", { provider: selectedProvider, accountId: k.account_id }).catch(() => {});
        }
      }
    };

    try {
      await invoke("add_key", {
        provider: selectedProvider,
        keys: [{ alias: validEntries[0].alias, key: firstKey }],
        baseUrl: newBaseUrl || undefined,
        custom: selectedProviderIsCustom || undefined,
      });
      await fetchProviderModels(selectedProvider);
      setTestResult({ success: true, message: t("harness.apiKeyValid") });
    } catch (e: any) {
      const errorMsg = e?.message || e?.toString() || "Test failed";
      setTestResult({ success: false, message: errorMsg });
      await cleanupTestKeys();
      setTesting(false);
      return;
    }
    await cleanupTestKeys();
    setTesting(false);

    // Save
    try {
      await invoke("add_key", {
        provider: selectedProvider,
        keys: keysPayload,
        baseUrl: newBaseUrl || undefined,
        defaultModel: undefined,
        models: newModels.length > 0 ? newModels : undefined,
        compactModel: newCompactModel || undefined,
        custom: selectedProviderIsCustom || undefined,
        ...protocolPayload(newProtocols, availableModels),
      });
      window.dispatchEvent(new CustomEvent('models-added'));
      onSuccess();
      onClose();
    } catch (e) {
      alert(`${t("harness.failedAddKey")}: ${e}`);
    }
  };

  const handleDiscoverCustomModels = async () => {
    const url = customBaseUrl.trim();
    if (!url) return;
    setCustomModelsLoading(true);
    setCustomDiscoverError(null);
    setCustomAvailableModels([]);
    // Probe with the first non-empty key; custom providers without a key
    // can still call `/models` on OpenAI-compatible endpoints.
    const probeKey = customKeyEntries.find((e) => e.key.trim().length > 0)?.key.trim();
    try {
      const models = await discoverModels(url, probeKey);
      setCustomAvailableModels(models);
      customProtocols.hydrate(models);
    } catch (e: any) {
      setCustomDiscoverError(e?.message || String(e));
    } finally {
      setCustomModelsLoading(false);
    }
  };

  const handleImportCustomJson = () => {
    const raw = customJsonInput.trim();
    if (!raw) return;
    setCustomJsonError(null);
    const result = parseOfflineJson(raw);
    if (!result) {
      setCustomJsonError(t("harness.customJsonParseError"));
      return;
    }
    // Replace discovered list. Keep previously selected IDs only if they
    // still exist in the new list — otherwise the user would silently lose
    // selections without explanation.
    setCustomAvailableModels(result.models);
    setCustomModelCaps((prev) => ({ ...result.caps, ...prev }));
    setCustomModels((prev) => prev.filter((id) => result.models.some((m) => m.id === id)));
    setCustomDiscoverError(null);
  };

  const handleUseExampleJson = () => {
    setCustomJsonInput(OFFLINE_JSON_EXAMPLE);
    if (customJsonError) setCustomJsonError(null);
  };

  const handleAddCustom = async () => {
    const name = customProviderName.trim();
    const id = customProviderId.trim();
    const url = customBaseUrl.trim();
    if (!name) { alert(t("harness.customProviderNameRequired")); return; }
    if (!id) { alert(t("harness.customProviderIdRequired")); return; }
    if (!url) { alert(t("harness.customBaseUrlRequired")); return; }
    if (dynamicProviders.some(p => p.id === id) || keys.some(k => k.provider === id)) {
      alert(t("harness.providerIdExists"));
      return;
    }
    setCustomTesting(true);
    const customValidEntries = customKeyEntries
      .map((e) => ({ alias: e.alias.trim(), key: e.key.trim() }))
      .filter((e) => e.key.length > 0);
    const customKeysPayload = customValidEntries.length > 0
      ? customValidEntries
      : [{ alias: "", key: "" }];
    try {
      await invoke("add_key", {
        provider: id,
        keys: customKeysPayload,
        baseUrl: url,
        models: customModels.length > 0 ? customModels : undefined,
        modelCapabilities: customModels.length > 0 ? customModelCaps : undefined,
        compactModel: customCompactModel || undefined,
        custom: true,
        ...protocolPayload(customProtocols, customAvailableModels),
      });
      window.dispatchEvent(new CustomEvent('models-added'));
      onSuccess();
      onClose();
    } catch (e) {
      alert(`${t("harness.failedAddKey")}: ${e}`);
    } finally {
      setCustomTesting(false);
    }
  };

  if (!open) return null;

  const selectedProviderName = dynamicProviders.find(p => p.id === selectedProvider)?.name || selectedProvider;

  // ── Render ──
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay" onClick={onClose}>
      <div
        className="w-full max-w-lg max-h-[85vh] overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-xl flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header with optional back button */}
        <div className="shrink-0 flex items-center gap-2 border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <Plug className="h-5 w-5 text-text-tertiary " />
          {step !== "picker" && (
            <button
              onClick={() => setStep("picker")}
              className="text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-200"
              aria-label={t("common.back")}
            >
              <ChevronLeft className="h-4 w-4" />
            </button>
          )}
          <h2 className="text-sm font-semibold text-text ">
            {step === "picker" && t("harness.availableProviders")}
            {step === "add" && (selectedProviderIsLocal ? t("harness.connectLocalProvider") : t("harness.addApiKey")) + " " + selectedProviderName}
            {step === "custom" && t("harness.addCustomProvider")}
          </h2>
        </div>

        {/* Scrollable content */}
        <OverlayScrollbar fill="flex" className="px-5 py-4">

          {/* ── Step: Picker ── */}
          {step === "picker" && (
            <ProviderPicker
              providers={dynamicProviders}
              keys={keys}
              onConnect={handleConnect}
              onAddCustom={handleStartCustom}
            />
          )}

          {/* ── Step: Add ── */}
          {step === "add" && (
            <div className="space-y-2">
              {/* Provider display (read-only) */}
              <div>
                <label className="mb-1 block text-xs text-text-tertiary">{t("harness.provider")}</label>
                <div className="w-full rounded-md border border-border-outer bg-zinc-50 px-3 py-2 text-xs  dark:bg-zinc-900">
                  {selectedProviderName}
                </div>
              </div>

              {/* API Keys — one row per account, +/- to add/remove.
                  Hard cap of MAX_KEY_ENTRIES rows. List becomes scrollable
                  past ~5 entries so the dialog height stays stable. */}
              {needsApiKey(selectedProvider) && (
                <div>
                  <div className="mb-1 flex items-baseline justify-between">
                    <label className="block text-xs text-text-tertiary">{t("harness.apiKey")}</label>
                    <span className="text-10 text-text-tertiary">
                      {t("harness.accountLabel", { defaultValue: "alias" })}
                    </span>
                  </div>
                  <OverlayScrollbar className="space-y-1.5 pr-1" maxHeight={200}>
                    {newKeyEntries.map((entry, idx) => (
                      <div key={idx} className="flex items-center gap-1.5">
                        <StyledInput
                          type="text"
                          value={entry.alias}
                          onChange={(e) => {
                            const next = [...newKeyEntries];
                            next[idx] = { ...next[idx], alias: e.target.value };
                            setNewKeyEntries(next);
                          }}
                          placeholder="alias"
                          className="w-[110px] shrink-0"
                        />
                        <StyledInput
                          type="password"
                          value={entry.key}
                          onChange={(e) => {
                            const next = [...newKeyEntries];
                            next[idx] = { ...next[idx], key: e.target.value };
                            setNewKeyEntries(next);
                          }}
                          placeholder={keyPlaceholder(selectedProvider)}
                          className="flex-1"
                        />
                        <button
                          type="button"
                          onClick={() => setNewKeyEntries(newKeyEntries.filter((_, i) => i !== idx))}
                          disabled={newKeyEntries.length <= 1}
                          aria-label="Remove key"
                          className="shrink-0 rounded p-1 text-text-tertiary hover:bg-zinc-100 disabled:opacity-30 disabled:hover:bg-transparent dark:hover:bg-zinc-700"
                        >
                          <Minus className="h-3.5 w-3.5" />
                        </button>
                      </div>
                    ))}
                  </OverlayScrollbar>
                  <button
                    type="button"
                    onClick={() =>
                      setNewKeyEntries([...newKeyEntries, { alias: "", key: "" }])
                    }
                    disabled={newKeyEntries.length >= MAX_KEY_ENTRIES}
                    className="mt-1.5 flex items-center gap-1 text-xs text-text-tertiary hover:text-text-primary disabled:opacity-30 disabled:hover:text-text-tertiary"
                  >
                    <Plus className="h-3.5 w-3.5" />
                    {t("harness.addKey", { defaultValue: "Add key" })}
                  </button>
                </div>
              )}

              {/* Base URL */}
              <div>
                <label className="mb-1 block text-xs text-text-tertiary">{t("harness.baseUrl")}</label>
                <StyledInput
                  type="text"
                  value={newBaseUrl}
                  onChange={(e) => setNewBaseUrl(e.target.value)}
                  placeholder="https://..."
                  fontMono
                />
              </div>

              {/* Model selection (shared multi-select component) */}
              <ModelMultiSelect
                models={availableModels}
                loading={modelsLoading}
                selected={newModels}
                onSelectedChange={setNewModels}
                caps={newModelCaps}
                onCapsChange={setNewModelCaps}
                expandedModels={newExpandedModels}
                onExpandedToggle={(modelId) =>
                  setNewExpandedModels((prev) => {
                    const next = new Set(prev);
                    if (next.has(modelId)) next.delete(modelId);
                    else next.add(modelId);
                    return next;
                  })
                }
                showModelCapEditor={selectedProviderIsLocal}
                protocols={newProtocols}
                // "Re-verify" needs a saved provider; when this provider
                // already has keys it can be probed from here too.
                protocolProviderId={
                  keys.some((k) => k.provider === selectedProvider)
                    ? selectedProvider
                    : undefined
                }
                compactModel={newCompactModel}
                onCompactModelChange={setNewCompactModel}
                showCompactModel={true}
              />

              {/* Test result */}
              {testResult && testResult.success && (
                <div className="rounded-md bg-green-50 px-3 py-2 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-400">
                  {testResult.message}
                </div>
              )}
              {testResult && !testResult.success && (
                <ErrorBox message={testResult.message} />
              )}
            </div>
          )}

          {/* ── Step: Custom ── */}
          {step === "custom" && (
            <div className="space-y-2">
              {/* Provider Name */}
              <div>
                <label className="mb-1 block text-xs text-text-tertiary">{t("harness.customProviderName")}</label>
                <StyledInput
                  type="text"
                  value={customProviderName}
                  onChange={(e) => {
                    setCustomProviderName(e.target.value);
                    setCustomProviderId(slugifyProviderId(e.target.value));
                  }}
                  placeholder="e.g. My GPT Proxy"
                />
              </div>

              {/* Provider ID */}
              <div>
                <label className="mb-1 block text-xs text-text-tertiary">{t("harness.customProviderId")}</label>
                <StyledInput
                  type="text"
                  value={customProviderId}
                  onChange={(e) => setCustomProviderId(e.target.value)}
                  placeholder="e.g. custom-my-gpt-proxy"
                  fontMono
                />
              </div>

              {/* Base URL */}
              <div>
                <label className="mb-1 block text-xs text-text-tertiary">{t("harness.customBaseUrl")}</label>
                <StyledInput
                  type="text"
                  value={customBaseUrl}
                  onChange={(e) => setCustomBaseUrl(e.target.value)}
                  onBlur={() => { if (customBaseUrl.trim()) handleDiscoverCustomModels(); }}
                  onKeyDown={(e) => { if (e.key === "Enter" && customBaseUrl.trim()) { e.preventDefault(); handleDiscoverCustomModels(); } }}
                  placeholder="https://api.example.com/v1"
                  fontMono
                />
              </div>

              {/* API Keys (optional) — same multi-entry shape as the add step. */}
              <div>
                <div className="mb-1 flex items-baseline justify-between">
                  <label className="block text-xs text-text-tertiary">
                    {t("harness.apiKey")} <span className="text-text-tertiary">({t("harness.optional")})</span>
                  </label>
                  <span className="text-10 text-text-tertiary">
                    {t("harness.accountLabel", { defaultValue: "alias" })}
                  </span>
                </div>
                <OverlayScrollbar className="space-y-1.5 pr-1" maxHeight={200}>
                  {customKeyEntries.map((entry, idx) => (
                    <div key={idx} className="flex items-center gap-1.5">
                      <StyledInput
                        type="text"
                        value={entry.alias}
                        onChange={(e) => {
                          const next = [...customKeyEntries];
                          next[idx] = { ...next[idx], alias: e.target.value };
                          setCustomKeyEntries(next);
                        }}
                        placeholder="alias"
                        className="w-[110px] shrink-0"
                      />
                      <StyledInput
                        type="password"
                        value={entry.key}
                        onChange={(e) => {
                          const next = [...customKeyEntries];
                          next[idx] = { ...next[idx], key: e.target.value };
                          setCustomKeyEntries(next);
                        }}
                        placeholder="sk-..."
                        className="flex-1"
                      />
                      <button
                        type="button"
                        onClick={() => setCustomKeyEntries(customKeyEntries.filter((_, i) => i !== idx))}
                        disabled={customKeyEntries.length <= 1}
                        aria-label="Remove key"
                        className="shrink-0 rounded p-1 text-text-tertiary hover:bg-zinc-100 disabled:opacity-30 disabled:hover:bg-transparent dark:hover:bg-zinc-700"
                      >
                        <Minus className="h-3.5 w-3.5" />
                      </button>
                    </div>
                  ))}
                </OverlayScrollbar>
                <button
                  type="button"
                  onClick={() =>
                    setCustomKeyEntries([...customKeyEntries, { alias: "", key: "" }])
                  }
                  disabled={customKeyEntries.length >= MAX_KEY_ENTRIES}
                  className="mt-1.5 flex items-center gap-1 text-xs text-text-tertiary hover:text-text-primary disabled:opacity-30 disabled:hover:text-text-tertiary"
                >
                  <Plus className="h-3.5 w-3.5" />
                  {t("harness.addKey", { defaultValue: "Add key" })}
                </button>
              </div>

              {/* Manual JSON import — fallback when the upstream base URL
                  has no /models endpoint. Mirrors offline_providers.json's
                  model-spec shape so the user can paste vendor docs verbatim. */}
              <details className="rounded-md border border-border-divider">
                <summary className="cursor-pointer select-none px-3 py-1.5 text-xs text-text-tertiary hover:text-text-primary">
                  {t("harness.customJsonInputLabel", { defaultValue: "Or import model JSON" })}
                </summary>
                <div className="space-y-1.5 border-t border-border-divider p-3">
                  <p className="text-10 text-text-tertiary">
                    {t("harness.customJsonImportHint", {
                      defaultValue: "Same shape as offline_providers.json — see the formatted example below.",
                    })}
                  </p>
                  {/* Pretty-printed demo — gives users something to copy
                      from so they know where the schema starts and ends. */}
                  <pre className="max-h-[160px] overflow-auto rounded-md bg-zinc-50 px-2 py-1.5 font-mono text-10 text-text-tertiary dark:bg-zinc-900">
                    {OFFLINE_JSON_EXAMPLE}
                  </pre>
                  <div className="flex justify-end">
                    <button
                      type="button"
                      onClick={handleUseExampleJson}
                      className="text-10 text-text-tertiary hover:text-text-primary"
                    >
                      {t("harness.customJsonUseExample", { defaultValue: "Use this example" })}
                    </button>
                  </div>
                  <textarea
                    value={customJsonInput}
                    onChange={(e) => {
                      setCustomJsonInput(e.target.value);
                      if (customJsonError) setCustomJsonError(null);
                    }}
                    placeholder={t("harness.customJsonInputPlaceholder", {
                      defaultValue: "Paste model JSON here, or click \"Use this example\" above",
                    })}
                    rows={5}
                    className="w-full resize-y rounded-md border border-border-divider bg-transparent px-2 py-1.5 font-mono text-11 outline-none focus:border-zinc-400 dark:focus:border-zinc-500"
                  />
                  {customJsonError && <ErrorBox message={customJsonError} />}
                  <div className="flex justify-end">
                    <button
                      type="button"
                      onClick={handleImportCustomJson}
                      disabled={!customJsonInput.trim()}
                      className="rounded-md bg-zinc-200 px-3 py-1.5 text-xs font-medium text-text hover:bg-zinc-300 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
                    >
                      {t("harness.customJsonImport", { defaultValue: "Import" })}
                    </button>
                  </div>
                </div>
              </details>

              {/* Model discovery status */}
              {customModelsLoading && (
                <div className="rounded-md bg-zinc-50 px-3 py-2 text-xs text-text-tertiary dark:bg-zinc-900">
                  {t("harness.discoveringModels")}
                </div>
              )}
              {customDiscoverError && (
                <ErrorBox message={`${t("harness.discoverFailed")}: ${customDiscoverError}`} />
              )}

              {/* Model selection (shared multi-select component) — only after discover */}
              {customAvailableModels.length > 0 && (
                <ModelMultiSelect
                  models={customAvailableModels}
                  selected={customModels}
                  onSelectedChange={setCustomModels}
                  caps={customModelCaps}
                  onCapsChange={setCustomModelCaps}
                  expandedModels={customExpandedModels}
                  onExpandedToggle={(modelId) =>
                    setCustomExpandedModels((prev) => {
                      const next = new Set(prev);
                      if (next.has(modelId)) next.delete(modelId);
                      else next.add(modelId);
                      return next;
                    })
                  }
                  showModelCapEditor={true}
                  protocols={customProtocols}
                  showCapabilityFilter={false}
                  compactModel={customCompactModel}
                  onCompactModelChange={setCustomCompactModel}
                  showCompactModel={true}
                />
              )}
            </div>
          )}
        </OverlayScrollbar>

        {/* Footer */}
        <div className="shrink-0 flex items-center justify-between gap-2 border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          {/* Status on the left */}
          <div className="flex-1 min-w-0">
            {step === "add" && testResult && testResult.success && (
              <div className="truncate rounded-md bg-green-50 px-3 py-1.5 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-400">
                {testResult.message}
              </div>
            )}
            {step === "add" && testResult && !testResult.success && (
              <div className="truncate text-xs text-red-600 dark:text-red-400" title={testResult.message}>
                {testResult.message}
              </div>
            )}
            {step === "add" && testing && (
              <div className="text-xs text-text-tertiary">{t("harness.testing")}</div>
            )}
          </div>

          {/* Buttons on the right */}
          <div className="flex gap-2 shrink-0">
            <button
              onClick={onClose}
              className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-700"
            >
              {t("common.cancel")}
            </button>
            {step === "add" && (
              <button
                onClick={handleAdd}
                // Do NOT disable on a missing API key: a greyed-out button
                // with no feedback made users believe selecting a provider
                // was enough (model never saved). handleAdd already guards
                // the key and surfaces `pleaseEnterApiKey` inline — keep
                // the button enabled so that hint can fire.
                disabled={testing}
                className="flex items-center gap-2 rounded btn-accent px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
              >
                {testing ? t("harness.saving") : t("harness.save")}
              </button>
            )}
            {step === "custom" && (
              <button
                onClick={handleAddCustom}
                disabled={!customProviderName.trim() || !customProviderId.trim() || !customBaseUrl.trim() || customTesting}
                className="flex items-center gap-2 rounded btn-accent px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
              >
                {customTesting ? t("harness.saving") : t("harness.save")}
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
