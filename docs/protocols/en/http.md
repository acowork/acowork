# HTTP Protocol

> Gateway exposes a REST API at `127.0.0.1:19876` (default). Built on Axum.
> For detailed route aggregation see source: [`core/acowork-gateway/src/http/routes.rs`](../../../core/acowork-gateway/src/http/routes.rs)
>
> **After ADR-033 + ADR-034**: Gateway is the **single entry point** for all HTTP requests; Runtime no longer directly serves clients.
> Data requests are **reverse-proxied** by Gateway to Runtime over localhost HTTP; event triggering, command push, and real-time streaming all go over [MQTT](./mqtt.md).

---

## Table of Contents

- [1. Basic Conventions](#1-basic-conventions)
- [2. Communication Flow](#2-communication-flow)
- [3. API Category Overview](#3-api-category-overview)
- [4. Gateway Native Endpoints (do not require Runtime online)](#4-gateway-native-endpoints-do-not-require-runtime-online)
  - [4.1 System & Health](#41-system--health)
  - [4.2 Agent Package Management](#42-agent-package-management)
  - [4.3 Agent Lifecycle Control](#43-agent-lifecycle-control)
  - [4.4 Avatar / Manifest Resources](#44-avatar--manifest-resources)
  - [4.5 LLM Providers & Models](#45-llm-providers--models)
  - [4.6 MCP Catalog](#46-mcp-catalog)
  - [4.7 Embedding Models](#47-embedding-models)
  - [4.8 Users & User-level Avatars](#48-users--user-level-avatars)
  - [4.9 Cron Scheduled Tasks](#49-cron-scheduled-tasks)
  - [4.10 Skills](#410-skills)
  - [4.11 Debug & Developer Tools](#411-debug--developer-tools)
  - [4.12 Remote File System Browsing](#412-remote-file-system-browsing)
  - [4.13 Global Resource Snapshot (Runtime active pull entry)](#413-global-resource-snapshot-runtime-active-pull-entry)
- [5. Gateway → Runtime Reverse Proxy (requires Runtime online)](#5-gateway--runtime-reverse-proxy-requires-runtime-online)
  - [5.1 Agent Runtime Configuration](#51-agent-runtime-configuration)
  - [5.2 Session Read-Only Queries](#52-session-read-only-queries)
  - [5.3 Attachments](#53-attachments)
    - [5.3.1 `POST /sessions/{sid}/files`](#531-post-sessionssidfiles)
    - [5.3.2 `GET /files/{document_id}`](#532-get-filesdocument_id)
    - [5.3.3 `attached_items` in message entries](#533-attached_items-in-message-entries)
  - [5.4 Memory](#54-memory)
    - [5.4.1 Service-direct prefix `/api/memory/*` (ADR-101)](#541-service-direct-prefix-apimemoryadr-100)
  - [5.5 Workspace](#55-workspace)
- [6. Static File Service (direct streaming of raw bytes)](#6-static-file-service-direct-streaming-of-raw-bytes)
- [7. Migrated to MQTT (HTTP endpoints removed)](#7-migrated-to-mqtt-http-endpoints-removed)
- [8. Common Error Codes](#8-common-error-codes)
- [9. Typical Request Examples](#9-typical-request-examples)
- [10. Notes](#10-notes)

---

## 1. Basic Conventions

- **Base URL**: `http://127.0.0.1:19876` (adjustable via `[http]` section in `gateway.toml`)
- **Content-Type**: `application/json; charset=utf-8`
- **Authentication**: when `[http].auth_enabled = true`, all `/api/*` requests must carry `Authorization: Bearer <token>`, token file located at `<data_dir>/http_token`.
- **Error format**: `{ "error": "..." }` + corresponding HTTP status code
- **Streaming event channel (MQTT)**: chat event streaming no longer uses WebSocket; clients subscribe to MQTT topic `chat/stream/{session_id}` (Desktop App subscribes via its Tauri backend MQTT client).
- **Command / write channel (MQTT)**: user-initiated session control (send message, activate, rename, delete, close, continue) and human‑in‑the‑loop approvals / Q&A also go over MQTT topics `acowork/agents/{id}/sessions/control/{cmd}`, no longer over HTTP (see [§7](#7-migrated-to-mqtt-http-endpoints-removed)).
- **Global resource active pull (HTTP)**: Runtime, after mqtt client + available_cache are ready (phase_a), actively `GET /api/global-resources` and performs a **503 retry loop** (30s total budget) as a **fallback to avoid retained-delivery race** alongside MQTT retained push. Shares the same `AvailableResourceCache::update_from_mqtt` processing path; 503 semantics detailed in [§4.13](#413-global-resource-snapshot-runtime-active-pull-entry).

---

## 2. Communication Flow

```mermaid
sequenceDiagram
    autonumber
    participant C as Client (Desktop App / CLI)
    participant G as Gateway (Axum)
    participant B as rumqttd Broker
    participant RT as Agent Runtime (MQTT client + localhost HTTP)

    C->>G: HTTP request + Bearer Token
    G->>G: auth / path parsing

    alt Gateway native handling
        G-->>C: 200 + JSON (no Runtime dependency)
    else Gateway → Runtime reverse proxy
        G->>RT: HTTP reverse proxy → Runtime localhost HTTP (127.0.0.1:random)
        RT-->>G: JSON / bytes
        G-->>C: 200 + JSON (verbatim passthrough)
    else Static files (Gateway returns raw byte stream)
        G-->>C: 200 + raw bytes (HTML / images / videos)
    else User commands / streaming events (moved to MQTT)
        C->>B: PUB acowork/agents/{id}/sessions/control/{cmd}
        B-->>RT: control/{cmd}
        RT-->>B: PUB chunk / tool_call / done
        B-->>C: subscribed to chat/stream/{session_id} receives events
    end
```

**Architecture highlights**:

| Category | Handler | Runtime online required? |
|---|---|---|
| Gateway native | Gateway single‑point processing | **No** |
| Gateway → Runtime reverse proxy | Gateway passthrough to Runtime localhost HTTP | **Yes** (503 if offline) |
| Gateway → memory-service reverse proxy (ADR-101) | Gateway passthrough to `acowork-memory` `127.0.0.1:18084` | **No** (Runtime is not involved in the memory surface at all; the service is kept alive by the Gateway supervisor, 503 while it is not ready) |
| Static files | Gateway reads disk and returns byte stream | **No** (only file existence required) |
| MQTT commands / streaming | rumqttd Broker relay | Yes (Runtime responds via MQTT subscription) |
| Global resource active pull (`GET /api/global-resources`) | Gateway single‑point responds to Runtime active pull | **No** (see [§4.13](#413-global-resource-snapshot-runtime-active-pull-entry)) |

Gateway **does not persist business data**: Skill, Agent runtime config, Session state, etc. are stored in Runtime local files / SQLite; since ADR-101 memory belongs to a separate process, `acowork-memory` (one database per agent instance — Gateway only supervises its lifecycle and reverse-proxies it). Gateway pulls snapshots or passthrough requests via HTTP reverse proxy, while commands / writes go through MQTT control topics.

---

## 3. API Category Overview

| Category | Count | Handler | Runtime dependency |
|---|---|---|---|
| **A. Gateway native** | ~50 | Gateway local storage / Vault / process management | No |
| **B. Gateway → Runtime reverse proxy** | ~25 | Passthrough to Runtime localhost HTTP | **Yes** |
| **C. Static files** | 2 path patterns | Gateway directly `fs::read` streaming return | No |
| **D. MQTT commands / streaming** (HTTP endpoints removed) | — | rumqttd Broker | Yes |

Source mapping:

| Category | Gateway implementation modules |
|---|---|
| A | [`agents.rs`](../../../core/acowork-gateway/src/http/agents.rs), [`provider_api.rs`](../../../core/acowork-gateway/src/http/provider_api.rs), [`models_api.rs`](../../../core/acowork-gateway/src/http/models_api.rs), [`mcp_catalog_api.rs`](../../../core/acowork-gateway/src/http/mcp_catalog_api.rs), [`embedding_api.rs`](../../../core/acowork-gateway/src/http/embedding_api.rs), [`users_api.rs`](../../../core/acowork-gateway/src/http/users_api.rs), [`cron_api.rs`](../../../core/acowork-gateway/src/http/cron_api.rs), [`skills_api.rs`](../../../core/acowork-gateway/src/http/skills_api.rs), [`config_api.rs`](../../../core/acowork-gateway/src/http/config_api.rs), [`fs_browse.rs`](../../../core/acowork-gateway/src/http/fs_browse.rs), [`debug_mqtt.rs`](../../../core/acowork-gateway/src/http/debug_mqtt.rs), [`publish_api.rs`](../../../core/acowork-gateway/src/http/publish_api.rs), [`global_resources_api.rs`](../../../core/acowork-gateway/src/http/global_resources_api.rs), [`intent_api.rs`](../../../core/acowork-gateway/src/http/intent_api.rs) |
| B | [`proxy.rs`](../../../core/acowork-gateway/src/http/proxy.rs) (ADR-033 Phase 2 + ADR-034 Phase 3) |
| C | [`workspaces.rs`](../../../core/acowork-gateway/src/http/workspaces.rs) (static resource part only) |

---

## 4. Gateway Native Endpoints (do not require Runtime online)

Gateway handles these directly, without needing the Runtime subprocess online. Covers: system health, Agent package management, LLM Provider / Models global resources, MCP catalog, embedding models, user profiles, Cron, Skills, debug tools.

### 4.1 System & Health

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | Health check (no auth), includes IPC (MQTT) / CronStore / disk space |
| GET | `/api/status` | System status: version, running Agent count, memory usage; if `mqtt.auth_enabled` is enabled, additionally returns `mqtt_username` / `mqtt_password` (Desktop MQTT credential delivery, ADR-055 Phase 5a) |
| GET | `/api/config` | Read Gateway configuration |
| PUT | `/api/config` | Update log level, log rotation, idle_timeout, default provider/model, HF mirror, etc. |
| DELETE | `/api/logs` | Clear logs |
| GET | `/api/agents/{id}/lsp-endpoint` | LSP Relay endpoint (node-local, ADR-055 §6.7): resolves the relay base URL (`endpoint`/`ready` fields) for the hosting Node per agent, for Desktop / Runtime to connect directly |

### 4.2 Agent Package Management

Package-level CRUD and publishing. Packages are installed to `<packages_dir>`, and Gateway maintains a manifest in `installed_agents`.

> `<packages_dir>` defaults to `<node_home>/packages` (`ACOWORK_NODE_HOME` env → `$HOME/.acowork/acowork-node` → `./.acowork-node`), giving local and standalone Node agents the same layout. File operations actually execute on the Node side; Gateway only exposes the HTTP API and maintains the manifest. See [ADR-055 §6.11 / §6.20](../adr/zh/ADR-055-remote-runtime-node-topology.md) and [runbook `single-machine-remote-topology.md`](../runbooks/single-machine-remote-topology.md).

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/agents` | List all installed Agents (including status, avatar, mqtt_online, etc.) |
| GET | `/api/agents/{id}` | Agent details (manifest, installed/connected/dev_mode, etc.) |
| DELETE | `/api/agents/{id}` | Uninstall Agent |
| POST | `/api/agents/install` | Install `.agent` package (multipart) |
| POST | `/api/agents/{id}/clone` | Clone Agent (skeleton or full) |
| POST | `/api/agents/{id}/publish/prepare` | Prepare packaging (validation, cleanup) |
| POST | `/api/agents/{id}/publish/build` | Build `.agent` package |
| POST | `/api/agents/{id}/publish/export` | Export package to target path |
| POST | `/api/agents/{id}/publish/install-locally` | Install build artifact locally |
| GET | `/api/packages/{agent_id}/download` | Download `.agent` package (Node install pull path); when auth enabled, validates `X-ACowork-Node-Token` (ADR-055 Phase 5a): missing/mismatch → 401/403 |

### 4.3 Agent Lifecycle Control

Subprocess-level control: start / stop / restart-debug / model provider probing.
**Model/provider switching** is done via MQTT `sessions/control/model_switch`, not HTTP.

| Method | Path | Purpose |
|---|---|---|
| POST | `/api/agents/{id}/start` | Start Agent Runtime subprocess |
| POST | `/api/agents/{id}/stop` | Stop Agent |
| POST | `/api/agents/{id}/restart-debug` | Restart in debug mode (enables Debug channel) |
| GET | `/api/agents/{id}/model` | Currently used model / provider (Gateway derives from manifest) |

### 4.4 Avatar / Manifest Resources

Gateway caches avatar resources (readable even when Agent is stopped); synchronized via MQTT `AgentHello`.

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/agents/{id}/avatar` | Retrieve built‑in avatar image from Agent package |
| POST | `/api/agents/{id}/manifest/avatar` | Upload/update manifest avatar |
| POST | `/api/agents/{id}/manifest/file` | Upload manifest resource file |
| GET | `/api/agents/{id}/manifest/avatar-assets` | List manifest avatar resources |
| GET | `/api/agents/{id}/avatar-file` | Retrieve avatar resource file |
| DELETE | `/api/agents/{id}/avatar-file` | Delete avatar resource |
| GET | `/api/agents/{id}/avatar-config` | Retrieve avatar runtime config (Gateway cache) |
| PUT | `/api/agents/{id}/avatar-config` | Update avatar config (takes effect only when Agent is stopped) |

### 4.5 LLM Providers & Models

Global LLM resources. API keys are encrypted and stored in Gateway Vault; configuration (base_url / models / compact_model) stored in `provider_list.json`.

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/providers` | List Providers (API key masked) |
| POST | `/api/providers` | Add new Provider (key + config) |
| DELETE | `/api/providers/{provider}` | Delete Provider |
| PUT | `/api/providers/{provider}` | Update Provider (key / config) |
| GET | `/api/models` | Models from all Providers (including local ollama / lmstudio) |
| GET | `/api/models/{provider}` | Models from a single Provider |
| POST | `/api/models/discover` | Discover models from custom base URL (OpenAI-compatible) |

### 4.6 MCP Catalog

Global MCP server catalog (shared registry similar to Providers, including credentials).

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/mcp-catalog` | List all MCP catalog entries (env fields masked) |
| PUT | `/api/mcp-catalog` | Replace catalog entirely |
| POST | `/api/mcp-catalog` | Add one entry |
| DELETE | `/api/mcp-catalog/{name}` | Delete entry |
| POST | `/api/mcp-catalog/probe` | Health probe (probe new config) |
| POST | `/api/mcp-catalog/{name}/probe` | Health probe (probe existing entry) |

### 4.7 Embedding Models

Embedding sidecar managed by Gateway (ONNX Runtime).

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/embedding-models` | List available embedding models and status |
| POST | `/api/embedding-models/test` | Probe model connectivity |
| POST | `/api/embedding-models/{id}/download` | Trigger model download |
| POST | `/api/embedding-models/{id}/select` | Switch current model |
| GET | `/api/embedding-models/{id}/status` | Download / load status |
| DELETE | `/api/embedding-models/{id}` | Delete downloaded model |
| GET | `/api/embedding-models/migration-progress` | Overall progress of embedding dimension migration |
| POST | `/api/embedding-models/{id}/start-migration` | Start migration |

### 4.8 Users & User-level Avatars

Global user profiles (independent of Agents).

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/users` | List user profiles |
| POST | `/api/users` | Create user profile |
| PUT | `/api/users/{user_id}` | Update user profile |
| POST | `/api/users/{user_id}/activate` | Activate user |
| GET | `/api/user/avatar-config` | Avatar config for currently active user |
| PUT | `/api/user/avatar-config` | Update avatar config |
| GET | `/api/user/avatar-assets` | List available avatar assets |
| GET | `/api/user/avatar-file` | Retrieve avatar file |
| POST | `/api/user/avatar-file` | Upload avatar file |
| DELETE | `/api/user/avatar-file` | Delete avatar file |

### 4.9 Cron Scheduled Tasks

Cron is managed by Gateway itself (persisted in SQLite).

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/agents/{id}/cron` | List Agent's scheduled tasks |
| POST | `/api/agents/{id}/cron` | Register new scheduled task (schedule + action + params) |
| DELETE | `/api/agents/{id}/cron/{cron_id}` | Delete scheduled task |

### 4.10 Skills

Skills are read from the `skills/` directory of an installed package.

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/agents/{id}/skills` | List skills |
| GET | `/api/agents/{id}/skills/{name}` | Skill details (SKILL.md parsing) |
| GET | `/api/agents/{id}/skills/{name}/history` | Skill execution history |
| POST | `/api/agents/{id}/skills/import` | Import skill ZIP (multipart) |

### 4.11 Debug & Developer Tools

Only exposed on localhost; must not be exposed to the network.

| Method | Path | Purpose |
|---|---|---|
| POST | `/api/debug/mqtt/shutdown` | Request broker thread exit (manual debugging only) |
| POST | `/api/debug/mqtt/start` | Restart broker thread |

### 4.12 Remote File System Browsing

Used only when a remote Desktop connects to a remote Gateway (not needed for local Tauri scenarios).

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/fs/browse` | Remotely browse server file system (directory listing only, no content reading) |

### 4.13 Global Resource Snapshot (Runtime active pull entry)

> **Implementation**: [`core/acowork-gateway/src/http/global_resources_api.rs`](../../../core/acowork-gateway/src/http/global_resources_api.rs)
> **Builders**: [`core/acowork-gateway/src/mqtt/global_resources_builders.rs`](../../../core/acowork-gateway/src/mqtt/global_resources_builders.rs)
> (HTTP and MQTT retained share the same set of `build_available_*` functions, zero field‑mapping boilerplate)
> **Runtime consumption**: [`core/acowork-runtime/src/startup/global_resources_pull.rs`](../../../core/acowork-runtime/src/startup/global_resources_pull.rs)

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/global-resources` | One‑shot pull of the latest snapshot of 6 global resource topics; Runtime calls once during phase_a |

**Why this endpoint is needed (fix for Bug B)**

Bug B: after clearing `.acowork` and going through onboarding again, the first chat with the system agent reported "unexpected error", while subsequent chats worked. Root cause:

1. Gateway immediately published an empty `AvailableProviders` (`provider_count=0, api_key_lengths=[]`) right after vault unlock but before any provider was added.
2. system agent started ~150 ms later, subscribed to `acowork/global/providers`, received retained → cached empty snapshot.
3. Desktop took ~19s to complete onboarding and add a provider; Gateway republished the new snapshot with keys.
4. But rumqttd's retained delivery **only delivers new values to subscribers that have not seen the old value** — Runtime that already cached the empty snapshot would not receive the update, and the session's provider=empty became permanently fixed, requiring manual `model_switch` to recover.

**Fix**: Runtime, after mqtt client + available_cache are ready (phase_a), **actively** `GET /api/global-resources` once, decodes each topic's base64 bytes and feeds them directly to [`AvailableResourceCache::update_from_mqtt`](../../../core/acowork-runtime/src/mqtt/available_cache.rs) — **exactly the same processing path** as MQTT retained push. Version validation, stale retained rejection, and ADR-059 §5.3 generation switch logic are all reused; Gateway does not need to maintain a second update pipeline for HTTP.

**Why it does not require Runtime online**

Gateway itself is the authoritative owner of global resources (Vault + resource_cache + embed_process + BootstrapOrchestrator); this endpoint does not depend on Runtime being online. However, Gateway's own bootstrap phase (Vault unlocking / provider onboarding incomplete) means global resources are **not yet finalized** — in that case it returns `503 + Retry-After` (see error codes below), **not** `200 + empty data`: "resources not ready" and "zero resources" are two completely different semantics. An empty snapshot would cause Runtime to cache "nothing" as "there is nothing", and subsequent sessions would always start with an empty provider list. See also `GET /api/bootstrap` for the overall Gateway bootstrap state.

**Two‑channel protocol分工**

| Channel | Role | Notes |
|---|---|---|
| **MQTT retained** (`acowork/global/*`) | **primary** — real‑time incremental pushes | Connected Runtimes receive automatically; `Notify` triggers republish |
| **`GET /api/global-resources`** (this endpoint) | **active pull** — startup fallback against retained‑delivery race | Called once per startup; does not rely on retained push timing |

Both channels share the same `update_from_mqtt` processing path ([`AvailableResourceCache::update_from_mqtt`](../../../core/acowork-runtime/src/mqtt/available_cache.rs)), with zero special handling on the Runtime side.

**Response**

```json
{
  "instance_id": "instance-abc-123",
  "topics": {
    "acowork/global/providers":        "CgcKBXNrLXYx...",
    "acowork/global/mcps":             "CggKBmFkbWlu...",
    "acowork/global/embedding_models": "CggK...",
    "acowork/global/user_profile":     "CggK...",
    "acowork/global/bootstrap":        "CggK..."
  }
}
```

| Field | Type | Description |
|---|---|---|
| `instance_id` | `string` | ADR-059 §5.3 Gateway generation id, same source as `acowork/global/bootstrap` retained and `GET /api/bootstrap` |
| `topics` | `BTreeMap<string, string>` | sorted map (ensures deterministic response, easy diff testing); value is base64‑encoded `DataEnvelope` protobuf bytes, **semantically identical** to the MQTT retained topic payload of the same name |

`topics` covers 6 global resource topics:

| JSON key | MQTT topic | protobuf payload |
|---|---|---|
| `acowork/global/providers` | `acowork/global/providers` | `AvailableProviders` |
| `acowork/global/mcps` | `acowork/global/mcps` | `AvailableMcps` |
| `acowork/global/embedding_models` | `acowork/global/embedding_models` | `AvailableEmbeddingModels` |
| `acowork/global/user_profile` | `acowork/global/user_profile` | `AvailableUsers` (ADR-042) |
| `acowork/global/bootstrap` | `acowork/global/bootstrap` | `BootstrapState` (ADR-059) |

**Runtime consumption**

1. base64‑decode each `topics[k]` → bytes.
2. Call `cache.update_from_mqtt(topic, &bytes)` (the key `k` already includes the full topic name, so it goes through the same deserialization + version validation + generation switch logic as MQTT retained).
3. Compare `instance_id` with local `cache.bootstrap_instance_id()`:
   - Equal → no action.
   - Not equal → first clear all old snapshots (providers / mcps / embedding_models / lsps / user_profile / bootstrap), then apply new snapshots. `bootstrap_state`'s `update_from_mqtt` also triggers its own generation switch logic (double safeguard).

**Retry loop (Bug B fix v3)**

Runtime performs a retry loop on `503` ([`pull_global_resources_from_gateway`](../../../core/acowork-runtime/src/startup/global_resources_pull.rs)):

| Gateway phase | HTTP | `Retry-After` | Runtime behaviour |
|---|---|---|---|
| `Booting` / `Unspecified` | `503` | `2`s | sleep 2s and retry |
| `Failed` | `503` | `10`s | sleep 10s and retry |
| `ShuttingDown` | `503` | `-1` (sentinel) | **abort pull**, rely only on MQTT retained |
| `Ready` / `Degraded` | `200` | N/A | apply snapshot, loop ends |

Loop boundaries ([`global_resources_pull.rs`](../../../core/acowork-runtime/src/startup/global_resources_pull.rs)):

- Total budget `PULL_MAX_DURATION = 30s`: on timeout, abandon and Phase A does not block (still uses whatever MQTT retained delivered).
- **`503` never writes to cache** (never‑poison): existing snapshot (e.g., from MQTT retained just delivered) will not be overwritten by "not ready" data — this is a key correctness invariant.
- No hint / connection errors / 5xx (non‑503) → linear backoff (500ms initial, capped at 5s).
- 4xx (non‑503) / JSON parse failure → Fatal, abandon immediately (retry won't help).

**Why not expand the 6 protobuf structures as embedded JSON**

The types generated by prost do **not** have `serde::Serialize/Deserialize` derives by default. Expanding them as embedded JSON would require:

1. Adding `#[derive(serde::Serialize, Deserialize)]` to all messages in `mqtt_payload.proto`;
2. Resolving the subtle serde handling of `oneof payload { ... }` with `#[serde(flatten)]` / tagging;
3. Writing a bunch of conversion code like `AvailableProviders → ProviderEntry` on the Gateway side.

This is costly, error‑prone, and would make the two channels (HTTP/MQTT) wire formats **inconsistent**: future protobuf field additions would break the HTTP contract, and the Runtime would need two separate parsing paths.

Encoding protobuf bytes as base64 keeps the two channels **wire‑format identical**, with zero special handling on the Runtime side. Typical snapshot total < 5 KB, base64 overhead is negligible.

**Error codes**

| Status | Condition | `Retry-After` | Description |
|---|---|---|---|
| `200` | `BootstrapPhase::Ready` / `Degraded` | N/A | Full `GlobalResourcesView`; `topics` may be empty map (**zero resources is a valid state**) |
| `503` | `Booting` / `Unspecified` (orchestrator not attached) | `2`s | Resources not yet finalised, retry later |
| `503` | `Failed` | `10`s | Bootstrap failed, long backoff retry |
| `503` | `ShuttingDown` | `-1` (sentinel) | **Do not retry**, abandon pull |
| `401` | Bearer token missing or invalid | N/A | when `[http].auth_enabled = true` |

`503` body is uniformly `NotReadyView`: `{instance_id, phase, phase_detail, retry_after_seconds, error}` (`retry_after_seconds` same value as header, dual‑channel redundancy; client may use either).

Runtime consumption rule: `503` **does not update local cache**, backs off per `Retry-After`; `200` is always authoritative snapshot (empty `instance_id` / empty `topics` are treated as normal snapshots — empty resources are valid, not unready).

---

### 4.16 Internal service-to-service endpoints (acowork-pm → Gateway, Intent dispatch surface)

> **Implementation**: [`core/acowork-gateway/src/http/intent_api.rs`](../../../core/acowork-gateway/src/http/intent_api.rs).
> This section covers only the two endpoints acowork-pm calls; the rest of `/api/intents/*` is the
> Gateway↔Runtime / frontend chat surface whose contract is owned by Design 27 §4.5's field table and is
> not duplicated here. Auth is the internal-service boundary (same as `/api/intents/project-group`): PM
> acts as the project owner from its own domain data; the Gateway does not re-validate `owner`.

#### 4.16.1 `POST /api/intents/task-dispatch`

Dispatches a PM task into its project group chat and wakes the target agent's execution session (Design 27 §5.4.2). Request body:

| Field | Required | Description |
|---|---|---|
| `project_id` / `task_id` | yes | Task coordinates; `conversation_id` is minted by the Gateway via the `task:{project}:{task}` constant helper |
| `target_agent` | yes | Target agent instance id (ADR-073 UUID) |
| `owner` | yes | Dispatching human (project owner) — the chat row's `from` |
| `title` | no | Task title (used when ensuring the group chat) |
| `message` | yes | **Human text** (title + description): goes to the group-chat row body and the envelope's `params.message` |
| `assignment_brief` | no | **High-trust envelope field** (the claim→work→submit playbook): enters only the Runtime execution session's system block, never any chat surface; agent-sourced params carrying this key are stripped by the Gateway at the edge (ADR-102) |
| `chat_session_id` | no | Project group chat; ensured idempotently when absent |
| `message_id` | no | **Caller-supplied dispatch generation identity.** The PM scanner persists one before the first attempt and replays it on every retry → the chat row dedupes on `meta.message_id` and the mailbox budget dedupes on the same id (one charge per generation). Absent → the Gateway mints one (one row per call, no retry idempotency) |

The envelope `kind` is always stamped `pm_task` (human input, keyed routing, Chat Router bypassed). The response is always 200 + JSON (a failed delivery is not an HTTP error — PM backs off and retries on it):

```json
{ "message_id": "…", "status": "delivered | failed",
  "conversation_id": "task:…", "chat_session_id": "…",
  "error_code": "target_not_installed | not_authorized | conversation_limit_exceeded | chat_log_unavailable | …", "error": "…" }
```

#### 4.16.2 `POST /api/intents/task-close`

Ends the target agent's logical execution context for a task when it reaches a terminal state (Done / Rejected / Cancelled). Request body `{project_id, task_id, target_agent}` (any empty → 400; internal-service token required); the Gateway forwards the `conv:task:{project}:{task}` routing key to the target Runtime's `/intent-context/close`. Idempotent; an unreachable Runtime is reported, not retried (the next dispatch self-heals a dead mapping). Response `{ "status": "closed | skipped | failed", "key": "conv:task:…", "reason" | "error": … }` (`skipped` = the agent is not running, nothing to close).

#### 4.16.3 New PM task resource field (acowork-pm REST; source of truth: Design 27 §5.4.2)

`Task.dispatch_message_id: string | null` — the dispatch generation identity: persisted before the first attempt, reused across retries, cleared on state transitions (reopen / reassignment); passed through in the task JSON, read-only for clients.

---

## 5. Gateway → Runtime Reverse Proxy (requires Runtime online)

> **Implementation**: `core/acowork-gateway/src/http/proxy.rs`
> **Protocol**: Gateway looks up the Runtime's random port from `RuntimeHttpRegistry` (populated by MQTT retained payload `acowork/agents/{id}/http_port`), then HTTP reverse‑proxies. If Runtime is not registered / has exited, Gateway returns **503**.
>
> **Runtime‑side** actual interfaces see [`core/acowork-runtime/src/http/server.rs`](../../../core/acowork-runtime/src/http/server.rs) for the 25‑route inventory (ADR-034 §11.2).
>
> **Node reverse‑proxy authentication (ADR-055 Phase 5a)**: when `mqtt.auth_enabled` is enabled, Gateway, on outbound reverse‑proxy, resolves the agent's hosting Node via `installed_agents.node_id` → node registry, and automatically injects `X-ACowork-Node-Token: <node_token>` header; Node inbound validates this header (an enrolled Node must match `identity.node_token`; mismatch → 403 + `X-Error-Origin: node`). When auth is disabled, no header is sent, behaviour identical to pre‑Phase 4.

Gateway does not parse the Runtime response body; all reads and writes are verbatim passthrough. This means Runtime is the **authoritative owner of workspace config / session state**, and Gateway acts purely as a reverse proxy. (Memory is not in that list: since ADR-101 it is private data of `acowork-memory`, see [§5.4](#54-memory).)

### 5.1 Agent Runtime Configuration

| Method | Path | Purpose | Runtime path |
|---|---|---|---|
| GET | `/api/agents/{id}/config` | Read merged Agent configuration | `/agents/{id}/config` |
| PUT | `/api/agents/{id}/config` | Update Agent config (max_output_tokens, temperature, prompt, avatar…) | `/agents/{id}/config` |
| GET | `/api/agents/{id}/tools` | Read enabled built‑in tools list | `/agents/{id}/tools` |
| GET | `/api/agents/{id}/builtin-tools` | Read builtin‑tools enabled list | `/agents/{id}/builtin-tools` |
| PUT | `/api/agents/{id}/builtin-tools` | Write builtin‑tools enabled list | `/agents/{id}/builtin-tools` |
| GET | `/api/agents/{id}/status` | Runtime‑perspective status (cumulative tokens, loop state, etc.) | `/agents/{id}/status` |
| GET | `/api/agents/{id}/mcp-servers` | Read Agent's MCP service config | `/agents/{id}/mcp-servers` |
| PUT | `/api/agents/{id}/mcp-servers` | Write MCP service config | `/agents/{id}/mcp-servers` |
| GET | `/api/agents/{id}/providers` | Read Runtime‑side Provider list (actual data after MQTT sync) | `/agents/{id}/providers` |

> **ADR-040 Win11-MCP-ToolsBugFix (2026-07)**: the above `mcp-servers` / `providers` were previously stubbed by Gateway returning 200 without persistence, causing user selections in the Tools Tab MCP server to be lost. They are now uniformly reverse‑proxied to Runtime endpoints `get_agent_mcp_servers` / `put_agent_mcp_servers` etc.

### 5.2 Session Read-Only Queries

> **Session write operations (create / activate / rename / delete / close / continue) have all moved to MQTT**
> `acowork/agents/{id}/sessions/control/{cmd}`, see [§7](#7-migrated-to-mqtt-http-endpoints-removed).
> Only **read‑only** reverse proxy remains here.

| Method | Path | Purpose | Runtime path |
|---|---|---|---|
| GET | `/api/agents/{id}/sessions` | Session list (Runtime perspective, token stats merged) | `/sessions` |
| GET | `/api/agents/{id}/sessions?active=true` | **Only currently-active sessions** (ADR-038 `Active` = resident in Runtime memory; the Desktop rebuilds its tab strip from this after a restart) | `/sessions?active=true` |
| GET | `/api/agents/{id}/latest-session` | Latest session (quick startup lookup) | `/sessions/latest` |
| GET | `/api/agents/{id}/conversations/latest` | Latest session messages (the only conversations endpoint retained per ADR-034) | `/sessions/latest` |
| GET | `/api/agents/{id}/sessions/{sid}` | Single session full state (meta + state merged) | `/sessions/{sid}` |
| GET | `/api/agents/{id}/sessions/{sid}/state` | **legacy alias**: forwards to `/sessions/{sid}` (kept for backward compatibility with old callers) | `/sessions/{sid}` |
| GET | `/api/agents/{id}/sessions/{sid}/messages` | Fetch message history (cursor pagination) | `/sessions/{sid}/messages` |

> **`?active=true` (ADR-038)**: returns only the sessions **resident in Runtime memory** right now — the `Active` lifecycle set, entered by `POST .../open` and left by `POST .../close`. It is therefore the authoritative record of "which sessions are currently open", which is why the Desktop does not persist its tab list: after a restart it issues this one query and projects the backend truth onto the tab strip (the frontend projects, it does not own the state). Like the caller-scope filter, this filter runs **before** pagination, so `total_count` / `total_pages` describe the "active and readable" subset and a client can never page into rows it may not see; the two filters intersect. Omitted or `active=false` = the ordinary full history list.

### 5.3 Attachments

Attachment blobs are stored on disk under Runtime `<work_dir>/files/<document_id>` (no extension); metadata is passed via MQTT PUB `acowork/agents/{id}/sessions/control/chat_message` `attached_items` field to Runtime for writing JSONL message entries (see [mqtt.md §Session write operations](./mqtt.md) and [ADR-046](../../adr/en/ADR-046-unified-attachment-entries.md)).

| Method | Path | Purpose | Runtime path |
|---|---|---|---|
| POST | `/api/agents/{id}/sessions/{sid}/files` | Upload file (multipart) | `/sessions/{sid}/files` |
| GET | `/api/agents/{id}/files/{doc_id}` | Read blob raw bytes | `/files/{document_id}` |

#### 5.3.1 `POST /sessions/{sid}/files`

Accepts `multipart/form-data`, fields:

| Field | Required | Description |
|---|---|---|
| `file` | ✅ | Binary file content; `name` field derived from part header `filename` |
| `format` | ⬜ | Lowercase extension (no dot, e.g. `pdf` / `png`); defaults to extension from `filename` |
| `width` | ⬜ | Image pixel width (only for images); client measures via `new Image()` |
| `height` | ⬜ | Image pixel height (only for images) |

Unknown fields are ignored (forward‑compatible with future client field extensions).

**Response** `200 OK`:

```json
{
  "documentId": "a1b2c3d4…_8f7e",
  "filename": "Q3-report.pdf",
  "format": "pdf",
  "sizeBytes": 482301,
  "width": null,
  "height": null
}
```

`documentId` is content hash + random suffix (legacy algorithm), used to locate the blob under `<work_dir>/files/`. The actual blob file on disk is `<documentId>.<safe_ext>` (see above).
**Same content uploaded twice returns the same `documentId`** (deduplication semantics — only one blob on disk, and message JSONL references point to the same ID).

Error codes:

- `400`: multipart parsing failure / missing `file` field
- `503`: AttachmentService not injected (startup or service unavailable)

#### 5.3.2 `GET /files/{document_id}`

Returns blob raw bytes. `Content-Type` derivation rule: client provides lowercase extension via query param `format` (if omitted, server takes from response header `X-Format`, **fallback to `application/octet-stream`**).

#### 5.3.3 `attached_items` in message entries

Runtime receives the **typed** attachment item array pushed from the frontend via MQTT `attached_items` field. The **wire shape** corresponds to Rust's [`AttachedItem`](../../../core/acowork-core/src/protocol.rs): serde tag `type` uses snake_case (`file_upload`, `attached_selection`, etc.), **variant fields use camelCase** (`documentId`, `sizeBytes`, `absPath`, `startLine`, `endLine`). Runtime maps it to JSONL‑persisted [`AttachmentMeta`](../../../core/acowork-runtime/src/conversation.rs) in `loop_persist.rs::write_attached_items` (variant fields converted back to snake_case: `document_id`, `size_bytes`, `abs_path`, `start_line`, `end_line`).

| wire `type` | wire fields (camelCase) | JSONL fields (snake_case) | Scenario |
|---|---|---|---|
| `file_upload` | `documentId`, `filename`, `format`, `sizeBytes` | `document_id`, `filename`, `format`, `size_bytes` | User‑uploaded document (PDF/DOCX/PPTX/XLSX), blob already stored |
| `image_upload` | same + optional `width` / `height` | same + optional `width` / `height` | User‑uploaded image (PNG/JPG), blob already stored |
| `attached_file` | `absPath`, `name` | `abs_path`, `name` | "Add to Chat" workspace file (**not copied**, reference by path) |
| `attached_selection` | `absPath`, `name`, `startLine`, `endLine` | `abs_path`, `name`, `start_line`, `end_line` | "Add to Chat" with line selection |
| `attached_folder` | `absPath`, `name` | `abs_path`, `name` | "Add to Chat" entire folder (**not copied**, LLM uses its own tools to traverse) |

> **Contract lock**:
> - Desktop side sender: `apps/acowork-desktop/src/lib/types.ts::toWireAttachedItems`
> - Rust fixture regression test: `core/acowork-core/tests/attached_items_wire.rs`
>   (reads `tests/fixtures/desktop_attached_items.json`, asserts deserialization success + camelCase field names per item)
> - Desktop fixture generator: `apps/acowork-desktop/scripts/dump-attached-wire.mts`
>   (any field name change must rerun this script and update the fixture)
>
> **Important**: camelCase is a hard constraint of the Rust deserializer. snake_case field names will **not** raise any error — runtime in `gateway_loop.rs:813-820` uses `serde_json::from_value::<AttachedItem>(...).ok()` and silently discards — the result is that users see "attachments disappeared" but no error in logs. This fixture test is to prevent such regressions from reaching production.

> The latter three (`attached_*`) are constructed directly by the frontend and do not need a prior HTTP upload; only the first two (`*_upload`) require calling `POST /sessions/{sid}/files` to obtain a `documentId`.

### 5.4 Memory

> **ADR-101: memory has been split out into a separate process, `acowork-memory`**
> (supervised by the Gateway, default `127.0.0.1:18084`, one database per instance at
> `{data_dir}/instances/{instance_id}/memory.db`). The panel paths
> `/api/agents/{id}/memory/*` and the response wire shape are **byte-identical**:
> `memory_proxy` (`core/acowork-gateway/src/http/memory_proxy.rs`) turns the id in the
> path into `?instance_id=` and reverse-proxies the service's `/memory/*`. Service not
> started / restarting → Gateway answers `503 + Retry-After: 2` directly. Gateway's
> `memory_api.rs` is still an **empty router** (ADR-033): registering its paths would
> conflict with the proxy routes, causing `Router::merge()` to panic at startup.

| Method | Path | Purpose | Service path |
|---|---|---|---|
| GET | `/api/agents/{id}/memory/nodes` | List nodes (pagination + filters: `type` / `keyword` / `time_range`) | `/memory/nodes` |
| GET | `/api/agents/{id}/memory/nodes/{nid}` | Read single node | `/memory/nodes/{nid}` |
| POST | `/api/agents/{id}/memory/nodes` | Create node | `/memory/nodes` |
| PUT | `/api/agents/{id}/memory/nodes/{nid}` | Update node | `/memory/nodes/{nid}` |
| DELETE | `/api/agents/{id}/memory/nodes/{nid}` | Delete node | `/memory/nodes/{nid}` |
| GET | `/api/agents/{id}/memory/stats` | Statistics: totals, storage bytes, distribution by type/status, embedding dimensions, etc. | `/memory/stats` |
| GET | `/api/agents/{id}/memory/graph` | Full graph pull (frontend graph view) | `/memory/graph` |
| POST | `/api/agents/{id}/memory/distill` | Trigger one EpisodicDistiller pass (the former `consolidate` endpoint was folded into this one) | `/memory/distill` |
| GET | `/api/agents/{id}/memory/consolidation/status` | Consolidation / forgetting pipeline status and scheduling config | `/memory/consolidation/status` |
| PUT | `/api/agents/{id}/memory/consolidation/status` | Write scheduling config back | `/memory/consolidation/status` |
| GET | `/api/agents/{id}/memory/search` | In-instance retrieval (global search uses the short path in §5.4.1) | `/memory/search` |
| POST | `/api/agents/{id}/memory/rebuild-embeddings` | Rebuild the vector index (`scope`: `all` / `repair`, 202 background) | `/memory/rebuild-embeddings` |
| GET | `/api/agents/{id}/memory/rebuild-progress` | Rebuild progress polling | `/memory/rebuild-progress` |

> The panel's "Rebuild Index" button does **not** call `rebuild-embeddings` directly: it
> goes through Gateway `POST /api/embedding-models/{id}/start-migration` (§4.7), which
> forwards to that instance's service endpoint and writes progress back into the
> `migration-progress` snapshot.
>
> This section is the **only non-Runtime backend** inside §5 (Gateway → Runtime reverse
> proxy): since ADR-101 storage and panel data for memory live on the service side, all
> other subsections are still Runtime reverse proxies.

RAG is not part of the memory surface and stays with the Runtime:

| Method | Path | Purpose | Runtime path |
|---|---|---|---|
| GET | `/api/agents/{id}/rag/status` | RAG configuration status (whether configured, provider name) | `/agents/{id}/rag/status` |
| POST | `/api/agents/{id}/rag/query` | Direct RAG query (bypasses LLM, for debugging/connectivity verification) | `/agents/{id}/rag/query` |

#### 5.4.1 Service-direct prefix `/api/memory/*` (ADR-101)

| Method | Path | Caller | Service path |
|---|---|---|---|
| GET | `/api/memory/search?instance_id={id}` | Desktop global search memory tab | `/memory/search` |
| POST | `/api/memory/mcp` | Agent Runtime (MCP tool calls) | `/mcp` |
| POST | `/api/memory/admin/bootstrap?instance_id={id}` | Agent Runtime first-start bootstrap (write entry D) | `/admin/bootstrap` |
| POST | `/api/memory/admin/import?instance_id={id}` | **Node only**: one-shot legacy-store handover (ADR-101 decision 10) | `/admin/import` |

Mapping and gating rules:

- `mcp` and `admin/*` are the service's own namespaces; every other short path lands on
  `/memory/{rest}`.
- Non-MCP calls **MUST** carry `instance_id` (missing → `400`); reads (`GET`/`HEAD`) are
  gated at `Op::Use`, writes at `Op::Manage` (a machine actor has no `AuthContext` →
  passes through, ADR-087).
- `X-MCP-Actor` is injected only after the Gateway has validated the install table;
  `X-ACowork-Node-Token` stops at the Gateway — it **never crosses the reverse proxy**.
- The `/admin/import` request body is raw SQLite bytes
  (`Content-Type: application/x-sqlite3`) capped at 64 MiB (equal to Gateway's
  `GLOBAL_BODY_LIMIT`); the response is
  `{imported_nodes, imported_vectors, imported_lexical_rows, imported_purge_log_rows, skipped_existing}`.
  A namespace that already holds memories is **not merged into** — only
  `skipped_existing: true` is returned.

### 5.5 Workspace

> **Workspace config is owned by Runtime** (`<work_dir>/config/agent_workspaces.json`).
> Gateway acts as a thin reverse proxy, concentrating `workspace_id` resolution and path‑traversal guards in Runtime (ADR-040). **Only static file serving (see §6) remains with Gateway direct return**, because HTML preview iframes need raw bytes — the Runtime JSON envelope cannot be used.

| Method | Path | Purpose | Runtime path |
|---|---|---|---|
| GET | `/api/agents/{id}/workspaces` | List workspaces | `/workspaces` |
| POST | `/api/agents/{id}/workspaces` | Add workspace directory | `/workspaces` |
| GET | `/api/agents/{id}/workspaces/tree` | Directory tree | `/workspaces/tree` |
| GET | `/api/agents/{id}/workspaces/find` | Find file by name | `/workspaces/find` |
| GET | `/api/agents/{id}/workspaces/search` | Content search (`include`, `max_results`, `case_sensitive`, `whole_word`, `match_file_names`) | `/workspaces/search` |
| PUT | `/api/agents/{id}/workspaces/{ws_id}` | Update workspace (alias, access, etc.) | `/workspaces/{ws_id}` |
| DELETE | `/api/agents/{id}/workspaces/{ws_id}` | Delete workspace | `/workspaces/{ws_id}` |
| PUT | `/api/agents/{id}/workspaces/{ws_id}/prompt-file` | Set injection prompt file | `/workspaces/{ws_id}/prompt-file` |
| GET | `/api/agents/{id}/workspaces/file` | Read file (with metadata) | `/workspaces/file` |
| POST | `/api/agents/{id}/workspaces/file` | Create file | `/workspaces/file` |
| PUT | `/api/agents/{id}/workspaces/file` | Write file | `/workspaces/file` |
| DELETE | `/api/agents/{id}/workspaces/file` | Delete file | `/workspaces/file` |
| POST | `/api/agents/{id}/workspaces/dir` | Create directory | `/workspaces/dir` |
| DELETE | `/api/agents/{id}/workspaces/dir` | Delete directory | `/workspaces/dir` |
| POST | `/api/agents/{id}/workspaces/copy` | Copy file/directory | `/workspaces/copy` |
| POST | `/api/agents/{id}/workspaces/rename` | Atomic rename file/dir | `/workspaces/rename` |

---

## 6. Static File Service (direct streaming of raw bytes)

> **Implementation**: `core/acowork-gateway/src/http/workspaces.rs::resolve_tree_path`
> **Reason to remain in Gateway**: HTML preview iframes require raw bytes (HTML / CSS / binary images) — the Runtime JSON envelope (base64 content + metadata) would break `<img>` / `<link>` / `<script>` tags. Path‑traversal guards (canonicalize + `..` checks) are performed locally by Gateway.

| Method | Path | Purpose |
|---|---|---|
| GET | `/workspace-files/{agent_id}/{workspace_id}/{*path}` | Direct link to any workspace file, resolves absolute path via `workspace_id` |
| GET | `/ws-files/{agent_id}/{*path}` | Direct link to Agent home (no workspace_id) |

> The two path names are historical and remain unchanged.

---

## 7. Migrated to MQTT (HTTP endpoints removed)

> **ADR-033 + ADR-034**: the following interactions now use MQTT; **HTTP no longer provides corresponding endpoints**.
> Registering these paths at the Gateway HTTP layer would conflict with `proxy_routes` / `chat_routes` and panic at startup.
> Callers should subscribe / publish to the corresponding MQTT topics. Full protocol details in [mqtt.md](./mqtt.md).

| Removed HTTP endpoint | Replacement MQTT channel | Notes |
|---|---|---|
| `POST /api/agents/{id}/message` | `PUB acowork/agents/{id}/sessions/control/chat_message` | Send message (includes message_id, content, session_id, command, attached_items, content_parts) — ADR-046 removed `document_ids` / `attached_context`; unified via `attached_items` (wire fields camelCase, see §5.3.3) |
| `GET /api/agents/{id}/stream` | `SUB acowork/agents/{id}/sessions/{sid}/messages/#` (or `chat/stream/{session_id}`) | Streaming chat events: chunk / tool_call / done / approval_needed / question_pending |
| `POST /api/agents/{id}/sessions/{sid}/activate` | `PUB sessions/control/open_session` | ADR-038: Closed / NotFound → Active |
| `PUT /api/agents/{id}/sessions/{sid}/title` | `PUB sessions/control/update_title` | |
| `DELETE /api/agents/{id}/sessions/{sid}` | `PUB sessions/control/delete_session` | |
| `POST /api/agents/{id}/sessions/{sid}/close` | `PUB sessions/control/close_session` | Triggers distillation, retains JSONL |
| `POST /api/agents/{id}/continue` | `PUB sessions/control/continue_execution` | Resume after pause (e.g., iteration_limit) |
| `POST /api/agents/{id}/approval` | `PUB inbound` ApprovalDecision | User allow/deny for tool calls |
| `POST /api/agents/{id}/question` | `PUB inbound` QuestionAnswer | User answers to `ask_user_question` prompts |
| `POST /api/agents/{id}/model-switch` (if any) | `PUB sessions/control/model_switch` | Switch model + optional provider |

**Runtime‑side handling entry**: all the above commands are deserialised and dispatched via the `ControlAction` enum in [`core/acowork-runtime/src/mqtt/control_handler.rs`](../../../core/acowork-runtime/src/mqtt/control_handler.rs).

---

## 8. Common Error Codes

| Status | Scenario |
|---|---|
| 400 | Parameter validation failure, content too long, invalid id format |
| 401 | Bearer token missing or invalid |
| 404 | Agent / resource not found |
| 409 | State conflict: Agent not running, not installed |
| 500 | Gateway internal error |
| 502 / 503 | MQTT / reverse proxy channel unavailable, Runtime disconnected (**proxy‑endpoint specific**) |
| 504 | Gateway → Runtime request timeout |

---

## 9. Typical Request Examples

### 9.1 Install Agent (Gateway native)

```http
POST /api/agents/install HTTP/1.1
Authorization: Bearer <token>
Content-Type: multipart/form-data; boundary=----abc

------abc
Content-Disposition: form-data; name="package"; filename="hello.agent"
Content-Type: application/octet-stream

<binary>
------abc--
```

### 9.2 Start Agent and send message (MQTT)

```http
POST /api/agents/{id}/start HTTP/1.1
Authorization: Bearer <token>
```

```text
# Legacy HTTP POST /api/agents/{id}/message removed, use MQTT:
PUB acowork/agents/{id}/sessions/control/chat_message
{
  "session_id": "sess-active",
  "message_id": "msg-11111111",
  "content": "Hello",
  "params_json": "{\"document_ids\":[]}"
}
```

Response (on SUB side `chat/stream/{session_id}`):

```json
{ "message_id": "msg-11111111", "type": "chunk", "text": "..." }
{ "type": "done" }
```

### 9.3 Upload attachment + reference in message (HTTP → MQTT)

```http
POST /api/agents/com.acowork.senior-engineer/sessions/sess-active/files HTTP/1.1
Authorization: Bearer <token>
Content-Type: multipart/form-data; boundary=----abc

------abc
Content-Disposition: form-data; name="file"; filename="Q3-report.pdf"
Content-Type: application/pdf

<binary>
------abc
Content-Disposition: form-data; name="format"

pdf
------abc--
```

Response:

```json
{
  "documentId": "a1b2c3d4…_8f7e",
  "filename": "Q3-report.pdf",
  "format": "pdf",
  "sizeBytes": 482301
}
```

Then reference it via `params_json.attached_items` (the **5 types** see §5.3.3) when sending MQTT message:

```text
PUB acowork/agents/com.acowork.senior-engineer/sessions/control/chat_message
{
  "session_id": "sess-active",
  "message_id": "msg-22222222",
  "content": "Summarise this Q3 report",
  "params_json": "{\"attached_items\":[{\"type\":\"file_upload\",\"documentId\":\"a1b2c3d4…_8f7e\",\"filename\":\"Q3-report.pdf\",\"format\":\"pdf\",\"sizeBytes\":482301}]}"
}
```

> `attached_items` array wire field names are **camelCase** (`documentId`, `sizeBytes`, `absPath`, `startLine`, `endLine`), matching the Rust `AttachedItem` deserialiser. Full field details in [§5.3.3](#533-attached_items-in-message-entries).

### 9.4 Query Memory (reverse proxy → memory service)

```http
GET /api/agents/com.acowork.senior-engineer/memory/nodes?page=1&size=20&type=Episodic&time_range=7d HTTP/1.1
Authorization: Bearer <token>
```

`memory_proxy` handles this on the Gateway (ADR-101): the instance id in the path is the
sole coordinate — the proxy writes it as `?instance_id=` (a caller-supplied
`instance_id` is **dropped, never merged**, so nobody can address a namespace they were
not authorized for) → reverse-proxies
`http://127.0.0.1:{memory_port}/memory/nodes?...` (default `18084`) → verbatim returns.

When the service process does not exist (not started / restarting) the Gateway answers
`503` + `Retry-After: 2` without consulting any port — the same contract as
`/api/doc/*`, `/api/pm/*` and `/api/user/*`.

### 9.5 Query consolidation status (reverse proxy → memory service)

```http
GET /api/agents/com.acowork.senior-engineer/memory/consolidation/status HTTP/1.1
Authorization: Bearer <token>
```

Response (the Desktop "Memory distiller" / "Memory forgetting" cards render this envelope directly):

```json
{
  "idle_secs": 42,
  "bg_task_running": true,
  "distiller": {
    "enabled": true,
    "episode_backlog": 3,
    "secs_since_distill": 120,
    "interval_secs": 3600,
    "accumulation_threshold": 50,
    "idle_secs": 1800,
    "models": [{ "provider_id": "openai", "model_id": "gpt-4o-mini" }],
    "last_run": {
      "at": "2026-10-23T02:11:07+00:00",
      "episodes_scanned": 12,
      "total_promoted": 7,
      "facts_promoted": 4,
      "preferences_promoted": 1,
      "relations_promoted": 1,
      "procedures_promoted": 1,
      "autobio_promoted": 0,
      "episodes_marked_consolidated": 9,
      "error": null,
      "funnel": { "scanned": 12, "backlog_remaining": 0, "llm_calls": 3 }
    }
  },
  "forgetting": {
    "enabled": true,
    "half_life_days": 30,
    "dormant_threshold": 0.15,
    "archive_days": 180,
    "interval_secs": 86400
  }
}
```

`distiller.last_run` is `null` until the instance has distilled once; `distiller.models`
is the candidate chain the panel's model picker reads and writes (**additive** to the
Runtime response shape, which had no model field). `PUT` on the same path writes the
scheduling config back: body `{"distiller": {...}, "forgetting": {...}}`, omitted fields
keep their current values, unknown fields are rejected with `400`
(`deny_unknown_fields`).

### 9.5.1 Trigger memory distillation (reverse proxy → memory service)

```http
POST /api/agents/com.acowork.senior-engineer/memory/distill HTTP/1.1
Authorization: Bearer <token>
```

No request body (`?instance_id=` is injected by the proxy). Response:

```json
{
  "started": true,
  "episodes_scanned": 12,
  "facts_promoted": 4,
  "preferences_promoted": 1,
  "relations_promoted": 1,
  "procedures_promoted": 1,
  "autobio_promoted": 0,
  "episodes_marked_consolidated": 9,
  "funnel": {
    "scanned": 12, "backlog_remaining": 0, "llm_calls": 3, "llm_calls_failed": 0,
    "llm_calls_truncated": 1, "below_importance": 2, "candidates_recalled": 9,
    "projected": 8, "verdict_no_merge": 3, "verdict_merged": 4,
    "verdict_superseded": 1, "episodes_consolidated": 9, "episodes_deferred": 2,
    "errors": 0
  }
}
```

Field description:

| Field | Type | Description |
|-------|------|-------------|
| `started` | `bool` | Whether the pass completed (always `true`; failures leave via the status codes below) |
| `episodes_scanned` | `usize` | Pending episodes read in this pass |
| `facts_promoted` / `preferences_promoted` / `relations_promoted` / `procedures_promoted` / `autobio_promoted` | `usize` | Nodes newly written (including merged results) per memory type |
| `episodes_marked_consolidated` | `usize` | Episodes marked consolidated, so they leave the backlog |
| `funnel` | `object` | Funnel counters: where the rest of the 12 scanned episodes went |

Status codes: `200` run completed (an empty backlog is still a completed run) · `409` run
not possible (memory provider not initialized, embedding provider not initialized, or the
distiller is disabled) · `500` run failed · `503` `distiller_not_available` (the pipeline
produced no result at all).

> **ADR-101 change**: the Runtime's former `POST …/memory/consolidate`
> (`{force, retention_days}` → `{started, duration_ms, episodes_consolidated,
> knowledge_nodes_generated, message}`) no longer exists; manual triggers go through
> `distill` and run the same complete EpisodicDistiller pipeline as the background loop
> (LLM extraction and merge verdicts included). There is no "basic policy only" branch.

### 9.5.2 Legacy memory store handover (Node only → memory service)

ADR-101 decision 10: the `{workspace}/memory/private.sqlite` that every Agent workspace
held before ADR-101 is handed to the service **unilaterally by `acowork-node`** — Node
owns the node-local filesystem (ADR-055), and neither the Runtime nor the Gateway takes
part.

```http
POST /api/memory/admin/import?instance_id=21212121-2121-4121-8121-212121212121 HTTP/1.1
X-ACowork-Node-Token: <node token>
Content-Type: application/x-sqlite3
Content-Length: 1048576

<raw private.sqlite bytes (WAL already checkpointed)>
```

Response:

```json
{
  "imported_nodes": 2,
  "imported_vectors": 1,
  "imported_lexical_rows": 2,
  "imported_purge_log_rows": 0,
  "skipped_existing": false
}
```

Contract points:

- Exactly two trigger points: the background pass after Node starts, and
  `acowork-node migrate-memory`. Each migrated instance gets a
  `{workspace}/memory/.migrated` sentinel, which later passes skip.
- The body is **raw bytes** (no multipart), capped at 64 MiB =
  `acowork_core::workspace::STORE_IMPORT_MAX_BYTES`, deliberately equal to the Gateway's
  `GLOBAL_BODY_LIMIT`; a bigger store is refused by Node before uploading, with an
  operator instruction instead of streaming 64 MiB into a 413.
- `X-ACowork-Node-Token` stops at the Gateway and **never crosses the reverse proxy**;
  a machine actor carries no `AuthContext`, so the write-tier `Op::Manage` ACL passes it
  through (ADR-087).
- A namespace that **already has memories is not merged**: `skipped_existing: true`, zero
  rows written to all four tables, and Node then **deletes nothing** — the legacy file
  stays where it was and the handover can be retried.
- Only after a real import (`skipped_existing: false`) does Node DELETE the memory rows
  from the local legacy store; the file itself is kept for the Runtime (sessions /
  ConversationMessage still live in it, ADR-082).
- Not a SQLite file (magic mismatch) → `400`; missing `instance_id` → `400`; service not
  ready → `503` (Node retries with 6 rounds × 5s backoff).

### 9.6 Query RAG status (reverse proxy)

```http
GET /api/agents/{id}/rag/status HTTP/1.1
Authorization: Bearer <token>
```

Response (RAG configured):

```json
{
  "configured": true,
  "provider_name": "enterprise_knowledge",
  "agent_id": "com.example.sales"
}
```

Response (RAG not configured):

```json
{
  "configured": false,
  "provider_name": null,
  "agent_id": "com.example.sales"
}
```

### 9.7 Direct RAG query (reverse proxy)

Bypasses the LLM tool‑call path and directly queries the RAG service. Used for debugging RAG connectivity and query quality.

```http
POST /api/agents/{id}/rag/query HTTP/1.1
Authorization: Bearer <token>
Content-Type: application/json

{
  "query": "Product Q3 roadmap",
  "top_k": 5,
  "score_threshold": 0.7
}
```

Response:

```json
{
  "query": "Product Q3 roadmap",
  "results": [
    {
      "content": "Q3 roadmap includes three milestones...",
      "source_url": "https://wiki.corp.example.com/q3-roadmap",
      "chunk_id": "chunk-abc123",
      "score": 0.92,
      "source_label": "[RAG:enterprise_knowledge]"
    }
  ],
  "result_count": 1,
  "provider_name": "enterprise_knowledge"
}
```

Returns `503` if RAG is not configured; `400` for empty query.

### 9.8 Fetch message history (reverse proxy)

```http
GET /api/agents/{id}/sessions/sess-active/messages?cursor=...&limit=50 HTTP/1.1
Authorization: Bearer <token>
```

### 9.9 Static file direct link (iframe / img)

```html
<!-- Workspace arbitrary file, resolved by workspace_id -->
<img src="http://127.0.0.1:19876/workspace-files/com.acowork.senior-engineer/ws-1/assets/logo.svg">

<!-- Agent home direct link -->
<img src="http://127.0.0.1:19876/ws-files/com.acowork.senior-engineer/assets/avatar.png">
```

### 9.10 Pull global resource snapshot (Runtime startup active pull)

```http
GET /api/global-resources HTTP/1.1
Authorization: Bearer <token>
```

Response (example, structure shown, not actual length):

```json
{
  "instance_id": "instance-7f3a9b2e-1c4d-4e8f-9a5b-2d6f8e0c1234",
  "topics": {
    "acowork/global/providers":        "CgcKBXNrLXYxGgIIUg==",
    "acowork/global/mcps":             "CggKBmFkbWluGgIIUg==",
    "acowork/global/embedding_models": "CggK...",
    "acowork/global/user_profile":     "CggK...",
    "acowork/global/bootstrap":        "CggK..."
  }
}
```

Each `topics[k]` value is base64‑encoded `DataEnvelope` protobuf bytes, **semantically identical** to the MQTT retained topic payload of the same name. Runtime processing:

```rust
// pseudo‑code (actual in core/acowork-runtime/src/startup/global_resources_pull.rs)
for (topic, b64) in &body["topics"].as_object().unwrap() {
    let bytes = base64::decode(b64.as_str().unwrap()).unwrap();
    cache.update_from_mqtt(topic, &bytes);   // same entry as MQTT retained
}
if body["instance_id"] != cache.bootstrap_instance_id().unwrap_or("") {
    cache.providers = None;   // ADR-059 §5.3 generation switch
    cache.mcps = None;
    // ... apply new snapshots
}
```

---

## 10. Notes

1. **Gateway does not persist business data**: Skill, Agent runtime config, Session state, etc. are stored in Runtime local files / SQLite, and memory in the separate `acowork-memory` process's per-instance database (ADR-101); Gateway pulls snapshots or passthrough via HTTP reverse proxy.
2. **Proxy endpoints require the backend online**: if Runtime is not registered / has exited, returns 503; the MQTT channel `acowork/agents/{id}/http_port` is the **sole source** for Gateway to discover the Runtime port — **retained publish** is critical (after Gateway restart, broker replays the last port). The memory surface (§5.4 / §5.4.1) is the only exception: its backend is the `acowork-memory` process the Gateway supervisor starts itself (port read from the port-file, not from MQTT), so a down Runtime does not affect it.
3. **Most writes trigger hot pushes**: for example, after modifying Provider / MCP config, Gateway synchronises the latest available list to all connected Runtimes via MQTT **retained publish**; see [mqtt.md §Global resource availability broadcast](./mqtt.md).
4. **CORS**: always enabled as `CorsLayer::permissive()` (any origin, any method, any header; **without** `allow_credentials(true)` — `*` wildcard conflicts with `Access-Control-Allow-Credentials: true`, tower-http panics at build; also frontend `fetch` defaults to `credentials: 'same-origin'`, so this header is unnecessary). Dev mode Vite (`:5173`) and production Tauri custom protocol (`tauri://localhost` / `http(s)://tauri.localhost`) both cross‑origin access Gateway (`:19876`); any hardcoded allowlist would be broken when browsers resolve `localhost` to different IP literals. Local bind defaults to `127.0.0.1` — an attacker would already need access to the loopback to exploit permissive CORS, so it's zero‑risk on loopback. For remote deployments, CSRF protection relies on `Authorization: Bearer <token>` (`[http].auth_enabled = true`); Gateway does not send Set‑Cookie, and browsers by default do not send cookies with `credentials: 'same-origin'`.
5. **Static file service**: `/workspace-files` and `/ws-files` paths are served directly by the Axum router as file streams, for frontend `<img>` / video direct references (historical names remain unchanged).
6. **Session write operations have all moved to MQTT** (see §7): do not attempt HTTP `POST /message` / `/activate` / `/continue` etc. — these paths **do not exist** at the Gateway HTTP layer; calls will return 404.
7. **Runtime active pull of global resources on startup**: every Runtime startup, after phase_a (mqtt client + available_cache ready), actively `GET /api/global-resources` once. This is a **fallback against retained‑delivery race**, in addition to MQTT retained push, to fix Bug B (first chat after clearing `.acowork` gave "unexpected error"). The HTTP endpoint does not depend on Runtime being online and belongs to §4 Gateway native endpoints; Runtime consumption reuses the same `update_from_mqtt` entry as MQTT retained pushes, with version validation and ADR-059 §5.3 generation switch logic zero‑divergence. See [§4.13](#413-global-resource-snapshot-runtime-active-pull-entry).
