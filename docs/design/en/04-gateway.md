# Gateway Component Detailed Design

> Version: v3.2 | Last Updated: 2026-07-12
> Major revision in this version: §9 IPC section fully aligned with [ADR-033](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) — Gateway ↔ Runtime channel replaced with **MQTT pub/sub + HTTP reverse proxy**. HTTP API still keeps REST interfaces; streaming events changed from WebSocket to MQTT topic subscription.

---

Gateway is a resident system-level process implemented in Rust. Gateway **does not proxy Agent business logic** (does not proxy LLM calls, does not proxy tool execution); it only handles coordination work that must be centralized.

Gateway serves two consumer categories simultaneously:

```
┌──────────────────┐         ┌──────────────────┐
│  Agent Runtime   │         │  Desktop App     │
│  (multiple procs)│         │  / CLI           │
└────────┬─────────┘         └────────┬─────────┘
         │ MQTT + HTTP reverse proxy │ HTTP API
         │ (MQTT pub/sub + proxy)    │ (REST)
         ▼                            ▼
┌────────────────────────────────────────────────┐
│                Gateway (single process)         │
│                                                │
│  ┌─────────────┐  ┌──────────┐  ┌──────────┐  │
│  │ Package Mgr │  │ Lifecycle│  │ Intent   │  │
│  │             │  │ Manager  │  │ Router   │  │
│  ├─────────────┤  ├──────────┤  ├──────────┤  │
│  │ Key Vault   │  │ Budget   │  │ Rate     │  │
│  │             │  │ Tracker  │  │ Limiter  │  │
│  └─────────────┘  └──────────┘  └──────────┘  │
└────────────────────────────────────────────────┘
```

- **MQTT + HTTP reverse proxy**: IPC channel for Agent Runtime — MQTT carries real-time event push (chat chunk, tool_call, done, device status Will+Retained), HTTP reverse proxy carries large data queries (history messages, session queries), Runtime itself does not expose external ports
- **HTTP API**: REST interface for Desktop App / CLI (Axum, localhost only)

Both share Gateway's internal state, just different access layers.

## 1. Package Manager

- **Install**: Extract `.agent` to `~/.local/share/agent-gateway/agents/<agent_id>/`, verify manifest integrity, record version. Must verify package signature before installation (see [02-agent-package.md](./02-agent-package.md)); if signature is invalid or doesn't match the installed version's signature, reject installation.
- **Uninstall**: Delete the corresponding directory, optionally back up user data (including private SQLite memory layer).
- **Upgrade**: Preserve `data/` and user-modified `config/`, replace other files. If `runtime_version` is incompatible, prompt the user. On upgrade, verify the new package's signing certificate fingerprint matches the installed version.
- **Repository support**: Configurable multiple HTTP repository sources (like apt), periodically check for updates. Repository-provided .agent packages must be signed.

## 2. Lifecycle Manager

**Startup strategy:**
- On-demand: Start when receiving messages matching triggers or user explicitly invokes.
- Persistent: User can mark an Agent to auto-start on boot.
- Scheduled: Triggered by cron expression. (**Phase 3 implementation**, Phase 2 supports on-demand and persistent only)

**Process management:**
- Use `std::process::Command` to create child processes, set independent working directory, environment variables.
- Startup parameter injection: Agent package path, Gateway MQTT broker address (`127.0.0.1:19875`), Agent ID, workspace path.
- **API Key distribution**: Agent Runtime obtains Key from Gateway via MQTT handshake (`AgentHello` message, see [ADR-034](../../adr/zh/ADR-034-mqtt-http-boundary.md), not passed via environment variables to prevent ps leakage.
- Health check: If Agent process exits, decide whether to auto-restart based on exit code (configurable).

**Sleep and wake:**
- Use kill-restart strategy: After idle timeout, directly kill Agent Runtime process, re-spawn when needed next time.
- Agent state persists via private SQLite, restores context from Memory on startup.
- Do not use SIGSTOP/SIGCONT (Windows doesn't support, process still occupies memory, state serialization difficult).
- Agent can declare `startup_timeout_ms` in manifest; Gateway uses this to decide whether to pre-warm (start ahead of time).

## 3. Intent Router

**Input sources:**
- Requests from user interface (CLI/GUI).
- Scheduled task triggers.
- Intent messages forwarded by other Agents via Gateway (see [06-communication.md](./06-communication.md)).

**Routing rules:**
- Route directly to target Agent based on `target` field in message.
- If target Agent is not running, start it on-demand.
- If `target` not specified, match installed Agents' manifest `triggers.message.pattern`.

## 4. Sandbox Configurator

> **2026-04-25 Decision (ADR-007)**: Process-level sandbox (bubblewrap / AppContainer / Seatbelt) deferred to Phase 7. For Phases 3~6, when Gateway launches Agent Runtime, only **policy-level isolation** is used (permission checks + path allow-list), without OS-level mandatory isolation. The full design is preserved below for Phase 7 reference.

Gateway configures sandbox parameters based on manifest when launching Agent Runtime, after which OS layer executes isolation. Implementation differs per platform, but isolation goals are consistent.

**Cross-platform Isolation Strategy Comparison:**

| Isolation Dimension | Linux | Windows | macOS | Android | iOS |
|---------------------|-------|---------|-------|---------|-----|
| **Process Model** | Spawn independent process | Spawn independent process | Spawn independent process | Single-process multi-thread / Service | Single-process multi-thread / Extension |
| **File Isolation** | bubblewrap `--bind` (Phase 7) | Restricted token + ACL (Phase 7) | App Sandbox (Phase 7) | System sandbox | System sandbox |
| **Network Isolation** | `--unshare-net` (Phase 7) | Firewall API (Phase 7) | Network Extension (Phase 7) | System sandbox fallback | System sandbox fallback |
| **System Call Restrictions** | seccomp-bpf (Phase 7) | None (Job Object) | sandbox-exec (Phase 7) | System-level | System-level |
| **Resource Limits** | cgroups / rlimit (Phase 7) | Job Object limits (Phase 7) | rlimit | System-level | System-level |
| **WASM Engine** | Wasmtime (JIT) | Wasmtime (JIT) | Wasmtime (JIT) | wasmi (interpreter) | wasmi (interpreter, iOS forbids JIT) |

> WASM engine selection details see [12-tool-system.md](./12-tool-system.md) §3.1.

| **Data Directory** | XDG (`~/.local/share/`) | `%APPDATA%\AgentGateway\` | `~/Library/Application Support/AgentGateway/` | `context.getFilesDir()` | appSupportDir |

**Phase 3~6 Policy-Level Isolation**: Gateway launches Agent Runtime via bare `Command::new`, relying on Runtime-side PermissionChecker for path allow-list and network permission checks.

**Phase 7 Process-Level Sandbox Implementation Example (for reference):**
```bash
bwrap \
    --ro-bind /usr /usr \
    --ro-bind /lib /lib \
    --ro-bind /bin /bin \
    --ro-bind /usr/lib/agent-gateway/agent-runtime /app \
    --bind <agent_workspace> /workspace \
    --dev /dev \
    --proc /proc \
    --unshare-net \              # Default network denial (whitelist by manifest when needed)
    --die-with-parent \
    agent-runtime /workspace/agent-package --socket /tmp/gateway.sock
```

**Windows (Phase 7):**
- `CreateRestrictedToken` + Job Object + filesystem ACL

**macOS (Phase 7):**
- `sandbox-exec` config file

## 5. Key Vault

Centralized management of all LLM API Keys, encrypted storage:

```
~/.config/agent-gateway/vault/
├── openai_key.enc
├── anthropic_key.enc
└── vault.key               # Master key, derived from user password
```

- Agent manifest references Key with `vault:openai_key`, not stored plaintext.
- Agent Runtime fetches Key via MQTT handshake after startup (one-time transfer, not via environment variables).
- Key stored zero-copy/sealed on Rust side (using `secrecy::SecretString`), LLM Client directly uses this Secret to sign requests; WASM plugin layer absolutely has no API to read this string.

## 6. Budget Tracker

Receives LLM usage reports from Agent Runtime, maintains cross-Agent statistics:

- Each Agent has independent daily/monthly Token and cost limits.
- On overrun, send signal to Agent (stop / fallback / warn).
- Provides budget query interface for Agent local pre-check.

## 7. Rate Limiter

Coordinates concurrent requests from multiple Agents to the same LLM Provider, avoiding triggering API Rate Limits:

- Before Agent calls LLM, apply for rate token via Gateway (extremely lightweight RPC, < 0.1ms).
- Gateway allocates tokens based on Provider's RPM/TPM limits.

## 8. Configuration and Data Storage

- **Gateway's own configuration**: `~/.config/agent-gateway/config.toml` (containing Vault config, repository list, default LLM config, etc.).
- **Each Agent's workspace**: `~/.local/share/agent-gateway/agents/<agent_id>/workspace/`:
  - `data/`: Copied from package, read-write.
  - `config/`: User-modifiable configuration (initially from package config).
  - `memory/`: Workspace store SQLite file (`private.sqlite`) — carries the session meta and the conversation index (ADR-082). Memory nodes live in the standalone memory service (`$HOME/.acowork/acowork-memory/instances/{instance_id}/memory.db`, ADR-101).
  - `runtime/`: Temporary files (pid, HTTP port).
- **Logs**: Gateway collects stdout/stderr from all Agents, writes to `~/.local/share/agent-gateway/logs/`, supports filtering by Agent.

## 9. HTTP API (Desktop App / CLI Access Layer)

> **Current implementation status**: HTTP API is fully implemented based on Axum, providing REST interfaces; streaming events go through MQTT (see §9.1 dual-protocol architecture and [`docs/protocols/zh/README.md`](../../protocols/zh/README.md)). The following interface definitions reflect actual implementation, but for documentation conciseness, only core routes are listed. Complete route list see code `crates/acowork-gateway/src/http/routes.rs`.

The IPC between Gateway and Agent Runtime, starting from [ADR-033](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md), is unified as **MQTT pub/sub + HTTP reverse proxy**: MQTT handles real-time event push (chat chunk, tool_call, done, device status Will+Retained), HTTP reverse proxy handles large data queries and session history pulls. HTTP API as the unified access layer for Desktop App and CLI, invisible to Runtime.

### 9.1 Why Dual Protocol (MQTT + HTTP)

| Dimension | MQTT | HTTP Reverse Proxy |
|-----------|------|---------------------|
| Consumer | Agent Runtime (Gateway ↔ Runtime bidirectional) | Gateway → Runtime (on-demand forwarding) |
| Listener | Gateway embedded rumqttd broker (`127.0.0.1:19875`) | Gateway HTTP reverse proxy (`127.0.0.1:19876`) |
| Communication Mode | Topic pub/sub (many-to-many, multiplexed) | Request/response (point-to-point) |
| Frame Format | MQTT Payload (independent protobuf namespace, see [`core/acowork-core/proto/mqtt_payload.proto`](../../../core/acowork-core/proto/mqtt_payload.proto)) | Standard HTTP/JSON |
| Authentication | localhost only (rely on local loopback protection) | localhost only + optional Bearer Token |
| Use Cases | Real-time event push, state sync, Key distribution (AgentHello), Intent triggering, Will+Retained lifecycle | Session history, message queries, config writeback, large payload forwarding |

Both share Gateway's internal logic (Package Manager, Lifecycle Manager etc.), just with different carrying protocols. MQTT does not carry req/res — any "wait for reply" scenario goes through HTTP reverse proxy, which Gateway internally converts to Runtime localhost HTTP call (Runtime itself doesn't expose external ports).

### 9.2 HTTP Server Configuration

```rust
// When Gateway process starts, two ports are simultaneously opened:
// 1. MQTT broker (embedded rumqttd, for Agent Runtime + Desktop App)
// 2. HTTP API (Axum, for Desktop App / CLI; also serves Gateway -> Runtime reverse proxy)

pub struct HttpConfig {
    /// Listen address, default 127.0.0.1
    pub host: String,
    /// Listen port, default 19876
    pub port: u16,
    /// Whether to enable authentication (Bearer Token), default false
    pub auth_enabled: bool,
}
```

> **CORS is always permissive** — all deployment scenarios use `CorsLayer::permissive()`, no longer keeping
> `[http].cors_enabled` config item. Reason: dev mode Vite (`:5173`) and production Tauri custom protocol
> (`tauri://localhost`) are both cross-origin accesses to Gateway (`:19876`), and a hardcoded allowlist will fail
> when the browser resolves `localhost` to different IP literals; locally bound to `127.0.0.1` by default, permissive
> CORS has no additional risk on loopback. CSRF protection in remote scenarios relies on
> `Authorization: Bearer <token>` (controlled by `auth_enabled`), not CORS.

- Default listen `http://127.0.0.1:19876`, localhost only, not exposed externally
- Port configurable in `config.toml`
- On port conflict, auto-increment tries (19876 → 19877 → 19878...), final port written to pidfile for Desktop App discovery

### 9.3 Route Definitions

```rust
use axum::{Router, routing::{get, post, put, delete}, extract::WebSocketUpgrade};

pub fn http_routes() -> Router<GatewayState> {
    Router::new()
        // Health check
        .route("/health", get(health_check))

        // --- Agent management ---
        .route("/api/agents", get(list_agents))
        .route("/api/agents/:id", get(get_agent_detail))
        .route("/api/agents/install", post(install_agent))
        .route("/api/agents/:id", delete(uninstall_agent))
        .route("/api/agents/:id/clone", post(clone_agent))   // :id = INSTANCE id (ADR-073)
        .route("/api/agents/:id/start", post(start_agent))
        .route("/api/agents/:id/stop", post(stop_agent))

        // --- Conversation ---
        .route("/api/agents/:id/message", post(send_message))
        .route("/api/agents/:id/stream", get(agent_stream_ws))

        // --- Vault ---
        .route("/api/vault/keys", get(list_keys))
        .route("/api/vault/keys", post(add_key))
        .route("/api/vault/keys/:provider", delete(remove_key))
        .route("/api/vault/keys/:provider", put(update_key))

        // --- Configuration ---
        .route("/api/config", get(get_config))
        .route("/api/config", put(update_config))

        // --- Agent configuration ---
        .route("/api/agents/:id/config", get(get_agent_config))
        .route("/api/agents/:id/config", put(update_agent_config))

        // --- User / Identity ---
        .route("/api/users/profile", get(get_user_profile))
        .route("/api/users/profile", put(update_user_profile))

        // --- Models ---
        .route("/api/models", get(list_models))
        .route("/api/models/capabilities", get(get_model_capabilities))

        // --- Memory ---
        .route("/api/agents/:id/memory", get(query_memory))
        .route("/api/agents/:id/memory", post(store_memory))

        // --- Skills ---
        .route("/api/agents/:id/skills", get(list_skills))
        .route("/api/agents/:id/skills/:name", get(get_skill_detail))

        // --- Approval ---
        .route("/api/approvals", get(list_pending_approvals))
        .route("/api/approvals/:id/approve", post(approve))
        .route("/api/approvals/:id/reject", post(reject))

        // --- Publishing ---
        .route("/api/agents/:id/publish/prepare", post(publish_prepare))
        .route("/api/agents/:id/publish/build", post(publish_build))
        .route("/api/agents/:id/publish/install-locally", post(publish_install_locally))
        .route("/api/agents/:id/publish/export", post(publish_export))

        // --- Documents / Workspace ---
        .route("/api/workspace", get(get_workspace))
        .route("/api/workspace/documents", get(list_documents))

        // --- MCP Catalog ---
        .route("/api/mcp/servers", get(list_mcp_servers))
        .route("/api/mcp/servers/:id/install", post(install_mcp_server))

        // --- Scheduled tasks ---
        .route("/api/cron/jobs", get(list_cron_jobs))
        .route("/api/cron/jobs", post(create_cron_job))

        // --- System ---
        .route("/api/status", get(system_status))
}
```

### 9.4 Core Interface Details

#### 9.4.1 Agent Management

```json
// GET /api/agents
// → 200
{
    "agents": [
        {
            "agent_id": "com.example.weather",
            "name": "Weather Agent",
            "version": "1.0.0",
            "status": "running",       // running | stopped | error
            "dev": false,
            "pid": 12345               // populated when running
        }
    ]
}

// POST /api/agents/install — multipart, not JSON (see docs/protocols/en/http.md §9.1)
// Request: form-data { package: <bytes>, node_id: <explicit target node> }
//   ADR-113: node_id is REQUIRED; the Gateway infers no local default → 400
// → 202/200 { "agent_id": "com.example.weather", "version": "1.0.0" }
// → 400 { "error": "invalid package" / "missing required field: 'node_id'" }
// → structured dependency_not_ready (target node not yet NodeReady)

// POST /api/agents/{instance_id}/clone
// Request: { "mode": "skeleton" | "full", "new_agent_id": "com.example.weather",
//            "new_name": "Weather copy" }   // new_name optional, "" = "<name> (clone)"
// → 201 { "agent_id": "com.example.weather", "install_path": "{packages_dir}/{new_agent_id}/{new_instance_id}" }
//
// {instance_id} is the source INSTANCE (ADR-073); new_agent_id is a PACKAGE id
// and MAY equal the source's — one package can hold several instances, so
// uniqueness lives on the instance id the node mints.
// Mode semantics: 10-debug-protocol.md §7.2.1 (zh) / §9.3 (en).
```

#### 9.4.2 Conversation

```json
// POST /api/agents/:id/message
// Request: { "content": "What's the weather in Beijing today?" }
// → 200 { "message_id": "msg-001", "status": "queued" }
// → 404 { "error": "agent not found" }
// → 503 { "error": "agent not running" }

// Streaming events go through MQTT topic subscription (ADR-033), WebSocket channel deprecated.
// Message sending still goes via HTTP POST (one-shot, doesn't wait for streaming response):
//
// → POST /api/agents/:id/message  { content: "..." }
//   → 200  { "message_id": "msg-001", "status": "queued" }
//
// Streaming chunk / tool_call / done events delivered by Desktop App via MQTT broker subscription:
//
// → Client: SUBSCRIBE acowork/agents/:id/sessions/:sid/messages/+
// ← Broker PUB: { "type": "chunk",       "delta": "今", "message_id": "msg-001" }
// ← Broker PUB: { "type": "chunk",       "delta": "天", "message_id": "msg-001" }
// ← Broker PUB: { "type": "tool_call",   "name": "http_request", "params": {...} }
// ← Broker PUB: { "type": "tool_result", "name": "http_request", "result": {...} }
// ← Broker PUB: { "type": "done",        "message_id": "msg-001", "usage": {...} }
```

#### 9.4.3 Vault

```json
// GET /api/vault/keys
// → 200
{
    "keys": [
        { "provider": "openai", "has_key": true, "key_preview": "sk-...abc" },
        { "provider": "anthropic", "has_key": false }
    ]
}

// POST /api/vault/keys
// Request: { "provider": "openai", "key": "sk-proj-..." }
// → 201 { "provider": "openai" }
// → 400 { "error": "invalid key format" }
```

The Vault HTTP API **does not return plaintext Keys**, only existence and desensitized preview (first 3 chars + `...` + last 3 chars).

#### 9.4.4 System Status

```json
// GET /api/status
// → 200
{
    "gateway_version": "0.1.0",
    "uptime_seconds": 3600,
    "agents_running": 3,
    "agents_total": 7,
    "memory_usage_mb": 128
}

// GET /health
// → 200 { "status": "ok" }
```

### 9.5 Relationship between HTTP API and MQTT

> **History**: v3.1 and earlier versions used Socket API (Unix Socket / Named Pipe / Local TCP) as Gateway ↔ Runtime IPC channel; the gRPC era was Protocol Buffers bidirectional streaming (see [`16-ipc-grpc-migration.md`](./16-ipc-grpc-migration.md)). Starting from [ADR-033](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md), IPC converged to **MQTT + HTTP reverse proxy**.

Agent management operations in HTTP API (install/uninstall/start/stop) directly call Gateway internal components, sharing the same logic as MQTT channel processing:

```
POST /api/agents/:id/start
       │
       ▼
Gateway::lifecycle_manager().start_agent("com.example.weather")
       │
       ▼
(Same code path as state changes initiated with Agent Runtime via MQTT)
```

Conversation message forwarding path:

```
Desktop App → POST /api/agents/:id/message        (HTTP trigger)
       │
       ▼
Gateway → Intent Router → PUB intent/agents/:id/chat_message
       │                                       (MQTT real-time channel)
       ▼
Agent Runtime processes → PUB chat/stream/:sid (chunk/tool_call/done)
       │
       ▼
Broker pushes to Desktop App (MQTT SUBSCRIBE chat/stream/:sid)
```

HTTP API is not a bypass independent of the IPC channel, but Runtime protocol's **management plane + reverse proxy wrapper**. Runtime event streams go via MQTT, request/response via HTTP reverse proxy.

### 9.6 Security Design

| Measure | Description |
|---------|-------------|
| Localhost only | Default `127.0.0.1`, not exposed externally |
| Vault Key desensitization | GET endpoint doesn't return plaintext, POST endpoint receives plaintext |
| No CORS allow-list | Gateway doesn't maintain origin allow-list (CORS always permissive) — local loopback 0 risk |
| Optional Auth Token | Gateway generates random token, Desktop App retrieves on first connection, subsequent requests carry `Authorization: Bearer <token>` |
| Agent install verification | Mandatory package signature verification (HTTP and MQTT channels share the same verification path) |

Auth Token mechanism (optional, Phase 5+):
```
Gateway generates random token on startup → writes to ~/.config/agent-gateway/http_token
Desktop App reads this file on first connection → subsequent requests carry it
```

**Avatar endpoint not constrained by Auth Token (design decision):**

`GET /api/agents/:id/avatar` is a request directly initiated by the browser `<img>` tag, which cannot carry the `Authorization` header. If this endpoint requires Auth, avatars cannot load in remote Gateway scenarios. Given this:

- Avatar endpoint always allows anonymous access (even with `auth_enabled` on).
- Path traversal protection still applies (canonicalize mirror + prefix check).
- Remote Gateway deployment relies on network isolation / TLS / firewall / reverse proxy to limit access surface, not Auth Token.

#### 9.6.1 Remote Gateway Scenario

If the user deploys Gateway on a different machine from the Desktop App (e.g. home server / cloud host), the following configurations need to be manually adjusted:

| Config Item | Local Default | Remote Scenario Required | Description |
|-------------|---------------|--------------------------|-------------|
| `[http].host` | `127.0.0.1` | `0.0.0.0` or specific NIC IP | Otherwise Desktop App cross-host requests get RST |
| `[http].auth_enabled` | `false` | **Remote scenario required `true`** | Remote Gateway already binds 0.0.0.0, CORS permissive + no token = whole network callable, requires bearer-token auth |
| `cors_enabled` config item | _Deleted_ | — | CORS always permissive (see §9.2), no config switch needed |

In remote scenarios, package avatar files are at Gateway machine's `install_path`; Desktop App still fetches via `GET /api/agents/:id/avatar`, which Gateway reads from local filesystem and streams response. Avatar URLs append `?v=<version>` for HTTP cache busting; Gateway response header includes `Cache-Control: public, max-age=31536000, immutable`.

### 9.7 Desktop App Discovering Gateway

Desktop App needs to auto-discover Gateway's HTTP API port:

```rust
// Discovery strategy (by priority):
// 1. Read address saved in Desktop App's own configuration
// 2. Read Gateway's pidfile: ~/.local/share/agent-gateway/gateway.pid
//    pidfile content: { "pid": 12345, "http_port": 19876, "mqtt_port": 19875 }
// 3. Try default address http://127.0.0.1:19876/health
// 4. Prompt user to manually configure
```

## 10. Gateway CLI

Gateway CLI is a separate binary (`acowork`), providing command-line management entry for GUI-less scenarios. CLI communicates with Gateway via Gateway HTTP API, not directly operating Gateway internal state.

### 10.1 Command Interface

```bash
# Agent management
acowork install <path>          # Install .agent package (includes signature verification)
acowork uninstall <agent_id>    # Uninstall Agent
acowork list                    # List installed Agents and their status
acowork start <agent_id>       # Start Agent
acowork stop <agent_id>        # Stop Agent
acowork info <agent_id>        # View Agent detailed info (version, permissions, runtime status)

# Conversation
acowork chat <agent_id>        # Enter interactive conversation mode
acowork send <agent_id> <msg>  # Send a single message and output response

# Vault management
acowork vault list              # List configured Keys (desensitized display)
acowork vault add <provider>    # Add API Key (interactive input, no echo)
acowork vault remove <provider> # Remove API Key

# System management
acowork status                  # Gateway status (version, running Agents count, memory usage)
acowork config get [key]        # View Gateway configuration
acowork config set <key> <val>  # Modify Gateway configuration
```

### 10.2 Communication with Gateway

CLI communicates via Gateway HTTP API (`http://127.0.0.1:19876`), sharing the same set of REST endpoints as Desktop App (see §9). Discovery mechanism is also consistent with Desktop App: pidfile → default port → manual configuration.

```
acowork CLI → HTTP API → Gateway
                    ↑
Desktop App also goes through this path
```

### 10.3 Design Decisions

| Decision | Choice | Reason |
|----------|--------|--------|
| CLI and Gateway independent | Independent binary | Gateway can run without CLI (Desktop App sufficient), CLI can be installed on different machines for remote management (future) |
| Communication method | HTTP API (not directly MQTT) | HTTP API is standard REST, naturally fits CLI; MQTT is for Gateway ↔ Runtime real-time event push, CLI doesn't subscribe to streaming events |
| API Key input | Interactive no-echo | Prevents shell history leakage of Keys |
| CLI framework | clap | Rust ecosystem standard choice, consistent with acowork-sign toolchain |

## 11. Design Decision Records

| Decision | Choice | Reason |
|----------|--------|--------|
| Gateway doesn't proxy business logic | Pure coordination layer | Avoid single-point bottleneck; Agent Runtime direct LLM connection has lower latency |
| Dual protocol layer (since ADR-033) | MQTT + HTTP reverse proxy | MQTT carries real-time event push (chat chunk, tool_call, done, device status), HTTP reverse proxy carries large data queries and session history (see ADR-033 / ADR-034) |
| HTTP framework | Axum | Most mature HTTP framework in Rust ecosystem; Gateway already confirmed in technology selection |
| HTTP port | 127.0.0.1:19876 | Localhost only, secure; port configurable; auto-increment on conflict |
| Vault HTTP desensitization | No plaintext return | Prevents Desktop App frontend vulnerabilities causing Key leakage; POST endpoint receives plaintext is fine |
| Gateway discovery mechanism | pidfile + default address | Simple and reliable; pidfile written by Gateway at startup; Desktop App tries by priority |
| Desktop App and Gateway independent | Independent processes | Gateway can run independently to support CLI-only users |