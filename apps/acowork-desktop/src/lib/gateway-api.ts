//! Gateway HTTP API client for models.dev integration

import type {
  ProviderModelsResponse,
  ProviderListEntry,
  ModelInfo,
  BackendUserProfile,
  UserProfileListResponse,
  UserProfileMutationResponse,
  ActivateUserResponse,
  CreateUserRequest,
  UpdateUserRequest,
  OperationAck,
  EmbeddingModelsResponse,
  EmbeddingModelActionResponse,
  EmbeddingModelStatusResponse,
  EmbeddingTestResponse,
  CloudEmbeddingTestResponse,
  MigrationProgressResponse,
  RebuildScope,
  SelectModelMigrationResponse,
  CloudEmbeddingProvidersResponse,
  SelectCloudEmbeddingResponse,
  AddCloudEmbeddingProviderRequest,
  UpdateCloudEmbeddingProviderRequest,
  CloudEmbeddingProviderResponse,
  AgentLspEndpointResponse,
  LspInstallRunResponse,
  LspInstallScriptResponse,
  LspInstallStatusResponse,
  LspMutationResponse,
  LspRegistryView,
  LspServerEntry,
  LspServerStatusEntry,
  LspServersConfig,
  LspServersWithStatus,
  CompactModelRef,
  DefaultCompactModelResponse,
  NodeInfo,
} from "./types";
import { getGatewayUrl } from "./config";
import { httpApiError } from "./api-error";

// ── LSP Relay endpoint cache ───────────────────────────────────────────
//
// ADR-055 §6.7 (Phase 4): the relay is a node-local sidecar, so its
// endpoint is resolved PER AGENT via `GET /api/agents/{id}/lsp-endpoint`
// (the node hosting the agent runs the relay). Results are cached per
// agent. On error or invalidation, the cache entry is cleared so the
// next call re-fetches.

const relayEndpointCache = new Map<string, Promise<string | null>>();

/**
 * Get the cached LSP Relay base URL for an agent, fetching from Gateway
 * if needed.
 *
 * ADR-089 D3: the usable URL is assembled from the caller's own
 * gateway URL + the hosting `node_id` (`{gateway}/api/nodes/{id}/lsp`,
 * reverse-proxied to the node's loopback relay). The endpoint's raw
 * `endpoint` field is NOT used as a URL: it carries the advertise-host
 * address, which is wrong for relay-mode tunnels and needlessly
 * fragile for LAN clients — the gateway URL the app is actually
 * talking to is the only reachable base the client truly knows.
 *
 * Returns `null` when the relay is not available (the hosting node is
 * offline or has not published a ready LSP relay state; `reason`
 * carries the code). On fetch error, the cache entry is cleared so the
 * next call retries.
 */
export async function getCachedLspRelayEndpoint(
  agentId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<string | null> {
  let cached = relayEndpointCache.get(agentId);
  if (!cached) {
    cached = fetchAgentLspEndpoint(agentId, gatewayUrl)
      .then((ep) =>
        ep.ready && ep.node_id
          ? `${gatewayUrl.replace(/\/+$/, "")}/api/nodes/${encodeURIComponent(ep.node_id)}/lsp`
          : null,
      )
      .catch((err) => {
        relayEndpointCache.delete(agentId); // Clear cache on error
        throw err;
      });
    relayEndpointCache.set(agentId, cached);
  }
  return cached;
}

/**
 * Invalidate the cached LSP Relay endpoint for an agent, or for every
 * agent when called without an argument (e.g. after connection failure).
 */
export function invalidateLspRelayEndpointCache(agentId?: string): void {
  if (agentId !== undefined) {
    relayEndpointCache.delete(agentId);
  } else {
    relayEndpointCache.clear();
  }
}

/** Fetch all providers from Gateway's models cache */
export async function fetchProviders(
  gatewayUrl = getGatewayUrl(),
): Promise<ProviderListEntry[]> {
  const resp = await fetch(`${gatewayUrl}/api/models`);
  if (!resp.ok) throw new Error(`Failed to fetch providers: ${resp.status}`);
  const data = await resp.json();
  return data.providers as ProviderListEntry[];
}

/**
 * HTTP health double-check for distributed liveness.
 *
 * The Runtime may run on a remote node, so process-based liveness is not
 * observable from the desktop. The only trustworthy signals are network
 * based: MQTT (status topic) and HTTP (the Runtime's `/health` endpoint,
 * reverse-proxied through the Gateway at `/api/agents/{id}/health`).
 *
 * When MQTT reports an agent as offline/sleeping — e.g. the machine was
 * asleep and the MQTT connection dropped (KeepAlive timeout) while the
 * Runtime process itself stayed alive — this probes `/health`. A 2xx
 * answer means the Runtime is actually alive, and the caller should NOT
 * render it as offline/sleeping.
 *
 * Returns `true` iff the Runtime answers 2xx within `timeoutMs`; `false`
 * on any non-2xx or network error (unreachable over HTTP too).
 */
export async function verifyAgentHealth(
  agentId: string,
  timeoutMs = 3000,
  gatewayUrl = getGatewayUrl(),
): Promise<boolean> {
  try {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), timeoutMs);
    try {
      const resp = await fetch(`${gatewayUrl}/api/agents/${agentId}/health`, {
        signal: ctrl.signal,
      });
      return resp.ok;
    } finally {
      clearTimeout(timer);
    }
  } catch {
    return false;
  }
}

/** Fetch models for a specific provider from Gateway's models cache */
export async function fetchProviderModels(
  providerId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<ProviderModelsResponse> {
  const resp = await fetch(`${gatewayUrl}/api/models/${providerId}`);
  if (!resp.ok)
    throw new Error(`Failed to fetch models for ${providerId}: ${resp.status}`);
  return resp.json();
}

/** Discover models from a custom provider's base URL (OpenAI-compatible /v1/models) */
export async function discoverModels(
  baseUrl: string,
  apiKey?: string,
  gatewayUrl = getGatewayUrl(),
): Promise<ModelInfo[]> {
  const resp = await fetch(`${gatewayUrl}/api/models/discover`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ base_url: baseUrl, api_key: apiKey || undefined }),
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error((err as { error?: string }).error ?? `Discover failed: ${resp.status}`);
  }
  const data = await resp.json();
  return data.models ?? [];
}

// ── User Profile API ────────────────────────────────────────────────────

/** Fetch all user profiles from Gateway */
export async function fetchUsers(
  gatewayUrl = getGatewayUrl(),
): Promise<UserProfileListResponse> {
  const resp = await fetch(`${gatewayUrl}/api/users`);
  if (!resp.ok) throw new Error(`Failed to fetch users: ${resp.status}`);
  return resp.json();
}

/** Get the currently active user profile */
export async function fetchActiveUser(
  gatewayUrl = getGatewayUrl(),
): Promise<BackendUserProfile | null> {
  const data = await fetchUsers(gatewayUrl);
  return data.users.find((u) => u.is_active) ?? null;
}

/**
 * Create a new user profile.
 *
 * ADR-059 §7.3: the Gateway now answers `POST /api/users` with an
 * [`OperationAck`] (operation_id / state / resource_version /
 * terminal_error) — *not* the legacy `{ user, version }` envelope.
 * Older Desktop builds cast the response to `UserProfileMutationResponse`
 * and returned `data.user`, which was always `undefined` since
 * `OperationAck` has no `user` field; the call then silently
 * succeeded with the user profile never being threaded back to the
 * caller. OnboardingFlow treats this as fire-and-forget so the bug
 * never surfaced in the UI, but the contract was wrong.
 *
 * `updateUser` and `activateUser` still answer with their original
 * envelopes (`UserResponse` / `ActivateResponse`) — only the *create*
 * path is in the ADR-059 §7.3 set.
 */
export async function createUser(
  profile: CreateUserRequest,
  gatewayUrl = getGatewayUrl(),
): Promise<OperationAck> {
  const resp = await fetch(`${gatewayUrl}/api/users`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(profile),
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error((err as { error?: string }).error ?? `Failed to create user: ${resp.status}`);
  }
  // Backend now responds with an OperationAck envelope — surface it as-is.
  return (await resp.json()) as OperationAck;
}

/** Update an existing user profile */
export async function updateUser(
  userId: string,
  profile: UpdateUserRequest,
  gatewayUrl = getGatewayUrl(),
): Promise<BackendUserProfile> {
  const resp = await fetch(`${gatewayUrl}/api/users/${userId}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(profile),
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error((err as { error?: string }).error ?? `Failed to update user: ${resp.status}`);
  }
  // Backend responds with UserResponse { user, version } — unwrap the profile.
  const data = (await resp.json()) as UserProfileMutationResponse;
  return data.user;
}

/** Activate a user (deactivates all others) */
export async function activateUser(
  userId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<ActivateUserResponse> {
  const resp = await fetch(`${gatewayUrl}/api/users/${userId}/activate`, {
    method: "POST",
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error((err as { error?: string }).error ?? `Failed to activate user: ${resp.status}`);
  }
  // Backend responds with ActivateResponse { active_user_id, version }.
  return resp.json();
}

/** Reset onboarding wizard state. */
export async function resetOnboarding(): Promise<{ status: string; source: string }> {
  localStorage.removeItem("acowork_onboarding");
  return { status: "frontend_only", source: "local" };
}

// ── Embedding Model API ──────────────────────────────────────────────────

/** Fetch all embedding models with status */
export async function fetchEmbeddingModels(
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelsResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models`);
  if (!resp.ok) throw new Error(`Failed to fetch embedding models: ${resp.status}`);
  return resp.json();
}

/** Trigger download of an embedding model */
export async function downloadEmbeddingModel(
  modelId: string,
  variant?: string,
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelActionResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}/download`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ variant: variant ?? null }),
  });
  const data = await resp.json();
  if (!resp.ok) {
    throw new Error((data as EmbeddingModelActionResponse).message ?? `Download failed: ${resp.status}`);
  }
  return data as EmbeddingModelActionResponse;
}

/** Select (activate) an embedding model */
export async function selectEmbeddingModel(
  modelId: string,
  force = false,
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelActionResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}/select`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ force }),
  });
  const data = await resp.json();
  if (!resp.ok) {
    const actionResp = data as EmbeddingModelActionResponse;
    // Return the response even on CONFLICT so caller can handle dimension_mismatch
    if (resp.status === 409) return actionResp;
    throw new Error(actionResp.message ?? `Select failed: ${resp.status}`);
  }
  return data as EmbeddingModelActionResponse;
}

/** Poll download progress for an embedding model */
export async function fetchEmbeddingModelStatus(
  modelId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelStatusResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}/status`);
  if (!resp.ok) throw new Error(`Failed to fetch status: ${resp.status}`);
  return resp.json();
}

/** Delete a downloaded embedding model's files */
export async function deleteEmbeddingModel(
  modelId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelActionResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}`, {
    method: "DELETE",
  });
  const data = await resp.json();
  if (!resp.ok) {
    throw new Error((data as EmbeddingModelActionResponse).message ?? `Delete failed: ${resp.status}`);
  }
  return data as EmbeddingModelActionResponse;
}

/** Test the currently loaded embedding model */
export async function testEmbeddingModel(
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingTestResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/test`, {
    method: "POST",
  });
  if (!resp.ok) throw new Error(`Test request failed: ${resp.status}`);
  return resp.json();
}

/** Start embedding dimension migration for agents (ADR-073: instance-scoped).
 *
 * `scope` selects which rows the Runtime re-embeds: "all" (default — a model
 * swap, every vector changes meaning) or "repair" (only rows retrieval cannot
 * see — foreign-width or unembedded). Omitting it keeps the pre-field wire
 * format, which the Gateway/Rust layers also read as "all".
 */
export async function startMigration(
  modelId: string,
  instanceIds: string[],
  scope?: RebuildScope,
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelActionResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}/start-migration`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ instance_ids: instanceIds, ...(scope ? { scope } : {}) }),
  });
  const data = await resp.json();
  if (!resp.ok) throw new Error((data as EmbeddingModelActionResponse).message ?? `Migration start failed: ${resp.status}`);
  return data as EmbeddingModelActionResponse;
}

/** Get embedding migration progress for all agents */
export async function fetchMigrationProgress(
  gatewayUrl = getGatewayUrl(),
): Promise<MigrationProgressResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/migration-progress`);
  if (!resp.ok) throw new Error(`Failed to fetch migration progress: ${resp.status}`);
  return resp.json();
}

/** Select embedding model and return full migration response (handles 200 with migration info) */
export async function selectEmbeddingModelWithMigration(
  modelId: string,
  force: boolean,
  gatewayUrl = getGatewayUrl(),
): Promise<SelectModelMigrationResponse | EmbeddingModelActionResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}/select`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ force }),
  });
  const data = await resp.json();
  if (!resp.ok) throw new Error((data as EmbeddingModelActionResponse).message ?? `Select failed: ${resp.status}`);
  return data as SelectModelMigrationResponse | EmbeddingModelActionResponse;
}

// ── Cloud Embedding Providers (S1-7) ──────────────────────────────────

/** List all cloud embedding providers + the active selection (if any). */
export async function fetchCloudEmbeddingProviders(
  gatewayUrl = getGatewayUrl(),
): Promise<CloudEmbeddingProvidersResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-providers`);
  if (!resp.ok) throw new Error(`Failed to fetch cloud embedding providers: ${resp.status}`);
  return resp.json();
}

/** Select a cloud embedding model. Persists active selection and triggers
 *  an MQTT retained-topic republish so Runtime picks up the change. */
export async function selectCloudEmbeddingModel(
  providerId: string,
  modelId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<SelectCloudEmbeddingResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-providers/${encodeURIComponent(providerId)}/select`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ model_id: modelId }),
  });
  const data = await resp.json();
  if (!resp.ok) {
    throw new Error((data as SelectCloudEmbeddingResponse)?.message ?? `Select failed: ${resp.status}`);
  }
  return data as SelectCloudEmbeddingResponse;
}

/** Store an API key for a cloud embedding provider in the Gateway Vault. */
export async function setCloudEmbeddingApiKey(
  providerId: string,
  apiKey: string,
  gatewayUrl = getGatewayUrl(),
): Promise<{ provider_id: string; status: string; message: string }> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-providers/${encodeURIComponent(providerId)}/api-key`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ api_key: apiKey }),
  });
  const data = await resp.json();
  if (!resp.ok) {
    throw new Error(data?.message ?? `Failed to store API key: ${resp.status}`);
  }
  return data;
}

/** Remove a stored API key for a cloud embedding provider. */
export async function deleteCloudEmbeddingApiKey(
  providerId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<{ provider_id: string; status: string; message: string }> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-providers/${encodeURIComponent(providerId)}/api-key`, {
    method: "DELETE",
  });
  const data = await resp.json();
  if (!resp.ok) {
    throw new Error(data?.message ?? `Failed to remove API key: ${resp.status}`);
  }
  return data;
}

/** Test connectivity to a cloud embedding provider's /v1/embeddings endpoint
 *  using the stored API key. */
export async function testCloudEmbeddingProvider(
  providerId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<CloudEmbeddingTestResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-providers/${encodeURIComponent(providerId)}/test`, {
    method: "POST",
  });
  const data = await resp.json();
  if (!resp.ok) {
    throw new Error(data?.message ?? `Test request failed: ${resp.status}`);
  }
  return data as CloudEmbeddingTestResponse;
}

/** Add a user-defined cloud embedding provider.
 *  Persists to data_dir/user_embedding_providers.json + optionally stores
 *  an API key in Vault. Triggers an MQTT retained-topic republish so
 *  the broker sees the new provider on next config refresh. */
export async function addCloudEmbeddingProvider(
  payload: AddCloudEmbeddingProviderRequest,
  gatewayUrl = getGatewayUrl(),
): Promise<CloudEmbeddingProviderResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-providers`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      id: payload.id,
      name: payload.name,
      api: payload.api,
      models: payload.models,
      api_key: payload.api_key ?? undefined,
    }),
  });
  const data = await resp.json();
  if (!resp.ok) {
    throw new Error(data?.message ?? `Add provider failed: ${resp.status}`);
  }
  return data as CloudEmbeddingProviderResponse;
}

/** Update a user-defined cloud embedding provider. All fields optional.
 *  Refuses to update a bundled (offline) provider — backend returns 404. */
export async function updateCloudEmbeddingProvider(
  providerId: string,
  patch: UpdateCloudEmbeddingProviderRequest,
  gatewayUrl = getGatewayUrl(),
): Promise<CloudEmbeddingProviderResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-providers/${encodeURIComponent(providerId)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(patch),
  });
  const data = await resp.json();
  if (!resp.ok) {
    throw new Error(data?.message ?? `Update provider failed: ${resp.status}`);
  }
  return data as CloudEmbeddingProviderResponse;
}

/** Delete a user-defined cloud embedding provider. Also removes the
 *  Vault-stored API key and clears the active selection if it pointed
 *  at this provider. Refuses to delete a bundled (offline) provider. */
export async function deleteCloudEmbeddingProvider(
  providerId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<CloudEmbeddingProviderResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-providers/${encodeURIComponent(providerId)}`, {
    method: "DELETE",
  });
  const data = await resp.json();
  if (!resp.ok) {
    throw new Error(data?.message ?? `Delete provider failed: ${resp.status}`);
  }
  return data as CloudEmbeddingProviderResponse;
}


// ── LSP API ──────────────────────────────────────────────────────────────

/**
 * Fetch the LSP Relay endpoint of the node hosting an agent.
 *
 * ADR-055 §6.7 (Phase 4): the relay is a node-local sidecar, so the
 * endpoint must be resolved per agent via
 * `GET /api/agents/{id}/lsp-endpoint`. Desktop App and Agent Runtime
 * use this to discover the relay, then connect directly.
 *
 * Returns `{ ready: false, endpoint: null }` when the hosting node's
 * relay has not published a ready state.
 */
export async function fetchAgentLspEndpoint(
  agentId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<AgentLspEndpointResponse> {
  const resp = await fetch(
    `${gatewayUrl}/api/agents/${encodeURIComponent(agentId)}/lsp-endpoint`,
  );
  if (!resp.ok) throw new Error(`Failed to fetch LSP endpoint: ${resp.status}`);
  return resp.json();
}

/**
 * Build the base HTTP URL for the LSP Relay serving an agent.
 *
 * Returns `null` if the relay is not available (or no agent id was
 * provided).
 *
 * Mode note: this deliberately does NOT short-circuit in relay mode,
 * which it used to (57b3d73b, design doc 24 F7). That guard assumed the
 * relay's node-local HTTP endpoint is unreachable through the cloud
 * tunnel, and made the harness LSP panel report "relay not available"
 * on every relay-mode connection. The premise does not hold: the
 * editor's LSP WebSocket tunnels fine in relay mode (diagnostics pass,
 * rust-analyzer reaches `ready`), and this function only serves the
 * direct HTTP callers — the harness panel, project-root discovery and
 * the install-script runner. Letting them resolve the endpoint is what
 * makes the panel usable in relay mode; a genuinely absent relay still
 * returns null via the endpoint's own `ready` flag, so the existing
 * graceful-degrade path is unchanged.
 */
export async function getLspRelayUrl(
  agentId?: string,
  gatewayUrl = getGatewayUrl(),
): Promise<string | null> {
  if (!agentId) return null;
  return getCachedLspRelayEndpoint(agentId, gatewayUrl);
}

// ── LSP Relay direct API (servers / status / install) ───────────────────
//
// These functions call the LSP Relay directly (not through the Gateway).
// The caller must provide the relay base URL, typically obtained via
// `getLspRelayUrl()`.

/**
 * Fetch the configured LSP server list from the RELAY without probing
 * `PATH`.
 *
 * Kept for the read-only tooling surface (the relay's own view of the
 * registry file it loaded at start). The harness LSP panel reads the
 * **Node registry** instead via {@link fetchLspRegistry} — that is the
 * merged truth including entries the relay has not loaded yet, and it works
 * while the relay is down (ADR-091 §5.5).
 */
export async function fetchLspServers(
  relayUrl: string,
): Promise<LspServersConfig> {
  const resp = await fetch(`${relayUrl}/api/lsp/servers`);
  if (!resp.ok) {
    throw new Error(`Failed to fetch LSP servers: ${resp.status}`);
  }
  return resp.json();
}

/**
 * Fetch configured LSP servers together with per-language install status
 * in a single round-trip.
 *
 * Returns both the configured server list AND the per-language install
 * status in one call. The backend runs the PATH probes with bounded
 * concurrency (4 in flight), keeping total wall time roughly bounded
 * by a single probe timeout (~2s worst case).
 *
 * Results are cached on the LSP Relay side keyed by canonical language
 * with a configurable TTL (default 30 minutes). Pass `force = true` to
 * bypass the cache and re-probe every language — use this only on the
 * top-bar Refresh button, where a full re-probe is the user-intent.
 *
 * **Not the preferred initial-load call** — the harness UI uses the
 * two-step `fetchLspServers` + `fetchLspStatus` pair instead so the
 * list renders immediately and badges resolve incrementally. This
 * endpoint remains available for callers that want the combined payload
 * (e.g. CLI tools, one-shot scripts).
 */
export async function fetchLspServersWithStatus(
  relayUrl: string,
  options: { force?: boolean } = {},
): Promise<LspServersWithStatus> {
  const qs = options.force ? "?force=true" : "";
  const resp = await fetch(`${relayUrl}/api/lsp/servers-with-status${qs}`);
  if (!resp.ok) {
    throw new Error(
      `Failed to fetch LSP servers with status: ${resp.status}`,
    );
  }
  return resp.json();
}

/**
 * Re-probe per-language LSP installation status from the LSP Relay.
 *
 * Used by the per-row Check button: the user has already seen the
 * list, so we only need to re-probe status — there's no need to
 * re-fetch the server config.
 *
 * The relay probes `PATH` for each configured candidate command and
 * returns whether a usable binary was found. This is the source of
 * truth for the UI's "installed" badge and is used to disable the
 * Install button for already-installed servers.
 *
 * Results are cached on the LSP Relay side keyed by canonical language
 * with a configurable TTL (default 30 minutes). The harness UI does
 * **not** pass `force = true` here so the first call within TTL is
 * essentially free (HashMap read on the server); the top-bar Refresh
 * button uses `force = true` for a full re-probe.
 *
 * **`force = true` is rarely useful at this endpoint** — prefer
 * {@link fetchLspStatusForLanguage} for per-language forced probes,
 * since the batch endpoint still probes every language when forcing.
 */
export async function fetchLspStatus(
  relayUrl: string,
  options: { force?: boolean } = {},
): Promise<LspServerStatusEntry[]> {
  const qs = options.force ? "?force=true" : "";
  const resp = await fetch(`${relayUrl}/api/lsp/status${qs}`);
  if (!resp.ok) throw new Error(`Failed to fetch LSP status: ${resp.status}`);
  return resp.json();
}

/**
 * Re-probe LSP installation status for a single language.
 *
 * Preferred over {@link fetchLspStatus} for the harness UI's per-row
 * "Check Status" button — the previous behavior fetched the entire
 * status array (probing every language in the config) just to update
 * one row's badge. This endpoint probes only the requested language
 * (or hits the cache) and returns a single `LspServerStatusEntry`
 * object, not an array.
 *
 * The relay canonicalizes the input first, so language aliases (`js` →
 * `typescript`, `yml` → `yaml`, etc.) resolve cleanly. Unknown
 * languages return 404 — wrap in try/catch if the caller doesn't
 * filter by configured languages first.
 *
 * Pass `force = true` to bypass the cache for the single language
 * (e.g. immediately after an install). The relay's install endpoint
 * also drops the cache entry on success, so `force = true` is rarely
 * needed for the install flow.
 */
export async function fetchLspStatusForLanguage(
  relayUrl: string,
  language: string,
  options: { force?: boolean } = {},
): Promise<LspServerStatusEntry> {
  const qs = options.force ? "?force=true" : "";
  const encoded = encodeURIComponent(language);
  const resp = await fetch(`${relayUrl}/api/lsp/status/${encoded}${qs}`);
  if (resp.status === 404) {
    throw new Error(`Unknown LSP language: ${language}`);
  }
  if (!resp.ok) {
    throw new Error(
      `Failed to fetch LSP status for ${language}: ${resp.status}`,
    );
  }
  return resp.json();
}

// ── LSP registry admin API (ADR-091 §5.5 / §5.7) ────────────────────────
//
// The Node owns the LSP server registry; the Gateway reverse-proxies it at
// `/api/nodes/{id}/lsp-admin/{*tail}` and is where `can_view` / `can_manage`
// are decided. Two things follow from that shape and are easy to get wrong:
//
// * the base URL is derived from the relay proxy URL, not from a second
//   endpoint lookup — the registry must stay reachable when the relay is NOT
//   ready (§5.5: a broken language server is exactly why the user is here),
//   so nothing here may go through `lsp_endpoint` readiness;
// * keys are server NAMES, and for presets the name is the canonical
//   language — so `js` must be canonicalised by the caller the same way the
//   relay does it, or the write lands on a new custom key.

/**
 * The registry-admin base URL for a node, from its relay proxy base URL
 * (`{gateway}/api/nodes/{id}/lsp` → `{gateway}/api/nodes/{id}/lsp-admin`).
 *
 * Returns `null` for a URL that is not a Gateway node proxy (a raw
 * loopback relay address, for instance): the admin surface only exists
 * behind the Gateway, because that is where the caller's identity — and
 * therefore the permission check — lives.
 */
export function lspAdminUrlFromRelayUrl(relayUrl: string): string | null {
  const m = /^(.*)\/api\/nodes\/([^/]+)\/lsp\/?$/.exec(relayUrl);
  if (!m) return null;
  return `${m[1]}/api/nodes/${m[2]}/lsp-admin`;
}

/** Shared error rendering for the registry surface (`{error, code}` bodies). */
async function lspAdminError(resp: Response, action: string): Promise<Error> {
  let detail = "";
  try {
    const data = (await resp.json()) as { error?: string; stderr?: string; stdout?: string };
    detail = data.error || data.stderr || data.stdout || "";
  } catch {
    // Non-JSON answer (a Gateway 502 page, a tunnel reset) — status only.
  }
  return new Error(detail ? `${action}: ${resp.status} — ${detail}` : `${action}: ${resp.status}`);
}

/**
 * `GET .../lsp-admin/servers` — the merged runtime registry.
 *
 * This is the list the panel renders: presets ∪ customs, plus the persisted
 * install state and the two things a file cannot say (`conflicts`,
 * `installing`). It is a fast memory read on the Node and never blocks on a
 * PATH probe, so badges may be stale — the live probe is the separate
 * {@link fetchLspStatus} call.
 */
export async function fetchLspRegistry(
  adminUrl: string,
): Promise<LspRegistryView> {
  const resp = await fetch(`${adminUrl}/servers`);
  if (!resp.ok) throw await lspAdminError(resp, "Failed to fetch LSP registry");
  return resp.json();
}

/**
 * `GET .../lsp-admin/presets` — the read-only shipped registry.
 *
 * The "add custom server" form reads candidate definitions from here so a
 * typo in a new key is visible against what the suite already ships (D1).
 */
export async function fetchLspPresets(
  adminUrl: string,
): Promise<LspServersConfig> {
  const resp = await fetch(`${adminUrl}/presets`);
  if (!resp.ok) throw await lspAdminError(resp, "Failed to fetch LSP presets");
  return resp.json();
}

/** `GET .../servers/{name}/script` — the install script text (D14 UI gate). */
export async function fetchLspInstallScript(
  adminUrl: string,
  name: string,
): Promise<LspInstallScriptResponse> {
  const resp = await fetch(`${adminUrl}/servers/${encodeURIComponent(name)}/script`);
  if (!resp.ok) throw await lspAdminError(resp, "Failed to fetch install script");
  return resp.json();
}

/**
 * `POST .../servers/{name}/install` — run the install script on the Node.
 *
 * Deliberately uncapped in time: this is apt / rustup / npm downloading in
 * the foreground, and the Gateway's forward client for this path has a
 * connect timeout only (ADR-089 `relay_proxy_client`). The Node never
 * restarts the relay for an install (§5.10) — it fires a forced probe
 * instead, so the response's `willRestart` is false.
 */
export async function runLspInstall(
  adminUrl: string,
  name: string,
): Promise<LspInstallRunResponse> {
  const resp = await fetch(`${adminUrl}/servers/${encodeURIComponent(name)}/install`, {
    method: "POST",
  });
  const data = await resp.json().catch(() => ({}));
  if (!resp.ok) throw await lspAdminError(resp, "Install failed");
  return data as LspInstallRunResponse;
}

/** `GET .../servers/{name}/install/status` — the in-flight view of one key. */
export async function fetchLspInstallStatus(
  adminUrl: string,
  name: string,
): Promise<LspInstallStatusResponse> {
  const resp = await fetch(`${adminUrl}/servers/${encodeURIComponent(name)}/install/status`);
  if (!resp.ok) throw await lspAdminError(resp, "Failed to fetch install status");
  return resp.json();
}

/**
 * `POST .../servers` — add a custom server.
 *
 * Body is flat: `name` plus the metadata block (the Node `#[serde(flatten)]`
 * s them together). Answers `409 already_exists` for a preset key — presets
 * are not editable, only overridable through `candidate_args`.
 */
export async function addLspServer(
  adminUrl: string,
  name: string,
  metadata: LspServerEntry,
): Promise<LspMutationResponse> {
  const resp = await fetch(`${adminUrl}/servers`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name, ...metadata }),
  });
  if (!resp.ok) throw await lspAdminError(resp, "Failed to add LSP server");
  return resp.json();
}

/**
 * `PUT .../servers/{name}` — update the instance block.
 *
 * For a preset entry the Node keeps only `candidate_args` (the rest is
 * refreshed from the shipped preset on the next merge, D6), so the form must
 * not pretend the other fields were saved.
 */
export async function updateLspServer(
  adminUrl: string,
  name: string,
  metadata: LspServerEntry,
): Promise<LspMutationResponse> {
  const resp = await fetch(`${adminUrl}/servers/${encodeURIComponent(name)}`, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(metadata),
  });
  if (!resp.ok) throw await lspAdminError(resp, "Failed to update LSP server");
  return resp.json();
}

/**
 * `DELETE .../servers/{name}` — drop the registry entry.
 *
 * Custom only (presets come back from the suite, so the Node refuses with
 * `409 preset_not_removable`). The language-server binary stays on disk —
 * removing the record is not an uninstall of a tool other software may share.
 */
export async function deleteLspServer(
  adminUrl: string,
  name: string,
): Promise<LspMutationResponse> {
  const resp = await fetch(`${adminUrl}/servers/${encodeURIComponent(name)}`, {
    method: "DELETE",
  });
  if (!resp.ok) throw await lspAdminError(resp, "Failed to delete LSP server");
  return resp.json();
}


// ── Settings API ────────────────────────────────────────────────────────
//
// ADR-056: Global default compact model. The Gateway persists this on the
// `provider_list.json` top-level `default_compact_models` field and pushes
// it as part of `acowork/global/providers` (AvailableProviders) via MQTT
// retained, so Runtimes can apply the three-tier distillation fallback.

/**
 * `GET /api/settings/default-compact-model` — read the user's ordered
 * global candidate list. Empty array when not configured.
 */
export async function getDefaultCompactModels(
  gatewayUrl = getGatewayUrl(),
): Promise<CompactModelRef[]> {
  const resp = await fetch(`${gatewayUrl}/api/settings/default-compact-model`);
  if (!resp.ok) {
    throw new Error(`Failed to fetch default compact models: ${resp.status}`);
  }
  const data = (await resp.json()) as DefaultCompactModelResponse;
  return data.default_compact_models ?? [];
}

/**
 * `PUT /api/settings/default-compact-model` — replace the whole ordered
 * candidate list. An empty array clears it (Runtime then falls back to
 * provider compact_model and current chat model only).
 *
 * Returns the new list (as persisted by the Gateway).
 *
 * Throws on validation failure (unknown provider_id, or model_id not
 * belonging to that provider) — Gateway returns HTTP 422 with the
 * `error` field set; the list is validated atomically, so a rejected
 * PUT leaves the previous list untouched (ADR-056 §4.1).
 */
export async function setDefaultCompactModels(
  refs: CompactModelRef[],
  gatewayUrl = getGatewayUrl(),
): Promise<CompactModelRef[]> {
  const resp = await fetch(`${gatewayUrl}/api/settings/default-compact-model`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ default_compact_models: refs }),
  });
  const data = (await resp.json().catch(() => ({}))) as {
    default_compact_models?: CompactModelRef[];
    error?: string;
  };
  if (!resp.ok) {
    throw new Error(data.error ?? `Failed to set default compact models: ${resp.status}`);
  }
  return data.default_compact_models ?? [];
}

/**
 * `GET /api/nodes` — list all known Node Agents (online + offline),
 * sorted by node_id (ADR-055 §6.13.3 / Phase 3g).
 *
 * Returns an empty list when the Gateway has no node registry (MQTT
 * disabled) or when no node has ever reported.
 *
 * Field normalization: the Gateway contract is snake_case (`node_id`,
 * `agent_count`, …) — matching the `NodeInfo` type below. Gateway
 * builds from ADR-055 Phases 1–3 instead serialized `NodeResponse`
 * with camelCase names, which the desktop parsed as `undefined` —
 * every node failed to match its agent bucket, collapsing the
 * remote-mode sidebar into a single gray "unknown" group. Accepting
 * both shapes keeps desktop and Gateway independently upgradeable.
 */
export async function fetchNodes(gatewayUrl = getGatewayUrl()): Promise<NodeInfo[]> {
  const resp = await fetch(`${gatewayUrl}/api/nodes`);
  if (!resp.ok) {
    throw new Error(`Failed to fetch nodes: ${resp.status}`);
  }
  const raw = (await resp.json()) as Array<Record<string, unknown>>;
  return raw.map(normalizeNode);
}

/** Normalize one `/api/nodes` entry (snake_case contract or legacy camelCase) into `NodeInfo`. */
function normalizeNode(raw: Record<string, unknown>): NodeInfo {
  const snakeOrCamel = (snake: string, camel: string): unknown => raw[snake] ?? raw[camel];
  return {
    node_id: (snakeOrCamel("node_id", "nodeId") ?? "") as string,
    online: raw.online === true,
    online_since: snakeOrCamel("online_since", "onlineSince") as string | undefined,
    node_name: snakeOrCamel("node_name", "nodeName") as string | undefined,
    gateway_managed: snakeOrCamel("gateway_managed", "gatewayManaged") === true,
    hostname: raw.hostname as string | undefined,
    os: raw.os as string | undefined,
    arch: raw.arch as string | undefined,
    node_version: snakeOrCamel("node_version", "nodeVersion") as string | undefined,
    protocol_version: snakeOrCamel("protocol_version", "protocolVersion") as number | undefined,
    capabilities: (raw.capabilities ?? []) as string[],
    max_agents: snakeOrCamel("max_agents", "maxAgents") as number | undefined,
    agent_count: snakeOrCamel("agent_count", "agentCount") as number | undefined,
    http_endpoint: snakeOrCamel("http_endpoint", "httpEndpoint") as string | undefined,
    // ADR-055 §6.7: manage-list only (ADR-087 D5), so `undefined` for a
    // non-manager even when the relay is up — callers must treat a
    // missing endpoint as "no relay visible to me", not "no relay".
    lsp_endpoint: snakeOrCamel("lsp_endpoint", "lspEndpoint") as string | undefined,
    // ADR-087: permission bits (absent on pre-087 Gateways — undefined).
    can_manage: raw.can_manage as boolean | undefined,
    is_guest: raw.is_guest as boolean | undefined,
    visibility: raw.visibility as string | undefined,
  };
}

// ── ADR-087: node / agent permissions (owner · guests · visibility) ───
//
// Read side is one `GET .../permissions` per resource (manage-gated);
// writes go through the three PATCH endpoints (visibility / guests →
// owner ∨ admin, owner → admin). The app-wide authFetch interceptor
// attaches the bearer token, so these are plain fetches.

/** Response of `GET /api/{agents|nodes}/{id}/permissions`. */
export interface ResourcePermissions {
  owner_user_id: string | null;
  guests: string[];
  /** `"shared"|"private"` for agents, `"public"|"private"` for nodes. */
  visibility: string;
  /** Caller may write visibility/guests (owner ∨ admin ∨ local). */
  can_attribute: boolean;
  /**
   * Caller may *publish* this resource (`shared` / `public`).
   *
   * Distinct from `can_attribute`: that one is a permission answer, an
   * admin passes it even on a row that has no owner. This one is the
   * data answer — an ownerless row cannot hold a published value
   * (ADR-087 D6), so the switch must be disabled rather than offered
   * and then rejected. Absent on a pre-087 Gateway.
   */
  can_set_visibility?: boolean;
  /** No owner on record — the row is admin-only (ADR-087 D7). */
  ownerless?: boolean;
}

async function requestPermissions(
  kind: "agents" | "nodes",
  id: string,
): Promise<ResourcePermissions> {
  const resp = await fetch(
    `${getGatewayUrl()}/api/${kind}/${encodeURIComponent(id)}/permissions`,
  );
  if (!resp.ok) {
    throw new Error(`Failed to fetch permissions: ${await readGatewayError(resp)}`);
  }
  return (await resp.json()) as ResourcePermissions;
}

async function patchPermissions(
  kind: "agents" | "nodes",
  id: string,
  segment: "visibility" | "guests" | "owner",
  body: unknown,
): Promise<void> {
  const resp = await fetch(
    `${getGatewayUrl()}/api/${kind}/${encodeURIComponent(id)}/${segment}`,
    {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    },
  );
  if (!resp.ok) {
    // `HttpApiError` carries the parsed denial: callers surface
    // `err.message` (localized "you lack permission" for 403, server
    // detail otherwise) instead of a raw `Failed to update …: forbidden`.
    throw await httpApiError(resp);
  }
}

/** Decode a Gateway error body (`{error}` or `{error:{message}}`) to text. */
async function readGatewayError(resp: Response): Promise<string> {
  try {
    const body = await resp.json();
    if (typeof body?.error === "string") return body.error;
    if (typeof body?.error?.message === "string") return body.error.message;
  } catch {
    /* non-JSON — fall through */
  }
  return String(resp.status);
}

export const fetchAgentPermissions = (id: string) => requestPermissions("agents", id);
export const fetchNodePermissions = (id: string) => requestPermissions("nodes", id);
export const patchAgentVisibility = (id: string, visibility: string) =>
  patchPermissions("agents", id, "visibility", { visibility });
export const patchAgentGuests = (id: string, guests: string[]) =>
  patchPermissions("agents", id, "guests", { guests });
export const patchNodeVisibility = (id: string, visibility: string) =>
  patchPermissions("nodes", id, "visibility", { visibility });
export const patchNodeGuests = (id: string, guests: string[]) =>
  patchPermissions("nodes", id, "guests", { guests });

/**
 * Claim or transfer ownership (ADR-087 D7). Admin only — transfer is an
 * authorization change, so an owner may share but never hand over
 * mastership. `null` / `""` clears the owner back to ownerless.
 *
 * The dialog needs this because an ownerless resource is admin-only and
 * therefore cannot be published: `PATCH .../visibility` rejects
 * `shared`/`public` with a 409 until someone claims it. Without a UI path
 * to claim, the visibility switch is unusable on every install that
 * predates ADR-087 (all existing agents/nodes start out ownerless).
 *
 * It is also the write behind the dialog's owner picker, which is how an
 * admin hands a resource to a specific account. Before that picker the only
 * owner the UI could produce was whoever was clicking — so in practice
 * ownership was not assignable at all, only self-claimable.
 */
export const patchAgentOwner = (id: string, ownerUserId: string | null) =>
  patchPermissions("agents", id, "owner", { owner_user_id: ownerUserId });
export const patchNodeOwner = (id: string, ownerUserId: string | null) =>
  patchPermissions("nodes", id, "owner", { owner_user_id: ownerUserId });

/**
 * `PATCH /api/nodes/{id}` — change a node's DISPLAY name (ADR-075 D4).
 *
 * Only `node_name` moves: `node_id` is a UUID routing key and is never
 * touched, so no agent re-bucketing or reconnect follows. Node-manage
 * gated on the Gateway (owner ∨ admin) — the same tier as install /
 * fs browse, which is why this goes through the Gateway rather than
 * straight to the node.
 */
export const patchNodeName = async (id: string, nodeName: string): Promise<void> => {
  const resp = await fetch(`${getGatewayUrl()}/api/nodes/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ node_name: nodeName }),
  });
  if (!resp.ok) {
    throw await httpApiError(resp);
  }
};

// ── Structured error codes (ADR-059 §6.3) ──────────────────────────────
//
// The Gateway's mutation APIs fail with `{ error, code, structured? }`
// where `structured.code` is one of the 5 closed protocol codes below.
// Clients MUST branch on the machine-readable code — never on
// human-readable text (the old "503 / never enrolled" matching is
// gone, Phase 5.5).

export const STRUCTURED_ERROR_CODES = {
  dependencyNotReady: "dependency_not_ready",
  operationUncertain: "operation_uncertain",
  operationExpired: "operation_expired",
  resourceVersionConflict: "resource_version_conflict",
  handshakeTimeout: "handshake_timeout",
} as const;

/** Parsed structured Gateway error (ADR-059 §6.3). */
export interface StructuredGatewayError {
  status: number;
  /** Human-readable message (diagnostics only — never branch on it). */
  error: string;
  /** Machine-readable protocol code, when the body carried one. */
  code?: string;
  /** Aggregated bootstrap phase at failure time (SCREAMING_SNAKE_CASE). */
  current_phase?: string;
  /** Subsystem-level diagnostic (e.g. "node.local not ready"). */
  phase_detail?: string;
  /** Operation id of the failed mutation (operation_uncertain). */
  operation_id?: string;
  /** Current resource version (resource_version_conflict). */
  current_version?: number;
  /** The version the client expected (resource_version_conflict). */
  client_expected_version?: number;
}

/**
 * Parse a Gateway error response into a [`StructuredGatewayError`].
 *
 * Returns `null` when the body is not a Gateway API error (e.g. an
 * HTML error page from a proxy) — callers then fall back to a plain
 * status-based message.
 */
export async function parseStructuredError(
  resp: Response,
): Promise<StructuredGatewayError | null> {
  const data = (await resp.json().catch(() => ({}))) as Record<string, unknown>;
  if (typeof data.error !== "string") return null;
  const structured = (data.structured ?? {}) as Record<string, unknown>;
  const code = typeof structured.code === "string" ? structured.code : undefined;
  return {
    status: resp.status,
    error: data.error,
    code,
    current_phase: typeof structured.current_phase === "string" ? structured.current_phase : undefined,
    phase_detail: typeof structured.phase_detail === "string" ? structured.phase_detail : undefined,
    operation_id: typeof structured.operation_id === "string" ? structured.operation_id : undefined,
    current_version: typeof structured.current_version === "number" ? structured.current_version : undefined,
    client_expected_version: typeof structured.client_expected_version === "number" ? structured.client_expected_version : undefined,
  };
}

/** True when the error carries the `dependency_not_ready` code. */
export function isDependencyNotReady(err: StructuredGatewayError | null): boolean {
  return err?.code === STRUCTURED_ERROR_CODES.dependencyNotReady;
}

/** True when the error carries the `operation_uncertain` code. */
export function isOperationUncertain(err: StructuredGatewayError | null): boolean {
  return err?.code === STRUCTURED_ERROR_CODES.operationUncertain;
}

/** True when the error carries the `operation_expired` code. */
export function isOperationExpired(err: StructuredGatewayError | null): boolean {
  return err?.code === STRUCTURED_ERROR_CODES.operationExpired;
}

/** True when the error carries the `resource_version_conflict` code. */
export function isResourceVersionConflict(err: StructuredGatewayError | null): boolean {
  return err?.code === STRUCTURED_ERROR_CODES.resourceVersionConflict;
}

/** True when the error carries the `handshake_timeout` code. */
export function isHandshakeTimeout(err: StructuredGatewayError | null): boolean {
  return err?.code === STRUCTURED_ERROR_CODES.handshakeTimeout;
}

/**
 * Client handling strategy per code (ADR-059 §6.3):
 *
 * - `dependency_not_ready` — retry once the bootstrap phase is READY:
 *   poll `get_bootstrap` (Tauri) or `GET /api/bootstrap`, then resubmit.
 * - `operation_uncertain` — the mutation may or may not have applied
 *   (MQTT dropped / Gateway restarted). NEVER blindly retry: surface
 *   the `operation_id` and let the user re-check later.
 * - `operation_expired` — the operation's lease expired; re-submit as
 *   a fresh operation.
 * - `resource_version_conflict` — the client's `expected_version` is
 *   stale (typically a Gateway restart). Re-fetch the bootstrap
 *   snapshot for the new `version` and let the user confirm re-submit.
 * - `handshake_timeout` — a readiness probe timed out; retry the
 *   handshake.
 */


