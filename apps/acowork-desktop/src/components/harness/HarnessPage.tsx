import { useState, useEffect, useCallback, useMemo, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-shell";
import type { VaultKeyEntry, ModelInfo, ModelCapabilitiesInfo, ProviderListEntry, McpServerConfigDef, McpCatalogEntryResponse, McpTransportDef, McpPresetDef } from "../../lib/types";
import { cn } from "../../lib/utils";
import { inputBase } from "../../lib/ui-styles";
import { StyledInput } from "../common/StyledInput";
import { Dropdown } from "../common/Dropdown";
import { ProviderLogo } from "../common/ProviderLogo";
import { isLocalProvider } from "../../lib/providers";
import { fetchProviderModels } from "../../lib/gateway-api";
import { getGatewayUrl } from "../../lib/config";
import { Monitor, MousePointer, Package, Search, Globe, BookOpen, FileText, PenTool, Star, Plus, Minus, Check, Pencil, Cpu, Plug, Database, Braces, Puzzle } from "lucide-react";
import { useMcpStore } from "../../stores/mcpStore";
import { MCP_PRESETS, presetToServerConfig } from "../../lib/mcp-presets";
import { normalizeMcpConfigInput } from "../../lib/mcpConfigNormalizer";
import { EmbeddingModelTab } from "./EmbeddingModelTab";
import { LspTab } from "./LspTab";
import { ModelMultiSelect, defaultMakeCaps } from "./ModelMultiSelect";
import { ProviderPicker } from "./ProviderPicker";
import { AddProviderFlow } from "./AddProviderFlow";
import { GlobalCompactModelCard } from "./GlobalCompactModelCard";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";
import { ErrorBox } from "../common/ErrorBox";
import { ExpandableRow, ListBox, ListRow } from "../common/list";
import { SectionPane } from "../common/SectionPane";
import { OverlayScrollbar } from "../common/OverlayScrollbar";

type HarnessTab = "providers" | "mcp" | "embedding" | "lsp";

export function HarnessPage() {
  const { t } = useTranslation();
  const [activeTab, setActiveTab] = useState<HarnessTab>("providers");

  const tabs: { id: HarnessTab; label: string; icon: ReactNode }[] = [
    { id: "providers", label: t("harness.tabProviders"), icon: <Cpu className="h-3.5 w-3.5" /> },
    { id: "mcp", label: t("harness.tabMcp"), icon: <Plug className="h-3.5 w-3.5" /> },
    { id: "embedding", label: t("harness.tabEmbedding"), icon: <Database className="h-3.5 w-3.5" /> },
    { id: "lsp", label: t("harnessLsp.tabLsp"), icon: <Braces className="h-3.5 w-3.5" /> },
  ];

  return (
    <SectionPane
      title={t("navBar.harness")}
      icon={<Puzzle className="h-3.5 w-3.5" />}
      items={tabs}
      selected={activeTab}
      onSelect={(id) => setActiveTab(id as HarnessTab)}
      storageKey="acowork-harness-list-width"
    >
      {/* Tab content — CSS visibility preserves component state across tab switches */}
      <div style={{ display: activeTab === "providers" ? "block" : "none" }}><ProvidersTab /></div>
      <div style={{ display: activeTab === "mcp" ? "block" : "none" }}><McpTab /></div>
      <div style={{ display: activeTab === "embedding" ? "block" : "none" }}><EmbeddingModelTab /></div>
      <div style={{ display: activeTab === "lsp" ? "block" : "none" }}><LspTab /></div>
    </SectionPane>
  );
}

/** Provider configuration */
function ProvidersTab() {
  const { t } = useTranslation();
  const [keys, setKeys] = useState<VaultKeyEntry[]>([]);
  const [keysLoading, setKeysLoading] = useState(true);
  const [showEditDialog, setShowEditDialog] = useState<string | null>(null);

  // AddProviderFlow state
  const [showAddFlow, setShowAddFlow] = useState(false);
  const [addFlowProvider, setAddFlowProvider] = useState<string | undefined>(undefined);
  const [addFlowEntry, setAddFlowEntry] = useState<ProviderListEntry | undefined>(undefined);

  // Edit dialog state
  /** Existing accounts for the provider being edited. `originalAlias`
 *  is snapshotted at dialog open so we can detect a change on save
 *  without re-reading from `keys` (which may shift if other code
 *  refetches). `key` stays empty unless the user wants to rotate;
 *  a non-empty value at save time triggers `update_account_key`.
 *  Entries removed from this list dispatch `remove_key` on save. */
  const [editExistingEntries, setEditExistingEntries] = useState<
    { accountId: string; originalAlias: string; alias: string; key: string }[]
  >([]);
  /** accountIds snapshotted at dialog open. Any id present here but
 *  missing from `editExistingEntries` at save time was removed by
 *  the user via the inline minus button and needs a DELETE call. */
  const [editOriginalAccountIds, setEditOriginalAccountIds] = useState<string[]>([]);
  /** New accounts to add when saving the edit dialog. Each row is
 *  `{alias, key}`; the first row always exists so the UI has at
 *  least one editable input. `ponytail: hard ceiling for UX, lift if needed. */
  const [editKeyEntries, setEditKeyEntries] = useState<
    { alias: string; key: string }[]
  >([{ alias: "", key: "" }]);
  const EDIT_MAX_KEY_ENTRIES = 256;
  const [editBaseUrl, setEditBaseUrl] = useState("");
  const [editModels, setEditModels] = useState<string[]>([]);
  const [editAvailableModels, setEditAvailableModels] = useState<ModelInfo[]>([]);
  const [editModelsLoading, setEditModelsLoading] = useState(false);

  // Edit dialog — per-model capabilities state
  const [editModelCaps, setEditModelCaps] = useState<Record<string, ModelCapabilitiesInfo>>({});
  const [editExpandedModels, setEditExpandedModels] = useState<Set<string>>(new Set());
  const [editCompactModel, setEditCompactModel] = useState("");

  // Tools-tab style level-1 collapsible groups on this tab (default open).
  const [configuredOpen, setConfiguredOpen] = useState(true);

  // Gateway config for default provider indication
  const [config, setConfig] = useState<GatewayConfig | null>(null);

  // Dynamic provider list from Gateway API
  const [dynamicProviders, setDynamicProviders] = useState<ProviderListEntry[]>([]);


  const fetchKeys = useCallback(async () => {
    try {
      const result = await invoke<VaultKeyEntry[]>("list_keys");
      setKeys(result);
    } catch {
      // Gateway may not be running
    } finally {
      setKeysLoading(false);
    }
  }, []);

  const fetchConfig = useCallback(async () => {
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/config`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const result = await resp.json() as GatewayConfig;
      setConfig(result);
    } catch {
      // Gateway may not be running
    }
  }, []);

  // Load provider list from Gateway API (offline_providers.json is the sole data source)
  const loadProviders = useCallback(async () => {
    try {
      const response = await fetch(`${getGatewayUrl()}/api/models`);
      if (response.ok) {
        const data = await response.json();
        setDynamicProviders(data.providers ?? []);
      }
    } catch {
      // Gateway may not be running
    }
  }, []);

  useEffect(() => {
    fetchKeys();
    fetchConfig();
    loadProviders();
  }, [fetchKeys, fetchConfig, loadProviders]);

  // Fetch available models for a provider from Gateway API
  const fetchModels = useCallback(async (providerId: string): Promise<ModelInfo[]> => {
    try {
      const data = await fetchProviderModels(providerId);
      return data.models ?? [];
    } catch {
      return [];
    }
  }, []);

  // Group configured keys by provider so the list shows one row per
  // provider with an "N keys" badge, not N near-duplicate rows. Order
  // follows the first appearance of each provider in `keys`.
  const keysByProvider = useMemo(() => {
    const order: string[] = [];
    const map = new Map<string, VaultKeyEntry[]>();
    for (const k of keys) {
      const list = map.get(k.provider);
      if (list) list.push(k);
      else {
        map.set(k.provider, [k]);
        order.push(k.provider);
      }
    }
    return order.map((provider) => ({
      provider,
      accounts: map.get(provider) ?? [],
    }));
  }, [keys]);

  // Remove the entire provider entry — drops every key in one call.
  // Per-account edits/removes still happen inside the edit dialog.
  const handleRemoveProvider = async (provider: string, count: number) => {
    if (!confirm(t("harness.removeProviderConfirm", { provider, count }))) return;
    try {
      await invoke("remove_key", { provider, accountId: undefined });
      await fetchKeys();
      await loadProviders();
    } catch (e) {
      alert(`${t("harness.failedRemoveKey")}: ${e}`);
    }
  };

  // Set a configured provider as the default for the Gateway
  const handleSetDefaultProvider = async (provider: string) => {
    try {
      const entry = keys.find((k) => k.provider === provider);
      await fetch(`${getGatewayUrl()}/api/config`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          default_provider: provider,
          default_model: entry?.models?.[0] || entry?.default_model || undefined,
        }),
      });
      await fetchConfig();
    } catch (e) {
      alert(`${t("harness.failedSetDefault")}: ${e}`);
    }
  };

  const handleEdit = async (provider: string) => {
    const keyEntry = keys.find((k) => k.provider === provider);
    const dynamicProvider = dynamicProviders.find((p) => p.id === provider);
    // Snapshot every existing account for this provider into the dialog
    // state. The user can rename, rotate keys, or remove any of these
    // inline. Originals are kept for change detection on save.
    const providerAccounts = keys.filter(
      (k) => k.provider === provider && k.account_id,
    );
    setEditExistingEntries(
      providerAccounts.map((k) => ({
        accountId: k.account_id as string,
        originalAlias: k.alias || "",
        alias: k.alias || "",
        key: "",
      })),
    );
    setEditOriginalAccountIds(providerAccounts.map((k) => k.account_id as string));
    setEditKeyEntries([{ alias: "", key: "" }]);
    setEditBaseUrl(keyEntry?.base_url ?? dynamicProvider?.api ?? "");
    const configuredModels = keyEntry?.models?.length ? keyEntry.models : keyEntry?.default_model ? [keyEntry.default_model] : [];
    setEditModels(configuredModels);
    setEditCompactModel(keyEntry?.compact_model ?? "");
    setEditModelCaps({});
    setEditExpandedModels(new Set());
    setShowEditDialog(provider);
    // Fetch models from Gateway API (includes input_modalities, context_window, etc.)
    setEditModelsLoading(true);
    const models = await fetchModels(provider);
    setEditAvailableModels(models);
    setEditModelsLoading(false);
    // Initialize per-model caps: prefer stored caps from vault (preserves
    // default_reasoning_effort, supports_reasoning, etc.), fall back to
    // live model data or sensible defaults.
    const storedCaps = keyEntry?.model_capabilities ?? {};
    const caps: Record<string, ModelCapabilitiesInfo> = {};
    for (const modelId of configuredModels) {
      const mi = models.find(m => m.id === modelId);
      const stored = storedCaps[modelId];
      caps[modelId] = stored
        ? { ...defaultMakeCaps(mi), ...stored }
        : defaultMakeCaps(mi);
    }
    setEditModelCaps(caps);
  };

  const handleEditSave = async () => {
    if (!showEditDialog) return;
    try {
      const provider = showEditDialog;

      // 1. Removed accounts: any accountId in the original snapshot that's
      //    no longer in `editExistingEntries` was dropped by the user
      //    via the inline minus button. Dispatch DELETE for each.
      const survivingIds = new Set(editExistingEntries.map((e) => e.accountId));
      for (const accountId of editOriginalAccountIds) {
        if (!survivingIds.has(accountId)) {
          await invoke("remove_key", { provider, accountId });
        }
      }

      // 2. Per-account changes: each surviving entry's alias is
      //    compared to its original; if the user supplied a non-empty
      //    key we treat that as a key rotation. Both go through PATCH
      //    on a single round-trip per changed account.
      for (const entry of editExistingEntries) {
        const aliasChanged = entry.alias.trim() !== entry.originalAlias;
        const keyChanged = entry.key.trim().length > 0;
        if (!aliasChanged && !keyChanged) continue;
        await invoke("update_account_key", {
          provider,
          accountId: entry.accountId,
          alias: aliasChanged ? entry.alias.trim() : null,
          key: keyChanged ? entry.key.trim() : null,
        });
      }

      // 3. New accounts: rows the user appended but doesn't have an
      //    accountId. Empty rows are dropped server-side too, so we
      //    filter here for clarity.
      const newKeys = editKeyEntries
        .map((e) => ({ alias: e.alias.trim(), key: e.key.trim() }))
        .filter((e) => e.key.length > 0);

      const keyEntry = keys.find((k) => k.provider === provider);
      const isLocal = isLocalProvider(provider);
      const isCustom = keyEntry?.custom ?? false;

      // 4. Provider-level config (base_url / models / compact_model /
      //    capabilities) + appending new accounts all land in the same
      //    `update_key` invoke. The Gateway handler applies them
      //    atomically against the in-memory state before releasing
      //    the lock. Always invoked: the Gateway treats undefined
      //    fields as "preserve existing", so a no-op save is cheap and
      //    keeps the call path uniform.
      const updatePayload: Record<string, unknown> = {
        provider,
        keys: newKeys,
        baseUrl: editBaseUrl || undefined,
        defaultModel: undefined,
        models: editModels.length > 0 ? editModels : undefined,
      };
      if ((isLocal || isCustom) && editModels.length > 0 && Object.keys(editModelCaps).length > 0) {
        updatePayload.modelCapabilities = editModelCaps;
      }
      // Include compact_model if set; explicitly null otherwise so the
      // Gateway clears the field when the user empties it.
      if (editCompactModel) {
        updatePayload.compactModel = editCompactModel;
      } else {
        updatePayload.compactModel = null;
      }
      await invoke("update_key", updatePayload);

      setShowEditDialog(null);
      await fetchKeys();
      await fetchConfig();
      window.dispatchEvent(new CustomEvent('models-added'));
    } catch (e) {
      alert(`${t("harness.failedUpdateKey")}: ${e}`);
    }
  };

  return (
    <div className="max-w-2xl space-y-4">
      {/* ADR-056: Global default compact model — lives at the top of the Providers Tab. */}
      <GlobalCompactModelCard
        keys={keys}
        providers={dynamicProviders}
      />

      {/* Configured Providers — Tools-tab level-1 collapsible card:
          chevron + title + count badge in the header, default open;
          configured keys render as unified rows in the inset body. */}
      {keysLoading ? (
        <div className="py-3 text-center text-xs text-text-tertiary">{t("harness.loadingKeys")}</div>
      ) : keys.length > 0 && (
        <ListBox dividers={false}>
          <ExpandableRow
            open={configuredOpen}
            onToggle={() => setConfiguredOpen((v) => !v)}
            title={t("harness.configuredProviders", { count: keysByProvider.length })}
            ariaLabel={t("harness.configuredProviders", { count: keysByProvider.length })}
            bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
          >
            <ListBox variant="plain">
              {keysByProvider.map(({ provider, accounts }) => {
                const providerMeta = dynamicProviders.find((p) => p.id === provider);
                const providerName = providerMeta?.name || provider;
                const isLocal = accounts[0]?.local || isLocalProvider(provider);
                const isCustom = accounts[0]?.custom || providerMeta?.custom;
                const isDefault = config?.default_provider === provider;
                // Per-provider model list lives on every account entry
                // (same value across accounts); surface the first one.
                const first = accounts[0];

                return (
                  <ListRow
                    key={provider}
                    trailing={
                      <div className="flex shrink-0 items-center gap-1.5">
                        <Tooltip content={isDefault ? t("harness.defaultProvider") : t("harness.setDefaultProvider")} variant="plain">
                          <button
                            type="button"
                            onClick={() => handleSetDefaultProvider(provider)}
                            aria-label={t("harness.setDefaultProvider")}
                            className={cn(
                              "rounded p-0.5",
                              isDefault
                                ? "text-amber-500"
                                : "text-text-tertiary hover:text-amber-500 dark:hover:text-amber-400",
                            )}
                          >
                            <Star className="h-3.5 w-3.5" />
                          </button>
                        </Tooltip>
                        <button
                          type="button"
                          onClick={() => handleEdit(provider)}
                          className="rounded btn-solid px-2 py-0.5 text-xs"
                        >
                          {t("harness.edit")}
                        </button>
                        <button
                          type="button"
                          onClick={() => handleRemoveProvider(provider, accounts.length)}
                          className="rounded btn-solid px-2 py-0.5 text-xs"
                        >
                          {t("harness.remove")}
                        </button>
                      </div>
                    }
                  >
                    <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5">
                      <ProviderLogo providerId={provider} size={12} />
                      <span className="truncate text-xs font-medium text-text-secondary ">{providerName}</span>
                      <span className="text-xs" style={{ color: "var(--color-accent)" }}>{t("harness.active")}</span>
                      {isCustom ? (
                        <Tooltip content={t("harness.customProviderNoKey")} variant="plain">
                          <span className="rounded bg-blue-100 px-1.5 py-0.5 text-10 text-blue-700 dark:bg-blue-900/30 dark:text-blue-400">
                            🔧 {t("harness.custom")}
                          </span>
                        </Tooltip>
                      ) : isLocal ? (
                        <Tooltip content={t("harness.localProviderNoKey")} variant="plain">
                          <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-10 text-text-secondary dark:bg-zinc-700 ">
                            🏠 {t("harness.local")}
                          </span>
                        </Tooltip>
                      ) : (
                        <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-10 text-text-secondary dark:bg-zinc-700">
                          {t("harness.keysCount", { count: accounts.length })}
                        </span>
                      )}
                    </div>
                    <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-0.5">
                      {first?.models?.length ? (
                        <span className="text-11 text-text-tertiary">{first.models.join(", ")}</span>
                      ) : first?.default_model ? (
                        <span className="text-11 text-text-tertiary">{first.default_model}</span>
                      ) : (
                        <span className="text-11 text-text-tertiary">—</span>
                      )}
                      {first?.compact_model && (
                        <Tooltip content={t("harness.compactModelHint")} variant="plain">
                          <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-10 text-text-secondary dark:bg-zinc-700 ">
                            {t("harness.compact")}: {first?.compact_model}
                          </span>
                        </Tooltip>
                      )}
                    </div>
                  </ListRow>
                );
              })}
            </ListBox>
          </ExpandableRow>
        </ListBox>
      )}

      {/* Available Providers — shared ProviderPicker renders three level-1
          collapsible groups (Custom / Local / Remote), each default open.
          No divider lines: boxes are spaced evenly by the root space-y-4. */}
      <ProviderPicker
        providers={dynamicProviders}
        keys={keys}
        onConnect={(providerId, entry) => {
          setAddFlowProvider(providerId);
          setAddFlowEntry(entry);
          setShowAddFlow(true);
        }}
        onAddCustom={() => {
          setAddFlowProvider(undefined);
          setAddFlowEntry(undefined);
          setShowAddFlow(true);
        }}
        onCatalogRefreshed={async () => {
          // The Gateway re-derives the capabilities of already-configured
          // providers on refresh, so the configured list has to be re-read as
          // well — not just the available one.
          await loadProviders();
          await fetchKeys();
        }}
      />

      {/* Add Provider Flow dialog (picker → add / custom) */}
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
        onSuccess={async () => {
          await fetchKeys();
          await fetchConfig();
          await loadProviders();
        }}
      />

      {/* Edit key dialog */}
      {showEditDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay" onClick={() => setShowEditDialog(null)}>
          <div
            className="flex w-[440px] max-h-[85vh] flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-xl"
            onClick={(e) => e.stopPropagation()}
          >
            {/* Header */}
            <div className="flex items-center gap-2 border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
              <Pencil className="h-5 w-5 text-text-tertiary " />
              <h2 className="text-sm font-semibold text-text ">
                {t("harness.editProvider")} {showEditDialog}
              </h2>
            </div>

            {/* Body */}
            <OverlayScrollbar fill="flex" className="space-y-2 px-5 py-4">
              {!isLocalProvider(showEditDialog) && (
                <div>
                  <div className="mb-1 flex items-baseline justify-between">
                    <label className="block text-xs text-text-tertiary">{t("harness.apiKey")}</label>
                    <span className="text-10 text-text-tertiary">
                      {t("harness.accountLabel", { defaultValue: "alias" })}
                    </span>
                  </div>
                  <OverlayScrollbar className="space-y-1.5 pr-1" maxHeight={200}>
                    {/* Existing accounts — editable inline. Remove from
                        this list drops the account on save. Empty key
                        leaves the stored value untouched; non-empty
                        rotates it. */}
                    {editExistingEntries.map((entry, idx) => (
                      <div key={entry.accountId} className="flex items-center gap-1.5">
                        <StyledInput
                          type="text"
                          value={entry.alias}
                          onChange={(e) => {
                            const next = [...editExistingEntries];
                            next[idx] = { ...next[idx], alias: e.target.value };
                            setEditExistingEntries(next);
                          }}
                          placeholder="alias"
                          className="w-[110px] shrink-0"
                        />
                        <StyledInput
                          type="password"
                          value={entry.key}
                          onChange={(e) => {
                            const next = [...editExistingEntries];
                            next[idx] = { ...next[idx], key: e.target.value };
                            setEditExistingEntries(next);
                          }}
                          placeholder={t("harness.leaveBlankToKeep", {
                            defaultValue: "leave blank to keep",
                          })}
                          className="flex-1"
                        />
                        <button
                          type="button"
                          onClick={() => setEditExistingEntries(editExistingEntries.filter((e) => e.accountId !== entry.accountId))}
                          aria-label="Remove account"
                          className="shrink-0 rounded p-1 text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700"
                        >
                          <Minus className="h-3.5 w-3.5" />
                        </button>
                      </div>
                    ))}
                    {/* New accounts — appended at the bottom. Disabled
                        when the only row is already empty (otherwise the
                        UI has no editable surface). */}
                    {editKeyEntries.map((entry, idx) => (
                      <div key={`new-${idx}`} className="flex items-center gap-1.5">
                        <StyledInput
                          type="text"
                          value={entry.alias}
                          onChange={(e) => {
                            const next = [...editKeyEntries];
                            next[idx] = { ...next[idx], alias: e.target.value };
                            setEditKeyEntries(next);
                          }}
                          placeholder="alias"
                          className="w-[110px] shrink-0"
                        />
                        <StyledInput
                          type="password"
                          value={entry.key}
                          onChange={(e) => {
                            const next = [...editKeyEntries];
                            next[idx] = { ...next[idx], key: e.target.value };
                            setEditKeyEntries(next);
                          }}
                          placeholder={t("harness.enterNewApiKey")}
                          className="flex-1"
                        />
                        <button
                          type="button"
                          onClick={() => setEditKeyEntries(editKeyEntries.filter((_, i) => i !== idx))}
                          disabled={editKeyEntries.length <= 1}
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
                    onClick={() => setEditKeyEntries([...editKeyEntries, { alias: "", key: "" }])}
                    disabled={editKeyEntries.length >= EDIT_MAX_KEY_ENTRIES}
                    className="mt-1.5 flex items-center gap-1 text-xs text-text-tertiary hover:text-text-primary disabled:opacity-30 disabled:hover:text-text-tertiary"
                  >
                    <Plus className="h-3.5 w-3.5" />
                    {t("harness.addKey", { defaultValue: "Add key" })}
                  </button>
                </div>
              )}

              {(
                <div>
                  <label className="mb-1 block text-xs text-text-tertiary">{t("harness.baseUrl")}</label>
                  <StyledInput
                    type="text"
                    value={editBaseUrl}
                    onChange={(e) => setEditBaseUrl(e.target.value)}
                    placeholder="https://..."
                    fontMono
                  />
                </div>
              )}

              {/* Model selection (shared multi-select component) */}
              {(() => {
                const editKeyEntry = showEditDialog ? keys.find(k => k.provider === showEditDialog) : undefined;
                const editIsLocal = showEditDialog ? isLocalProvider(showEditDialog) : false;
                const editIsCustom = editKeyEntry?.custom ?? false;
                return (
                  <ModelMultiSelect
                    models={editAvailableModels}
                    loading={editModelsLoading}
                    selected={editModels}
                    onSelectedChange={setEditModels}
                    caps={editModelCaps}
                    onCapsChange={setEditModelCaps}
                    expandedModels={editExpandedModels}
                    onExpandedToggle={(modelId) =>
                      setEditExpandedModels((prev) => {
                        const next = new Set(prev);
                        if (next.has(modelId)) next.delete(modelId);
                        else next.add(modelId);
                        return next;
                      })
                    }
                    showModelCapEditor={editIsLocal || editIsCustom}
                    compactModel={editCompactModel}
                    onCompactModelChange={setEditCompactModel}
                    showCompactModel={true}
                  />
                );
              })()}
            </OverlayScrollbar>

            {/* Footer */}
            <div className="flex items-center justify-end gap-2 border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
              <button
                onClick={() => setShowEditDialog(null)}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-700"
              >
                {t("common.cancel")}
              </button>
              <button
                onClick={handleEditSave}
                className="rounded btn-accent px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
              >
                {t("harness.save")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

/** MCP catalog tab. Exported for tests. */
const MCP_ICON_MAP: Record<string, React.ComponentType<{ className?: string }>> = {
  Monitor, MousePointer, Search, Globe, BookOpen, FileText, PenTool,
};
const MCP_FALLBACK_ICON = Package;

/** Public MCP server directory surfaced in the add-server dialog. Opened
 *  through plugin-shell `open` (already registered + `shell:allow-open`
 *  granted) rather than a plain `<a target="_blank">`: a bare anchor click
 *  routes through the webview's new-window path, which is the one under
 *  suspicion for the link-click webview crash (see the temporary probe in
 *  main.tsx). The URL itself is a constant — only the label is translated. */
const MCP_DIR_URL = "https://mcpdir.dev";

// Parse the free-text `KEY=VALUE, ...` (env, sep "=") and `Name: value, ...`
// (headers, sep ":") fields into a map. Splits on the FIRST separator only so
// a value may itself contain the separator (Windows paths, `Bearer` tokens,
// URLs); empty input yields `{}` so the wire field stays an empty object.
function parsePairs(text: string, sep: string): Record<string, string> {
  const trimmed = text.trim();
  if (!trimmed) return {};
  const out: Record<string, string> = {};
  for (const pair of trimmed.split(",")) {
    const idx = pair.indexOf(sep);
    if (idx === -1) continue;
    const k = pair.slice(0, idx).trim();
    const v = pair.slice(idx + sep.length).trim();
    if (k) out[k] = v;
  }
  return out;
}

// Inverse of parsePairs — re-serialize a map back into the text field shape
// (`sep` is inserted verbatim, so pass ": " for headers for readability).
function pairsToText(obj: Record<string, string>, sep: string): string {
  return Object.entries(obj).map(([k, v]) => `${k}${sep}${v}`).join(", ");
}

export function McpTab() {
  const { t } = useTranslation();
  const { catalog, loading, error, loadCatalog, addServer, updateServer, removeServer } = useMcpStore();
  const [showAddForm, setShowAddForm] = useState(false);
  // When set, the shared form dialog is in edit mode for this catalog
  // entry (name is identity — agents reference servers by name, so the
  // name field is locked and the PUT is keyed by this original name).
  const [editingName, setEditingName] = useState<string | null>(null);
  // Tools-tab style level-1 collapsible groups (default open)
  const [listOpen, setListOpen] = useState(true);

  // New/edit server form state
  const [newName, setNewName] = useState("");
  const [newDisplayName, setNewDisplayName] = useState("");
  const [newTransport, setNewTransport] = useState<McpTransportDef>("stdio");
  const [newCommand, setNewCommand] = useState("");
  const [newArgs, setNewArgs] = useState("");
  const [newUrl, setNewUrl] = useState("");
  const [newEnv, setNewEnv] = useState("");
  const [newHeaders, setNewHeaders] = useState("");

  // Paste-to-normalize state: a vendor `mcpServers` JSON block or a CLI
  // `mcp add` line, parsed into the form fields below (never submitted
  // directly — the user always reviews the autofilled fields first).
  const [showPaste, setShowPaste] = useState(false);
  const [pasteText, setPasteText] = useState("");
  const [parseMsg, setParseMsg] = useState<{ kind: "error" | "warn" | "ok"; text: string }[]>([]);

  // Preset env var form (for servers requiring API keys)
  const [presetEnvForm, setPresetEnvForm] = useState<Record<string, string>>({});
  const [activePreset, setActivePreset] = useState<McpPresetDef | null>(null);

  useEffect(() => {
    loadCatalog();
  }, [loadCatalog]);

  const catalogNames = useMemo(() => new Set(catalog.map((s) => s.name)), [catalog]);
  // ADR-090: the settings page is ONE list with two row kinds.
  //  - Preset rows are permanent built-ins: "Add" until they are registered
  //    in the Gateway catalog, then a neutral "Added" label. They are never
  //    removable — the registry copy of a built-in template is not the
  //    user's to delete.
  //  - Custom rows are catalog entries that match no preset; these carry
  //    Remove. Runtime verification (Test) lives on the Node machine — the
  //    Tools panel's enable switch drives ensure — so no row here probes.
  const presetIds = useMemo(() => new Set(MCP_PRESETS.map((p) => p.id)), []);
  const customServers = useMemo(
    () => catalog.filter((c) => !presetIds.has(c.name)),
    [catalog, presetIds],
  );

  // ADR-090: "add" is a pure registry write. No probe-then-add — a Gateway-side
  // probe would spawn the server on the Gateway machine, which says nothing
  // about the Node where an Agent will actually run it. Usability is verified
  // by the Agent when the user enables the server in the Tools panel.
  const handleAddFromPreset = (preset: McpPresetDef) => {
    if (preset.requiredEnv.length > 0) {
      // Show env form for API keys
      setActivePreset(preset);
      setPresetEnvForm(
        preset.requiredEnv.reduce((acc, key) => ({ ...acc, [key]: "" }), {})
      );
      return;
    }
    addServer(presetToServerConfig(preset));
  };

  const handlePresetEnvSubmit = () => {
    if (!activePreset) return;
    const env = { ...activePreset.optionalEnv, ...presetEnvForm };
    const config = presetToServerConfig(activePreset, env);
    setActivePreset(null);
    setPresetEnvForm({});
    addServer(config);
  };

  const handleAddManual = () => {
    if (!newName.trim()) return;
    const config: McpServerConfigDef = buildFormConfig(newName.trim());
    closeForm();
    if (editingName) {
      updateServer(editingName, config);
    } else {
      addServer(config);
    }
  };

  // Serialize the shared form state into a catalog config (args split on
  // whitespace, env parsed from the `KEY=VALUE, ...` text field, headers from
  // the `Name: value, ...` field — a header value may itself contain colons,
  // so only the first separates name from value).
  const buildFormConfig = (name: string): McpServerConfigDef => ({
    name,
    display_name: newDisplayName.trim() || undefined,
    transport: newTransport,
    command: newCommand.trim(),
    args: newArgs.trim() ? newArgs.trim().split(/\s+/) : [],
    url: newUrl.trim() || undefined,
    env: parsePairs(newEnv, "="),
    headers: parsePairs(newHeaders, ":"),
  });

  // Run the normalizer over the pasted vendor config / CLI line and autofill
  // the form fields below with the result. The parsed config is NEVER
  // submitted straight — the user reviews the fields, then hits Save (which
  // re-serializes the form, so manual edits after a parse are honored).
  const handleParse = () => {
    const r = normalizeMcpConfigInput(pasteText, newName.trim());
    if (r.errors.length > 0) {
      setParseMsg(r.errors.map((e) => ({ kind: "error", text: e.message })));
      return;
    }
    const s = r.servers[0];
    setNewTransport(s.transport);
    setNewCommand(s.command ?? "");
    setNewArgs((s.args ?? []).join(" "));
    setNewUrl(s.url ?? "");
    setNewEnv(pairsToText(s.env ?? {}, "="));
    setNewHeaders(pairsToText(s.headers ?? {}, ": "));
    // Name is the identity key: in add mode adopt the parsed name; in edit
    // mode the field is locked, so leave it and just note a mismatch.
    const msgs: { kind: "error" | "warn" | "ok"; text: string }[] = [];
    if (!editingName) {
      if (s.name) setNewName(s.name);
    } else if (s.name && s.name !== editingName) {
      msgs.push({ kind: "warn", text: `Parsed name "${s.name}" ignored — editing keeps the existing key.` });
    }
    if (r.servers.length > 1) {
      msgs.push({ kind: "warn", text: `Parsed ${r.servers.length} servers — filled the first (${s.name}); add the others one at a time.` });
    }
    for (const w of r.warnings) msgs.push({ kind: "warn", text: w });
    if (msgs.length === 0 || msgs.every((m) => m.kind !== "error")) {
      msgs.push({ kind: "ok", text: t("harnessMcp.parsedOk") });
    }
    setParseMsg(msgs);
  };

  // Open the shared dialog in edit mode, prefilled from the catalog row.
  // Sensitive env values arrive masked (`••••`) from GET /mcp-catalog;
  // sending them back verbatim makes the Gateway's update handler keep
  // the stored secret, so untouched keys survive the round-trip.
  const openEdit = (server: McpCatalogEntryResponse) => {
    setEditingName(server.name);
    setNewName(server.name);
    setNewDisplayName(server.display_name ?? "");
    setNewTransport(server.transport);
    setNewCommand(server.command ?? "");
    setNewArgs((server.args ?? []).join(" "));
    setNewUrl(server.url ?? "");
    setNewEnv(
      Object.entries(server.env ?? {}).map(([k, v]) => `${k}=${v}`).join(", "),
    );
    setNewHeaders(pairsToText(server.headers ?? {}, ": "));
    setShowAddForm(true);
  };

  const closeForm = () => {
    setShowAddForm(false);
    setEditingName(null);
    setNewName(""); setNewCommand(""); setNewArgs(""); setNewUrl(""); setNewEnv(""); setNewHeaders("");
    setNewDisplayName("");
    setShowPaste(false); setPasteText(""); setParseMsg([]);
  };

  return (
    <div className="max-w-2xl space-y-4">
      {/* MCP Server Catalog — Tools-tab level-1 collapsible card:
          header carries title + count badge + Add button; server rows
          live in the inset body (default open). */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={listOpen}
          onToggle={() => setListOpen((v) => !v)}
          title={t("harnessMcp.mcpServers", { count: MCP_PRESETS.length + customServers.length })}
          ariaLabel={t("harnessMcp.mcpServers", { count: MCP_PRESETS.length + customServers.length })}
          trailing={
            <span onClick={(e) => e.stopPropagation()}>
              <button
                type="button"
                onClick={() => setShowAddForm(true)}
                className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium"
              >
              {t("harnessMcp.addServer")}
            </button>
          </span>
        }
        bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
      >
          {error && (
            <div className="px-3 pt-2">
              <ErrorBox message={error} onClose={() => useMcpStore.setState({ error: null })} />
            </div>
          )}

          {loading && catalog.length === 0 && (
            <p className="px-3 py-3 text-xs text-text-tertiary">{t("harnessMcp.loadingCatalog")}</p>
          )}

          {/* ADR-090: one list, two row kinds — preset rows (permanent
              built-ins, Add / Added) then custom registry rows (Remove).
              No Test anywhere: usability is probed on the Node machine by
              the Agent at first enable, not by the Gateway. */}
          <ListBox variant="plain">
            {MCP_PRESETS.map((preset) => {
              const isAdded = catalogNames.has(preset.id);
              return (
                <ListRow
                  key={preset.id}
                  testId={`mcp-preset-${preset.id}`}
                  surface="inset"
                  leading={
                    (() => {
                      const Icon = MCP_ICON_MAP[preset.icon ?? ""] || MCP_FALLBACK_ICON;
                      return (
                        <Icon className="h-4 w-4 shrink-0 text-text-tertiary " />
                      );
                    })()
                  }
                  trailing={
                    isAdded ? (
                      <span
                        className="inline-flex shrink-0 items-center gap-1 text-11 text-text-tertiary"
                        data-testid={`mcp-preset-added-${preset.id}`}
                      >
                        <Check className="h-3.5 w-3.5" />
                        {t("harnessMcp.added")}
                      </span>
                    ) : (
                      <button
                        type="button"
                        onClick={() => handleAddFromPreset(preset)}
                        className="inline-flex shrink-0 items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium"
                      >
                        <Plus className="h-3 w-3" />
                        {t("harnessMcp.add")}
                      </button>
                    )
                  }
                >
                  <div className="flex flex-wrap items-center gap-x-1.5 gap-y-0.5">
                    <span className="truncate text-xs font-medium text-text-secondary ">{preset.name}</span>
                    <span className="shrink-0 rounded bg-zinc-100 px-1 py-0.5 text-10 text-text-tertiary dark:bg-zinc-700">
                      {preset.category}
                    </span>
                  </div>
                  <p className="mt-0.5 line-clamp-1 text-10 text-text-tertiary">{preset.description}</p>
                  {preset.requiredEnv.length > 0 && (
                    <p className="mt-0.5 text-10 text-amber-500">
                      {t("harnessMcp.requires")}{preset.requiredEnv.join(", ")}
                    </p>
                  )}
                </ListRow>
              );
            })}
            {customServers.map((server) => (
              <ListRow
                key={server.name}
                testId={`mcp-custom-${server.name}`}
                surface="inset"
                leading={
                  <Package className="h-4 w-4 shrink-0 text-text-tertiary " />
                }
                trailing={
                  <span className="inline-flex shrink-0 items-center gap-1">
                    <button
                      type="button"
                      onClick={() => openEdit(server)}
                      data-testid={`mcp-edit-${server.name}`}
                      className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium"
                    >
                      <Pencil className="h-3 w-3" />
                      {t("harnessMcp.edit")}
                    </button>
                    <button
                      type="button"
                      onClick={() => removeServer(server.name)}
                      className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium"
                    >
                      {t("harnessMcp.remove")}
                    </button>
                  </span>
                }
              >
                <div className="flex flex-wrap items-center gap-x-1.5 gap-y-0.5">
                  <span className="truncate text-xs font-medium text-text-secondary ">{server.display_name?.trim() || server.name}</span>
                  <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-10 font-mono text-text-tertiary dark:bg-zinc-700">
                    {server.transport}
                  </span>
                  {server.has_secrets && (
                    <span className="text-10 text-amber-500 shrink-0">{t("harnessMcp.hasApiKey")}</span>
                  )}
                </div>
                {/* When a friendly alias is set, still surface the immutable
                    identity key agents actually reference. */}
                {server.display_name?.trim() && (
                  <p className="mt-0.5 truncate text-10 text-text-tertiary font-mono break-all">{server.name}</p>
                )}
                {(server.command || server.url) && (
                  <p className="mt-0.5 truncate text-10 text-text-tertiary break-all">
                    {server.command || server.url}
                  </p>
                )}
              </ListRow>
            ))}
          </ListBox>
        </ExpandableRow>
      </ListBox>

      {/* Add / edit server dialog — one form, two modes (editingName set
          means edit). Name is the agent-side reference key, so it is
          locked in edit mode; everything else is freely editable. */}
      {showAddForm && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay">
          <OverlayScrollbar
            maxHeight="85vh"
            className="p-6"
            containerClassName="w-[440px] overflow-hidden rounded-md bg-modal-surface shadow-xl"
          >
            <h3 className="mb-1 text-sm font-semibold">
              {editingName ? t("harnessMcp.editCustomMcpServer") : t("harnessMcp.addCustomMcpServer")}
            </h3>
            {/* Where to find servers to add — the catalog only ships the
                built-in presets, so without this the dialog looks like the
                only place a server can come from. */}
            <p className="mb-3 text-10 text-text-tertiary">
              {t("harnessMcp.findServers")}{" "}
              <button
                type="button"
                onClick={() => { void open(MCP_DIR_URL).catch(() => {}); }}
                data-testid="mcp-find-servers-link"
                className="text-[var(--color-accent)] hover:underline"
              >
                {MCP_DIR_URL}
              </button>
            </p>
            <div className="space-y-2">
              {/* Paste-to-normalize: drop in a vendor `mcpServers` JSON block
                  or a CLI `mcp add` line and it fills the fields below. It
                  never submits — you always review the autofill first. */}
              <button
                type="button"
                onClick={() => setShowPaste((v) => !v)}
                className="text-xs font-medium text-[var(--color-accent)] hover:underline"
              >
                {showPaste ? t("harnessMcp.hidePaste") : t("harnessMcp.pastePlaceholder")}
              </button>
              {showPaste && (
                <div className="space-y-2 rounded-md border border-border-divider bg-panel-inset p-2">
                  <textarea
                    value={pasteText}
                    onChange={(e) => setPasteText(e.target.value)}
                    rows={4}
                    className={cn(inputBase, "font-mono text-10")}
                    placeholder={'{ "mcpServers": { ... } }  \n  or  \n  npx -y @modelcontextprotocol/server-filesystem ...'}
                  />
                  <button
                    type="button"
                    onClick={handleParse}
                    disabled={!pasteText.trim()}
                    className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium disabled:opacity-50"
                  >
                    {t("harnessMcp.parse")}
                  </button>
                  {parseMsg.length > 0 && (
                    <ul className="space-y-1">
                      {parseMsg.map((m, i) => (
                        <li
                          key={i}
                          className={cn(
                            "text-10",
                            m.kind === "error" && "text-red-500",
                            m.kind === "warn" && "text-amber-500",
                            m.kind === "ok" && "text-green-600",
                          )}
                        >
                          {m.text}
                        </li>
                      ))}
                    </ul>
                  )}
                </div>
              )}
              <div>
                <label className="mb-1 block text-xs text-text-tertiary">{t("harnessMcp.name")}</label>
                <input
                  value={newName}
                  onChange={(e) => setNewName(e.target.value)}
                  className={cn(inputBase, editingName && "cursor-not-allowed opacity-60")}
                  placeholder="my-server"
                  readOnly={!!editingName}
                />
                {editingName && (
                  <p className="mt-1 text-10 text-text-tertiary">{t("harnessMcp.nameIdentityHint")}</p>
                )}
              </div>
              <div>
                <label className="mb-1 block text-xs text-text-tertiary">{t("harnessMcp.displayName")}</label>
                <input
                  value={newDisplayName}
                  onChange={(e) => setNewDisplayName(e.target.value)}
                  className={inputBase}
                  placeholder={newName || t("harnessMcp.displayNamePlaceholder")}
                />
                <p className="mt-1 text-10 text-text-tertiary">{t("harnessMcp.displayNameHint")}</p>
              </div>
              <div>
                <label className="mb-1 block text-xs text-text-tertiary">{t("harnessMcp.transport")}</label>
                <Dropdown
                  value={newTransport}
                  onChange={(v) => setNewTransport(v as McpTransportDef)}
                  options={[
                    { value: "stdio", label: "stdio" },
                    { value: "http", label: "http" },
                    { value: "sse", label: "sse" },
                  ]}
                />
              </div>
              {newTransport === "stdio" ? (
                <>
                  <div>
                    <label className="mb-1 block text-xs text-text-tertiary">{t("harnessMcp.command")}</label>
                    <input
                      value={newCommand}
                      onChange={(e) => setNewCommand(e.target.value)}
                      className={inputBase}
                      placeholder="npx"
                    />
                  </div>
                  <div>
                    <label className="mb-1 block text-xs text-text-tertiary">{t("harnessMcp.arguments")}</label>
                    <input
                      value={newArgs}
                      onChange={(e) => setNewArgs(e.target.value)}
                      className={inputBase}
                      placeholder="-y @modelcontextprotocol/server-filesystem"
                    />
                  </div>
                </>
              ) : (
                <>
                  <div>
                    <label className="mb-1 block text-xs text-text-tertiary">{t("harnessMcp.url")}</label>
                    <input
                      value={newUrl}
                      onChange={(e) => setNewUrl(e.target.value)}
                      className={inputBase}
                      placeholder="http://localhost:3000"
                    />
                  </div>
                  {/* http/sse auth travels in request headers (env is only
                      injected into stdio subprocesses) — e.g. a Bearer token
                      for a hosted dashscope MCP. */}
                  <div>
                    <label className="mb-1 block text-xs text-text-tertiary">{t("harnessMcp.headers")}</label>
                    <input
                      value={newHeaders}
                      onChange={(e) => setNewHeaders(e.target.value)}
                      className={inputBase}
                      placeholder="Authorization: Bearer sk-xxx"
                    />
                    {editingName && (
                      <p className="mt-1 text-10 text-text-tertiary">{t("harnessMcp.envMaskedHint")}</p>
                    )}
                  </div>
                </>
              )}
              <div>
                <label className="mb-1 block text-xs text-text-tertiary">{t("harnessMcp.environment")}</label>
                <input
                  value={newEnv}
                  onChange={(e) => setNewEnv(e.target.value)}
                  className={inputBase}
                  placeholder="API_KEY=sk-xxx, DEBUG=true"
                />
                {editingName && (
                  <p className="mt-1 text-10 text-text-tertiary">{t("harnessMcp.envMaskedHint")}</p>
                )}
              </div>
            </div>
            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={closeForm}
                className="inline-flex items-center gap-1 rounded-md border border-zinc-300 px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-50 dark:border-zinc-600  dark:hover:bg-zinc-700"
              >
                {t("common.cancel")}
              </button>
              <button
                onClick={handleAddManual}
                disabled={!newName.trim()}
                className="inline-flex items-center gap-1 rounded btn-accent px-3 py-1.5 text-xs font-medium disabled:opacity-50"
              >
                {editingName ? t("harnessMcp.save") : t("harnessMcp.addServer")}
              </button>
            </div>
          </OverlayScrollbar>
        </div>
      )}

      {/* Preset env form (for servers requiring API keys) */}
      {activePreset && (
        <div className="rounded-md border border-[var(--color-accent)]/40 bg-modal-surface p-4">
          <h2 className="text-xs font-medium mb-1">{t("harnessMcp.configure")}{activePreset.name}</h2>
          <p className="text-10 text-text-tertiary mb-3">{activePreset.installHint}</p>
          <div className="space-y-2">
            {activePreset.requiredEnv.map((envKey) => (
              <div key={envKey}>
                <label className="mb-1 block text-10 text-text-tertiary">{envKey}</label>
                <input
                  type="password"
                  value={presetEnvForm[envKey] || ""}
                  onChange={(e) =>
                    setPresetEnvForm((prev) => ({ ...prev, [envKey]: e.target.value }))
                  }
                  className={inputBase}
                  placeholder={`${t("harnessMcp.enter")}${envKey}`}
                />
              </div>
            ))}
            <div className="flex gap-2">
              <button
                onClick={handlePresetEnvSubmit}
                className="inline-flex items-center gap-1 rounded btn-accent px-3 py-1 text-xs font-medium"
              >
                {t("harnessMcp.addServer")}
              </button>
              <button
                onClick={() => { setActivePreset(null); setPresetEnvForm({}); }}
                className="inline-flex items-center gap-1 rounded-md border border-zinc-300 px-3 py-1 text-xs font-medium text-text-secondary hover:bg-zinc-50 dark:border-zinc-600  dark:hover:bg-zinc-700"
              >
                {t("common.cancel")}
              </button>
            </div>
          </div>
        </div>
      )}

    </div>
  );
}

/** GatewayConfig type for local usage */
interface GatewayConfig {
  default_provider?: string;
  default_model?: string;
}