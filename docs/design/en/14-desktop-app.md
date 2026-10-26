# Desktop App

> Version: v3.4 | Last Updated: 2026-04-16

---

ACowork Desktop App is a Tauri-based desktop client serving as the primary user interface for interacting with Agents. Desktop App and Gateway are **independent processes**, communicating via the Gateway Service API.

## 1. Position and Responsibilities

Desktop App is the **user interface layer** of the ACowork platform; it carries no platform core logic. Its responsibilities are:

| Responsibility | Description |
|----------------|-------------|
| Agent interaction | User-Agent conversation UI, message send/receive |
| Agent management | Install, uninstall, clone, create, start/stop Agents |
| Debug panel | Developer-mode step debugging, record/replay on Agent Runtime |
| Configuration management | Gateway config, API Key management (Vault), Provider config |
| System tray | Gateway status indicator, quick actions |

**What Desktop App does NOT do:**
- Does not run Agent logic (Agent Runtime is a separate process)
- Does not manage Agent lifecycle (Gateway handles it)
- Does not store API Keys (Gateway Vault handles it)
- Does not proxy LLM calls (Agent Runtime connects directly)

## 2. Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                    ACowork Desktop App (Tauri v2)                │
│                                                                  │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │  WebView Frontend (React)                                 │  │
│  │                                                           │  │
│  │  ┌────────┐ ┌──────────┐ ┌──────────┐ ┌───────────────┐  │  │
│  │  │ Agent  │ │ Chat     │ │ Execution│ │ Settings      │  │  │
│  │  │ List   │ │ Panel    │ │ Results  │ │ (Vault/Config)│  │  │
│  │  └────────┘ └──────────┘ └──────────┘ └───────────────┘  │  │
│  │  ┌────────┐ ┌──────────┐ ┌──────────┐ ┌───────────────┐  │  │
│  │  │ Debug  │ │ Skill    │ │ Manifest │ │ Publish       │  │  │
│  │  │ Panel  │ │ Editor   │ │ Editor   │ │ Wizard        │  │  │
│  │  └────────┘ └──────────┘ └──────────┘ └───────────────┘  │  │
│  └───────────────────────────────────────────────────────────┘  │
│                            │ Tauri IPC (invoke)                  │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │  Rust Backend                                             │  │
│  │                                                           │  │
│  │  ┌──────────────┐  ┌──────────────┐  ┌───────────────┐  │  │
│  │  │ Gateway      │  │ Debug        │  │ Tray          │  │  │
│  │  │ Client       │  │ Protocol     │  │ Manager       │  │  │
│  │  │ (HTTP/MQTT)  │  │ Client       │  │               │  │  │
│  │  │              │  │ (HTTP+MQTT)  │  │               │  │  │
│  │  └──────────────┘  └──────────────┘  └───────────────┘  │  │
│  └───────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
          │                                    │
          │ Gateway Service API                │ Debug Protocol
          │ (HTTP REST + MQTT)                  │ (HTTP RPC + MQTT events)
         ▼                                    ▼
┌─────────────────────┐           ┌─────────────────────────┐
│  Gateway (separate) │           │  Agent Runtime          │
│                     │           │  (DevMode)              │
│  ┌───────────────┐  │           │                         │
│  │ Key Vault     │  │           │  Main loop under        │
│  │ Lifecycle     │  │           │  debugger control        │
│  │ Intent Router │  │           │  Editable re-execution  │
│  │ Package Mgr   │  │           │  Skill hot-reload       │
│  └───────────────┘  │           │  Record/replay engine   │
└─────────────────────┘           └─────────────────────────┘
```

### 2.1 Desktop App ↔ Gateway Communication

Desktop App interacts with Gateway via the **Gateway HTTP API**:

| Operation | HTTP Method | Path | Description |
|-----------|-------------|------|-------------|
| Gateway status | GET | `/health` | Health check |
| Agent list | GET | `/api/agents` | Installed Agents list |
| Agent install | POST | `/api/agents/install` | Install .agent package |
| Agent uninstall | DELETE | `/api/agents/:id` | Uninstall Agent |
| Agent clone | POST | `/api/agents/clone` | Clone Agent |
| Agent start/stop | POST | `/api/agents/:id/start` | Start Agent |
| Agent start/stop | POST | `/api/agents/:id/stop` | Stop Agent |
| Send message | POST | `/api/agents/:id/message` | Send user message to Agent |
| Streaming | MQTT subscribe | `acowork/agents/{id}/sessions/{sid}/messages/#` | Subscribe to Agent stream output |
| Vault ops | GET/POST | `/api/vault/*` | API Key management |
| Config ops | GET/PUT | `/api/config/*` | Gateway config |

> Note: Gateway's original Socket API (for Agent Runtime) remains unchanged. Desktop App uses HTTP API; Gateway needs to expose an additional HTTP layer for Desktop App invocation. These are two distinct consumers with layered protocols:

```
Gateway
├── Socket API (Port A)    ← Agent Runtime uses (existing design)
└── HTTP API (Port B)      ← Desktop App + CLI use (newly added)
```

### 2.2 Desktop App ↔ Agent Runtime Communication (DevMode)

In developer mode, Desktop App connects directly to the target Agent Runtime via the **Debug Protocol** (HTTP RPC + MQTT events):

```
Desktop App  ──debug_rpc──>  Agent Runtime (DevMode, HTTP localhost:19878)
             <──debug-event── (MQTT)
```

The complete Debug Protocol definition is in [10-debug-protocol.md](./10-debug-protocol.md).

## 3. Page Layout

Desktop App uses a **left-center-right four-column layout**, dynamically adjusting visibility and content based on current mode (User mode / Developer mode).

### 3.1 Overall Layout

```
┌──────────────────────────────────────────────────────────────────────┐
│  ACowork                            [Developer Mode ○]  [— □ ✕] │
├────┬──────────────┬────────────────────────┬────────────────────────┤
│    │              │                        │                        │
│ 💬 │   Agent      │     Chat Panel         │   Execution            │
│ 🤖 │   List       │                        │   Results              │
│ 📋 │              │                        │                        │
│ ⚙️ │  ┌────────┐  │  ┌──────────────────┐  │   User Mode:          │
│    │  │Agent A  │  │  │ User:            │  │   - Tool call results  │
│    │  │Agent B  │  │  │ Assistant:       │  │   - Execution time     │
│    │  │Agent C  │  │  │ Tool:            │  │   - Token usage        │
│    │  │...      │  │  │ Assistant:       │  │                        │
│    │  └────────┘  │  └──────────────────┘  │   Developer Mode:      │
│    │              │                        │   - Debug console      │
│    │              │  ┌──────────────────┐  │   - Step details       │
│    │              │  │ Input: [_______] │  │   - Breakpoint status  │
│    │              │  └──────────────────┘  │   - Recording control  │
│    │              │                        │                        │
└────┴──────────────┴────────────────────────┴────────────────────────┘
```

### 3.2 Region Descriptions

#### 3.2.1 Navigation Bar (leftmost, fixed 48px width)

Vertical icon navigation; click switches middle content area:

| Icon | Function | Description |
|------|----------|-------------|
| 💬 Chat | Chat view | Default view: Agent List + Chat + Results |
| 🤖 Models | Model management | Provider list, model config, API Key status |
| 📋 Skills | Skill list | Current Agent's Skill list, edit entry |
| ⚙️ Settings | Settings | Gateway connection config, global preferences, about |

#### 3.2.2 Agent List (second column, 240px width)

- Displays all installed Agents (from Gateway `/api/agents`)
- Each Agent entry: icon/name + running status indicator
- Click to select current interaction Agent
- Right-click menu: start/stop, view details, clone, uninstall, settings
- Bottom: `+ Create Agent` / `+ Install from File` buttons
- **Developer mode extra**: Agents display `dev` tag (development-state Agents)

#### 3.2.3 Chat Panel (third column, flexible width)

Current selected Agent's conversation UI:

- **Message stream**: Conversation history (system/user/assistant/tool messages)
- **Input area**: Bottom input box, multi-line input and shortcut send
- **Tool call display**: Inline display of tool_call and tool_result, expandable/collapsible
- **Streaming output**: LLM response real-time streaming display
- **Message actions**:
  - User mode: Copy message content, regenerate
  - Developer mode extra: Edit message content, Re-execute from here

#### 3.2.4 Execution Results Area (fourth column, 320px width, collapsible)

**User Mode:**
- Tool call summary: tool name, params, duration, status (success/failure)
- Current session Token usage statistics (prompt/completion/total)
- Current Agent runtime status

**Developer Mode** (replaces the above content):
- Debug controls: Resume / Pause / Step / Stop
- Step execution details: current iteration round, phase, LLM input/output
- Breakpoint panel: set breakpoints list, add/delete breakpoints
- Provider switcher: dropdown to select current Provider + model
- Recording control: start recording / stop recording / load replay

### 3.3 Developer Mode Switching

User mode and Developer mode are toggled via the toolbar at the top:

```
User mode
  │
  └── Enable Developer Mode
      │
      ├── Desktop App sends DebuggerAttach to Agent Runtime
      │   (if current Agent is not running in DevMode, notify Gateway to restart in DevMode first)
      │
      ├── Execution results area switches to debug panel
      │
      ├── Chat panel messages gain Edit / Re-execute actions
      │
      └── Navigation bar additionally displays Skills edit, Manifest edit entries
```

The complete capability definition for developer mode is in [10-debug-protocol.md](./10-debug-protocol.md).

### 3.4 Window Management

| Behavior | Description |
|----------|-------------|
| Close window | Hide to system tray (do not exit process) |
| System tray icon | Shows Gateway connection status (connected/disconnected/error) |
| Left-click tray icon | Show/focus main window |
| Right-click tray menu | Show Dashboard / Agent Chat / Status / Quit |
| Minimum size | 1024 x 600 |
| Default size | 1200 x 800 |

## 4. User Mode Features

### 4.1 First-Launch Onboarding

```
Step 1: Welcome
  "Welcome to ACowork, let's quickly configure your environment"

Step 2: Gateway connection
  ├─ Auto-detect local Gateway (try connecting to default address)
  ├─ Detection success → go to Step 3
  └─ Detection failure → prompt to start Gateway or configure address

Step 3: API Key configuration
  ├─ Import from file
  ├─ Manual input (Provider + Key)
  └─ Later manage in Settings

Step 4: Identity information collection (required → optional)
  ├─ Required: display name / language / timezone
  ├─ Optional: city / occupation / communication preferences
  └─ After completion, call Gateway HTTP API to write to UserProfile

Step 5: Install first Agent
  ├─ Select from local repository
  ├─ Drag-and-drop .agent file to install
  └─ Skip (install manually later)

→ Enter main interface
```

> Step 4 data flow: Desktop App → `POST /api/users` → Gateway → UserProfile module → persistent storage. See [18-user-identity-simplified.md](./18-user-identity-simplified.md).

### 4.2 Agent Management

| Operation | Entry | Description |
|-----------|-------|-------------|
| Install | Agent list bottom `+` / drag-drop .agent file | Call Gateway install API |
| Uninstall | Agent right-click menu | After confirmation, call Gateway uninstall API |
| Start/Stop | Agent right-click menu / status indicator | Call Gateway Lifecycle API |
| View details | Agent right-click menu | Display manifest info, runtime status, version |
| Clone | Agent right-click menu (developer mode) | See §5.1 |
| Create from scratch | Agent list bottom (developer mode) | See §5.2 |

### 4.3 Conversation

- User input message → Desktop App calls Gateway `/api/agents/:id/message` → Gateway forwards to Agent Runtime via Intent Router
- Agent responses are pushed back to Desktop App via MQTT streaming subscription
- Conversation history is stored in Agent's private SQLite; Desktop App does not persist conversation data

### 4.4 Settings Page

| Category | Content |
|----------|---------|
| Gateway | Connection address, health status, version info |
| Providers | Provider list, default Provider, model config |
| Vault | API Key management (add/remove/modify, via Gateway Vault API) |
| Appearance | Theme (light/dark), language, font size |
| General | Log level, data directory location, update check |

### 4.5 Workspace Management

#### Workspace Selector Interaction Spec

**Current workspace selection**:
- In the dropdown menu, the currently selected workspace entry shows a checkmark (✓) icon
- Dropdown button displays alias/path of the currently selected workspace (not fixed to the first)
- After selecting a new workspace, call `PUT /api/agents/{agent_id}/workspaces/current` to persist the selection
- Also update selection statistics (`select_count`, `last_selected_at`) for weight sorting

**Agent switching linkage**:
- When user switches to a different Agent, automatically reload that Agent's workspace list
- The list is **one global array carrying the agent it describes** (`workspacesAgentId`). Starting another Agent fetches for that one and overwrites the slot, so consumers render and allow picking only when the tag matches the selected Agent, and a failed fetch still claims the slot with an empty list — "this Agent's list is unknown" is not the same thing as "here is another Agent's list", which can be clicked and write a workspace id the current Agent does not own. On a mismatch the list is re-fetched and the panel recovers within a frame
- Restore that Agent's last current workspace selection

**State management**:
- Use dedicated `workspaceStore` (Zustand) to manage workspace state
- Contains `workspaces` list, `currentWorkspaceId`, `loading` state

## 5. Developer Mode Features

Developer mode layers debug capabilities on top of user mode. All debug protocol details are in [10-debug-protocol.md](./10-debug-protocol.md).

### 5.1 Agent Clone

Desktop App's Agent list right-click menu provides "Clone" option (visible in developer mode):

```
User right-click Agent A → Clone
       │
       ▼
Clone dialog:
  ├─ Source agent: com.example.weather (com.example.weather)
  ├─ Clone mode:
  │   ○ Skeleton (every package file — manifest, prompts, skills,
  │     assets and any other package directory)
  │   ● Full (+ workspace/config + conversation history + SQLite memory;
  │     files/ and logs/ stay behind)
  ├─ New Agent ID: [com.example.weather    ]  ← defaults to the source;
  │    package ids may repeat — uniqueness is on instance_id (ADR-073)
  ├─ New agent name: [blank = "Name (clone)"]
  └─ [Cancel]  [Clone]
       │
       ▼
Call Gateway /api/agents/:instance_id/clone
  body: { new_agent_id, new_name, mode }
       │
       ▼
Agent list refreshes; the new agent appears marked dev: true
```


### 5.2 Create from Scratch

Agent list bottom "Create Agent" button opens the creation wizard in developer mode:

```
Step 1: Basic info — agent_id, name, description, author
Step 2: LLM config — select Provider + model (from existing Vault Keys)
Step 3: Permission declaration — check required permission templates
Step 4: Template selection — blank / weather / calendar / custom
Step 5: Generate → call Gateway API to create workspace → new Agent marked dev: true → auto-enter DevMode
```

### 5.3 Skill Editor

The "Skills" view in the navigation bar provides editing capabilities in developer mode:

```
┌─ Skills ──────────────────────────────────────────┐
│                                                    │
│  Current Agent: com.example.weather-dev                │
│                                                    │
│  ┌──────────────┐  ┌──────────────────────────┐   │
│  │ Skills list   │  │ SKILL.md Editor           │   │
│  │              │  │                          │   │
│  │ ● weather-   │  │ ---                      │   │
│  │   query      │  │ name: weather-query      │   │
│  │              │  │ description: ...         │   │
│  │ ○ news-      │  │ triggers:               │   │
│  │   digest     │  │   - weather              │   │
│  │              │  │ ---                      │   │
│  │ [+ New]     │  │                          │   │
│  └──────────────┘  │ # Weather Query Skill    │   │
│                    │                          │   │
│                    │ ...                      │   │
│                    └──────────────────────────┘   │
│                                                    │
│  [🔄 Reload to Runtime]  [▶ Test in Chat]         │
└────────────────────────────────────────────────────┘
```

- `Reload to Runtime`: After saving, send `DebuggerReloadSkills` via Debug Protocol
- `Test in Chat`: After hot-loading, auto-send a triggering message in Chat panel

### 5.4 Publish Wizard

In developer mode, the "Publish" option in Agent list right-click menu opens the publish wizard:

```
Step 1: Check — manifest integrity, SKILL.md format, prompts presence
Step 2: Clean — remove dev tag, clear recordings/, reset config/
Step 3: Package — generate .agent ZIP file
Step 4: Sign — call acowork-sign to sign
Step 5: Distribute — local install / export to file / upload to repository
```

Detailed flow in [10-debug-protocol.md](./10-debug-protocol.md) §8.

## 6. System Tray

### 6.1 Tray Icon Status

| Status | Icon Style | Tooltip |
|--------|-----------|---------|
| Gateway connected | Normal icon | `ACowork — Connected` |
| Gateway disconnected | Gray icon | `ACowork — Disconnected` |
| Agent running | Blue pulse | `ACowork — 2 Agents Running` |
| Agent executing | Green pulse | `ACowork — Working` |
| Error | Red icon | `ACowork — Error` |

### 6.2 Tray Right-Click Menu

```
┌──────────────────┐
│ Show Dashboard   │
│ Agent Chat       │
│──────────────────│
│ Status: Running  │  (disabled, display only)
│──────────────────│
│ Start Gateway    │  (shown when disconnected)
│ Stop Gateway     │  (shown when connected)
│──────────────────│
│ Quit ACowork    │
└──────────────────┘
```

### 6.3 Gateway Health Polling

Desktop App periodically (every 5s) calls Gateway `/health` endpoint in background:

```
Health check
  │
  ├─ Gateway online → update tray status to Connected
  │
  ├─ Gateway offline → update tray status to Disconnected
  │   └─ Main window shows "Gateway not connected" banner, guides user to start
  │
  └─ 3 consecutive failures → degrade to 30s polling (reduce resource consumption)
```

## 7. Technology Selection

### 7.1 Tauri v2

| Component | Choice | Reason |
|-----------|--------|--------|
| **Framework** | Tauri v2 | Better security model (capability-based), mature plugin system, Rust ecosystem consistency |
| **IPC** | Tauri Commands (invoke) | Type-safe front-backend communication |
| **Frontend Framework** | React 19 + TypeScript | Most mature ecosystem, rich component library, Tauri + React proven |
| **Build Tool** | Vite | Fast HMR, Tauri officially recommended |
| **UI Component Library** | shadcn/ui + Tailwind CSS | Highly customizable, no runtime dependencies, tree-shakable |
| **State Management** | Zustand | Lightweight, TypeScript-friendly, suitable for medium-complexity apps |
| **HTTP + MQTT** | `fetch` + `mqtt.js` (or Tauri Command `debug_rpc`) | Streaming messages (MQTT subscribe), Debug Protocol (`debug_rpc` + `debug-event` events) |

### 7.2 Tauri Plugins

| Plugin | Purpose |
|--------|---------|
| `tauri-plugin-shell` | Call external commands (acowork-sign etc. CLI tools) |
| `tauri-plugin-store` | Persist Desktop App's own configuration (Gateway address, window state etc.) |
| `tauri-plugin-single-instance` | Prevent multi-instance |
| `tauri-plugin-dialog` | File picker dialog (install .agent package) |
| `tauri-plugin-notification` | System notifications (when Agent completes long task) |

### 7.3 Frontend Directory Structure

```
apps/acowork-desktop/
├── src-tauri/                  # Rust backend
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/           # Tauri permission declarations
│   ├── icons/                  # App icons
│   └── src/
│       ├── main.rs             # Entry
│       ├── lib.rs              # Tauri Builder config
│       ├── commands/           # Tauri Commands (frontend-callable Rust functions)
│       │   ├── mod.rs
│       │   ├── gateway.rs      # Gateway API wrapper
│       │   ├── agent.rs        # Agent management operations
│       │   ├── debug.rs        # Debug Protocol wrapper
│       │   ├── vault.rs        # Vault/Key management
│       │   └── settings.rs     # Config management
│       ├── gateway_client.rs   # Gateway HTTP client
│       ├── debug_client.rs     # Debug Protocol client
│       ├── state.rs            # Shared state (Arc<RwLock>)
│       └── tray/               # System tray
│           ├── mod.rs
│           ├── menu.rs
│           ├── icon.rs
│           └── events.rs
│
└── web/                        # React frontend
    ├── package.json
    ├── vite.config.ts
    ├── tsconfig.json
    ├── index.html
    └── src/
        ├── main.tsx
        ├── App.tsx
        ├── components/         # UI components
        │   ├── layout/         # Layout (four-column)
        │   ├── chat/           # Chat panel
        │   ├── agent-list/     # Agent list
        │   ├── results/        # Execution results area
        │   ├── debug/          # Debug panel (developer mode)
        │   ├── skills/         # Skill editor
        │   ├── settings/       # Settings page
        │   └── common/         # Common components
        ├── hooks/              # Custom hooks
        ├── stores/             # Zustand state
        ├── lib/                # Utility functions, type definitions
        └── styles/             # Tailwind styles
```

### 7.4 Cargo.toml Dependencies

```toml
[package]
name = "acowork-desktop"
version = "0.1.0"
edition = "2024"

[build-dependencies]
tauri-build = { version = "2", features = [] }

[dependencies]
tauri = { version = "2", features = ["tray-icon", "image-png"] }
tauri-plugin-shell = "2"
tauri-plugin-store = "2"
tauri-plugin-single-instance = "2"
tauri-plugin-dialog = "2"
tauri-plugin-notification = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "net"] }
tokio-tungstenite = "0.26"    # WebSocket (Debug Protocol)
anyhow = "1"
directories = "6"
# Desktop App does not depend on acowork internal crates — communicates with the platform
# via Gateway HTTP API / Debug WebSocket; all data structures are defined independently
# per API contracts to maintain UI layer decoupling from platform core.

[features]
default = ["custom-protocol"]
custom-protocol = ["tauri/custom-protocol"]
```

## 8. Gateway HTTP API (New)

Gateway needs to add an HTTP API layer for Desktop App and CLI. This is an independent consumer from the existing Socket API (for Agent Runtime).

### 8.1 Why HTTP API is Needed

| Dimension | Socket API (existing) | HTTP API (new) |
|-----------|----------------------|----------------|
| Consumer | Agent Runtime | Desktop App / CLI |
| Transport | Unix Socket / Named Pipe | HTTP (localhost) |
| Communication | Long connection + bidirectional push | Request/response + WebSocket streaming |
| Purpose | Inter-process real-time communication | User interface operations |

The Socket API is the underlying IPC protocol, not suitable for direct WebView calls. The HTTP API is the abstraction layer for user-facing operations; both share Gateway's internal logic.

### 8.2 HTTP Endpoint Definitions

```rust
// Gateway HTTP API routes (new)
pub enum HttpRoute {
    // Health check
    Get("/health") -> HealthResponse,

    // Agent management
    Get("/api/agents") -> AgentListResponse,
    Post("/api/agents/install") -> AgentInstallResponse,         // body: .agent file path
    Delete("/api/agents/:id") -> AgentUninstallResponse,
    
    Post("/api/agents/:id/start") -> AgentStartResponse,
    Post("/api/agents/:id/stop") -> AgentStopResponse,
    Get("/api/agents/:id") -> AgentDetailResponse,

    // Conversation
    Post("/api/agents/:id/message") -> MessageResponse,          // body: { content }
    Get("/api/agents/:id/stream") -> WebSocketUpgrade,          // Streaming conversation

    // Vault
    Get("/api/vault/keys") -> KeyListResponse,
    Post("/api/vault/keys") -> KeyAddResponse,                   // body: { provider, key }
    Delete("/api/vault/keys/:provider") -> KeyDeleteResponse,

    // Configuration
    Get("/api/config") -> ConfigResponse,
    Put("/api/config") -> ConfigUpdateResponse,

    // System info
    Get("/api/status") -> StatusResponse,
}
```

### 8.3 HTTP Server Implementation

Gateway adds an HTTP Server (using Axum), listening on `http://127.0.0.1:19876`:

```rust
// Gateway process listens on:
// 1. Socket API (for Agent Runtime)
// 2. HTTP API (for Desktop App / CLI)
// Both share the same Gateway internal state
```

HTTP port is configurable, default `19876`. Listens only on `127.0.0.1`, not exposed externally.

## 9. Relationship with Existing Documents

| Document | Relationship |
|----------|-------------|
| [01-overview.md](./01-overview.md) | Desktop App is the materialization of "Future Extensions" in the overview |
| [04-gateway.md](./04-gateway.md) | Gateway adds HTTP API, other components unchanged |
| [06-communication.md](./06-communication.md) | Socket API unchanged, HTTP API is the new consumer layer |
| [10-debug-protocol.md](./10-debug-protocol.md) | Desktop App's developer mode entirely depends on Debug Protocol |
| [12-tool-system.md](./12-tool-system.md) | Desktop App does not involve Tool execution, only displays results |
| [13-skill-system.md](./13-skill-system.md) | Desktop App's Skill editor is the UI entry for Skill lifecycle |

## 10. Design Decision Records

| Decision | Choice | Reason |
|----------|--------|--------|
| Desktop App and Gateway independent | Separate processes | Gateway can run independently for CLI-only users; clear responsibilities |
| Gateway adds HTTP API | Axum HTTP | Socket API not suitable for direct WebView calls; HTTP is Desktop App / CLI standard consumer interface; consistent with existing Axum choice |
| Layout | Left-center-right four columns | Navigation + Agent list + Chat + Results area, clear information hierarchy; developer mode reuses same layout overlaying debug panel |
| Frontend Framework | React + TypeScript | Most mature ecosystem; Tauri + React feasibility verified |
| UI Component Library | shadcn/ui + Tailwind | No runtime dependencies, customizable, tree-shakable |
| System tray | Close hides without exit | Standard desktop app behavior; Gateway is persistent, Desktop App as GUI frontend should also be persistent |
| Single instance | tauri-plugin-single-instance | Avoid multi-window chaos; second launch focuses existing window |
| Gateway status detection | Poll /health | Simple and reliable; 5s interval sufficient; degrade to 30s on failure |