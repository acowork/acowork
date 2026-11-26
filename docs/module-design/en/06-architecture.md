# Architecture: Dependencies, Data Flow, Roadmap, Build Artifacts, Testing

## 1. Inter-Module Dependencies

```
                    ┌──────────────┐
                    │ acowork-core│ ← shared types layer
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
    ┌─────────▼──────┐ ┌──▼────────┐ ┌─▼────────────┐
    │acowork-runtime│ │acowork-  │ │acowork-     │
    │                │ │gateway    │ │sign          │
    │    deps:       │ │           │ │              │
    │  · core        │ │ deps:     │ │ deps:        │
    │  · sqlite     │ │ · core    │ │ · core       │
    │  · sign(verify)│ │ · sign    │ │ (no runtime  │
    │                │ │ · vault   │ │  deps)        │
    └────────┬───────┘ └───┬───────┘ └──────────────┘
             │             │
    ┌────────▼──────┐ ┌───▼──────────┐
    │acowork-sqlite│ │acowork-vault│
    │               │ │              │
    │ deps:         │ │ deps:        │
    │ · core(Memory │ │ · core       │
    │   trait)      │ │   (none)     │
    └───────────────┘ └──────────────┘
```

**Key constraints**:
- `acowork-core` doesn't depend on any other internal crate
- `acowork-sqlite` only depends on `acowork-core`'s Memory trait
- `acowork-runtime` and `acowork-gateway` have **no direct dependency**, they communicate via IPC
- `acowork-sign` is an independent tool crate, doesn't depend on runtime crates

---

## 2. Data Flow and Communication

### 2.1 Bot Startup Flow (Data Perspective)

```
Gateway CLI: "start com.example.weather"
    │
    ├─1→ PackageManager: read installed manifest → AgentManifest
    │
    ├─2→ Gateway UserProfile: read identity → {name, city, language}
    │
    ├─3→ Vault: get api_key_ref → SecretString
    │
    ├─4→ SandboxConfig: generate sandbox params from manifest
    │
    └─5→ LifecycleManager: spawn agent-runtime process
         params: --package-path, --socket, --agent-id, 
               --workspace, --identity (JSON), --dev-mode
```

### 2.2 Bot Main Loop (Data Perspective)

```
Agent Runtime process startup
    │
    ├─1→ PackageLoader: parse ZIP → (manifest, prompts, skills, config)
    │
    ├─2→ IPC Client: connect to Gateway Socket → handshake
    │
    ├─3→ IPC Client: KeyRelease → SecretString (stored in process memory)
    │
    ├─4→ SqliteStore::open(workspace/memory/private.sqlite, embedding_dim)
    │
    └─5→ Main loop:
         Each iteration:
         ├─ Context::build() → ChatMessage[]
         ├─ Provider::chat() → ChatResponse
         ├─ ToolDispatcher::dispatch() → ToolResult[]
         ├─ History::append()
         └─ IPC Client: UsageReport (async)
```

### 2.3 Intent Cross-Bot Communication

```
Weather Agent → IPC Client → GatewayRequest::IntentSend
    │
    ▼
Gateway IntentRouter:
    ├─ find target agent
    ├─ if not running → LifecycleManager::start_agent()
    └─ forward Intent → Agent B's IPC connection
    │
    ▼
Calendar Agent ← GatewayResponse::IntentReceived
    ├─ process Intent
    └─ return result → Gateway → Weather Agent
```

---

## 3. Mapping to Roadmap

| Phase | Crates to Implement | Core Modules |
|-------|---------------------|--------------|
| **Phase 1: MVP** | core, runtime, gateway, sign, vault | `core`: manifest + protocol + traits<br>`runtime`: agent/loop + package/loader + providers/openai + tools/builtin(core 17) + tools/memory(5) + tools/agent(intent_send, ask_user) + ipc/client<br>`gateway`: package_manager + lifecycle + ipc/server + vault<br>`sign`: keygen + sign + verify<br>`vault`: encrypted storage |
| **Phase 2: Memory** | + sqlite | `acowork-sqlite`: all modules (schema + provider + retrieval + session_meta + conversation)<br>`runtime`: memory/ module<br>`gateway`: system_agent/identity_injector |
| **Phase 2.5: DevFramework** | + runtime/debug | `runtime`: debug/ all modules<br>`gateway`: lifecycle extension (clone API) |
| **Phase 3: Security Sandbox** | + gateway/sandbox | `gateway`: sandbox/ per-platform implementation<br>`runtime`: tools/wasm<br>`core`: permission enhancement |
| **Phase 4: Communication Coordination** | gateway extension | `gateway`: intent/ + budget/ + rate/<br>`runtime`: tools/gateway enhancement |
| **Phase 5: Cloud Ecosystem** | + desktop app | `apps/acowork-desktop`: Tauri app<br>`gateway`: package_manager/repository |

---

## 4. Build Artifacts

| Binary | Source Crate | Description |
|--------|--------------|-------------|
| `agent-runtime` | acowork-runtime | Bot unified execution engine |
| `acowork-gateway` | acowork-gateway | Gateway daemon |
| `acowork-keygen` | acowork-sign | Key pair generation |
| `acowork-sign` | acowork-sign | .bot package sign |
| `acowork-verify` | acowork-sign | .bot package verify |
| `acowork` | (CLI wrapper) | Unified CLI entry (aggregating subcommands) |

**Unified CLI Design** (Phase 5, Phase 1 uses independent binaries):

```bash
# Phase 1: independent binaries
agent-runtime /path/to/weather.agent --socket /tmp/gateway.sock
acowork-gateway start
acowork-keygen --alias my-key
acowork-sign --key my-key.pem --input weather.unsigned.agent
acowork-verify weather.agent

# Phase 5: unified CLI
acowork gateway start
acowork agent install weather.agent
acowork agent start com.example.weather
acowork keygen --alias my-key
acowork sign weather.unsigned.agent
acowork verify weather.agent
```

---

## 5. Testing Strategy

| Test Level | Location | Description |
|------------|----------|-------------|
| Unit tests | each crate `src/` `#[cfg(test)]` | Each trait implementation has tests |
| Component tests | `tests/` per crate | Single-crate integration tests |
| Integration tests | workspace root `tests/` | Cross-crate tests: Gateway + Runtime interaction |
| System tests | workspace root `tests/` | Complete flow: install Bot → start → conversation → tool call → stop |
| Live tests | manual | End-to-end tests connecting to real LLM API |

---

> **Next step**: Based on this module design, refine interface definitions for each module one by one. Recommend starting with `acowork-core`, because it is the foundation of all crates.