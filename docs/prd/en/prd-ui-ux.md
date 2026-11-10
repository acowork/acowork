# ACowork Desktop App — UI/UX Product Requirements Document

> Version: v2.0 | Revision Date: 2026-04-27  
> Related Design Docs: `docs/design/14-desktop-app.md`  
> Related Implementation Plan: S1 User Mode Task Definitions (archived in `docs/_internal/archive/plan/zh/plan-p5.md`, local reference)  
>  
> **v2.0 Revision Notes**: v1.0 of this document described a layout (top title bar with Gateway indicator, left navigation bar with Chat/Models/Skills/Settings, dynamic system tray status, independent Models view, Vault/Providers Settings tabs, etc.) that severely diverged from the current codebase. v2.0 has been completely rewritten section by section based on the actual code in `apps/acowork-desktop` to reflect the implementation as it stands.  
> Key differences: the navigation bar now has 5 views (Chat/Projects/Docs/Harness/Settings) + top avatar; Gateway status moved to the bottom status bar; system tray only contains a Quit menu; no standalone Models view (Provider management lives in the Harness view); Settings has five tabs: Profile/General/Appearance/Gateway/Nodes; the chat area consists of "Agent List + Chat Panel + optional File Editor + Results Panel + 40px right toolbar".

---

## 1. Document Purpose

This document defines the **user-mode** interaction specifications for all pages of the ACowork Desktop App, serving as the sole implementation reference for Phase 5 S1 frontend development. Developer-mode UI (Debug/DevMode) is described in this document as one tab within the Results Panel; the full protocol is detailed in `docs/design/10-debug-protocol.md`.

This document is based on the current implementation of the frontend code in `apps/acowork-desktop/src` and the backend code in `apps/acowork-desktop/src-tauri`. Frontend tech stack: React 19 + TypeScript + Vite + Tailwind CSS v4 + Zustand; desktop shell: Tauri v2.

---

## 2. Design System

### 2.1 Design Tokens

Based on Tailwind CSS v4 `@theme`, the project defines design variables (see `src/styles/globals.css`). Note: the actual code mainly uses **semantic surface colors** (`chat-area` / `modal-surface` / `nav-surface`, etc.) instead of shadcn's `primary`/`accent` grayscale variables.

```css
/* Semantic surface colors (auto‑flip with .dark, no dark: prefix needed) */
--color-chat-area:        hsl(0 0% 98%);      /* main workspace (light), dark = zinc-900 */
--color-modal-surface:    hsl(0 0% 100%);     /* dialog/card surface (light), dark = #27272A */
--color-modal-overlay:    hsl(0 0% 0% / 0.5); /* modal overlay (theme‑independent) */
--color-nav-surface:      hsl(240 8% 94%);    /* Agent list container (light), dark = #2E2E2E */
--color-nav-control:      hsl(240 5% 87%);    /* input/button static background (light) */
--color-nav-control-hover:hsl(240 5% 82%);    /* primary action hover */
--color-nav-item-hover:   hsl(240 6% 90%);    /* list row hover */
--color-nav-divider:      hsl(0 0% 78%);      /* list divider */
--color-editor-canvas:    #FFFFFF;            /* file editor canvas, dark = #1E1E1E (matches Monaco) */

/* Chat message surfaces */
--color-chat-bubble:      hsl(240 5.9% 90.6% / 0.4);  /* assistant/think/error/system bubbles */
--color-chat-title:       hsl(240 4.8% 86.1% / 0.5);  /* block title */
--color-chat-body:        hsl(0 0% 98% / 0.8);        /* block body (same as panel, visually merged) */
--color-chat-border:      hsl(240 5.9% 90.6%);        /* container border */
--color-chat-badge:       hsl(240 5.9% 90.6%);        /* sender role badge */
--color-chat-user:        color-mix(in srgb, var(--color-accent) 90%, transparent); /* user bubble */
--color-chat-user-text:   hsl(240 100% 100%);          /* user bubble text (always dark) */

/* Accent color (user‑configurable, default blue) */
--color-accent:           #3B82F6;

/* Spacing */
--spacing-nav:            52px;   /* left navigation bar width */
--spacing-agent-list:     240px;  /* Agent list default width */
--spacing-results:        320px;  /* Results Panel default width (actual code uses 340px, see §4.1) */
--spacing-chat-min:       360px;  /* Chat Panel minimum width (actual code uses 288px, see §4.1) */

/* Font sizes (scalable, see §6.5) */
--text-xs:    0.75rem;
--text-sm:    0.875rem;
--text-base:  1rem;
--text-lg:    1.125rem;
--text-xl:    1.25rem;

/* Border radii (4 levels) */
--radius-sm:  4px;   /* buttons, inputs, badges */
--radius-md:  6px;   /* cards, dialogs, message blocks */
--radius-lg:  8px;   /* popovers, banners */
--radius-xl:  12px;  /* panel containers, layout shells */

/* Animation */
--duration-fast:   150ms;
--duration-normal: 250ms;
--duration-slow:   400ms;

/* Dark mode: Tailwind class‑based (.dark class + @custom‑variant),
   colours auto‑flip via the semantic tokens above. */
```

### 2.2 Window Specifications (`src-tauri/tauri.conf.json`)

| Property | Value |
|----------|-------|
| Default size | 1200 × 800 |
| Minimum size | 1024 × 600 |
| Transparency | `transparent: true` |
| Title bar | `titleBarStyle: "Overlay"` + `hiddenTitle: true` (macOS uses native traffic‑lights) |
| Startup visibility | `visible: false`, shown by `getCurrentWindow().show()` after React first‑render (to avoid white‑screen/decorations flicker) |
| Single instance | Yes (Tauri built‑in) |
| Close behaviour | **Hide to tray** (`CloseRequested` intercepted: `window.hide()` + `api.prevent_close()` only when window visible; see §3.3). Real exit is only possible through the tray "Quit ACowork" menu |

### 2.3 Responsiveness

The current implementation **does not use CSS media query breakpoints**. Panel widths are manually adjusted via **drag handles** and persisted to `localStorage`:

| Panel | Default | Min | Max | Persistence key |
|-------|---------|-----|-----|-----------------|
| Agent List (sidebar) | 240px | 100px (collapses to 64px avatar‑only when dragged below 100px) | 400px | `acowork-sidebar-width` |
| Results Panel (right) | 340px | 200px | 600px | `acowork-right-width` |
| File Editor | 450px | 200px | No fixed cap (dynamic: window − sidebar − right panel − chat minimum 338px) | `acowork-file-width` |

On window resize: sidebar and Results Panel maintain absolute widths; Chat Panel and File Editor scale proportionally (changes <5% ignored to avoid jitter).

---

## 3. Global Components

### 3.1 Top Title Bar (`TitleBar.tsx`, height 32px)

```
┌──────────────────────────────────────────────────────────────────┐
│            ACowork                          [— □ ✕] (Win/Linux only)│
└──────────────────────────────────────────────────────────────────┘
```

| Element | Position | Description |
|---------|----------|-------------|
| "ACowork" brand text | Left | `text‑xs`, `data‑tauri‑drag‑region` for native dragging; double‑click maximise handled by system |
| Window controls | Right | **Windows/Linux only** render custom minimize/maximize/close buttons; macOS uses native traffic‑lights (80px left margin) |

**Note**: The title bar **does not** have a Gateway status indicator. Gateway connection status is shown in the bottom status bar and the SplashScreen (see §3.4, §7).

### 3.2 Left Navigation Bar (`NavBar.tsx`, width 52px)

```
┌────┐
│ 👤 │  ← User avatar (click → Settings → Profile tab)
├────┤
│ 💬 │  ← Chat (default view)
│ 📋 │  ← Projects (board icon, currently placeholder)
│ 📄 │  ← Docs (document icon, currently placeholder)
│ 🧩 │  ← Harness (puzzle icon, Provider/model management)
├────┤
│    │  (flex spacer)
├────┤
│ ⚙️ │  ← Settings (bottom)
└────┘
```

**Navigation items** (`NavView = "chat" | "projects" | "docs" | "harness" | "settings"`):

| Item | Icon (unselected/selected) | Description |
|------|----------------------------|-------------|
| User avatar | 40px avatar (`UserAvatar`) | Click navigates to Settings → Profile tab; hover shows ring |
| Chat | bubble (outline/solid) | Default view |
| Projects | board (outline/solid) | Currently renders "TODO" placeholder |
| Docs | document (outline/solid) | Currently renders "TODO" placeholder |
| Harness | puzzle (outline/solid) | Provider / Search / MCP / Embedding / LSP management |
| Settings | gear (outline/solid) | Bottom, separated from top group by `flex‑1` |

| Rule | Description |
|------|-------------|
| Selected state | Icon switches to filled variant, coloured with `currentColor` |
| Unselected state | Outline variant |
| Hover | `NavButton` rounded background highlight |
| Tooltip | `position="right"`, shown on hover |
| Accessibility | `role="navigation"` + `aria‑label` |

All view switching is driven by `AppLayout.currentView` state; non‑Chat views keep a 40px empty placeholder on the right to maintain visual symmetry.

### 3.3 System Tray (`src-tauri/src/tray/`)

**Current implementation is minimal**:

| State | Behaviour |
|-------|-----------|
| Menu | Only one item: **"Quit ACowork"** (kills local Gateway process tree before exiting, see `tray/events.rs`) |
| Left‑click | Restore and focus main window (`unminimize → show → set_focus`, like WeChat) |
| Right‑click | System automatically pops up the menu |
| Tooltip | Static `"ACowork"` (no dynamic status) |
| Icon | Embedded `icon.png` |

**Closing the window = hide to tray**: `CloseRequested` event is intercepted (when window visible) → `window.hide()` + `prevent_close()`, keeping the app in tray; only the tray Quit menu (or system exit) really terminates the process. This gives the tray a dual role of **window restore entry** + **persistent host**.

**Note**: The dynamic menu items described in v1.0 ("Show Dashboard / Agent Chat / Status / Start Gateway") and coloured status icons are **not implemented**.

### 3.4 Bottom Status Bar (`AppLayout.tsx` inline, height 24px)

Located at the very bottom of the window, carrying global status signals:

| Element | Description |
|---------|-------------|
| Status pill | `error` (red) / `warning` (amber) / `info` (grey); click to copy full text; hover Tooltip shows full content |
| Agent + context pill | Shown when selected Agent is running and Results Panel is collapsed or not on Status tab: `Agent: {name}` + `Context: {usage}% | {tokens}/{window}` (≥90% highlighted) |
| MQTT debug controls | `MqttDebugControls` (developer use, shows MQTT connection status) |
| File status cluster | When file editor is open, positioned absolutely below the file panel, shows cursor position / LSP status, etc. |

**Gateway disconnection signal**: when Gateway status is `error`, the bottom status bar shows a red status pill, and the title bar renders a `GatewayStatusChip` (see §3.5).

### 3.5 Gateway Disconnection Chip (`GatewayStatusChip.tsx`)

Rendered only when `gatewayStatus === "error"` (steady‑state offline; startup is handled by SplashScreen), pinned to the right of "ACowork" in the title bar:

```
┌─────────────────────────────────────────────────────────────┐
│ ACowork  (◉ Gateway unreachable  2 ▾)           [search][win] │
└─────────────────────────────────────────────────────────────┘
```

- Solid amber pill (`bg-amber-500`, dark `bg-amber-600`) with white text, `h-6`, darkens on hover
- **Breathing animation**: the Wifi icon carries `animate-pulse` (icon only — pulsing the label would flicker the text)
- **Never auto-dismisses** — a sleep/wake network switch leaves the gateway down until the user picks an address or a retry succeeds, so a fading toast would under-report a live problem
- Shows a candidate count badge when reachable alternatives were found, so there is something to click without opening the popover
- Click opens a popover (`w-80`, closes on outside click / Esc): status line → candidate list (one solid `btn-accent` row per address with its latency in ms, `max-h-56` vertical scroll) → action row
- Local mode (`gatewayMode === "local"`): shows "本地网关未运行" / starting shows "正在启动本地网关", provides **[Start]** + **[Retry]**
- Remote mode: shows "网关不可达", provides **[Retry]**

---

## 4. Chat View (Default View)

### 4.1 View Structure

```
┌────┬────────────┬─────────────┬─────────────────┬──────────┬────┐
│    │ Agent List │ Chat Panel  │ FileEditor(optional)│ Results  │    │
│ N  │ (240px drag)│ (flex)      │ (450px, visible │ Panel    │ R  │
│ A  │            │             │  when files open)│ (340px   │ I  │
│ V  │ [search]   │ [Session    │                 │ foldable)│ G  │
│ 5  │ ☀AgentA    │  Tabs]      │                 │ [Tab bar]│ H  │
│ 2  │  AgentB    │ [toolbar]   │                 │ Workspace│ T  │
│ p  │  AgentC    │ [message    │                 │ Status   │ 4  │
│ x  │            │  stream]    │                 │ Memory   │ 0  │
│    │ [+ Add]    │ [input      │                 │ Tools    │ p  │
│    │            │  area]      │                 │ Debug    │ x  │
│    │            │             │                 │ Setup    │    │
└────┴────────────┴─────────────┴─────────────────┴──────────┴────┘
```

**Panel order** (`AppLayout.tsx`, Chat view):

1. `NavBar` (52px)
2. `AgentList` (drag‑resizable)
3. Divider (1px drag handle)
4. `ChatPanel` (`flex‑1` elastic)
5. `FileEditorPanel` (**only when there are open files**, `openFiles.length > 0`)
6. `ResultsPanel` (rendered when `!resultsCollapsed`, foldable and resizable)
7. `RightNavBar` (40px toolbar)

### 4.2 Agent List (`AgentList.tsx`, default 240px)

#### Data Sources

| Data | Source | Refresh timing |
|------|--------|----------------|
| Agent list | Zustand `agentStore.agents` (internally calls Tauri `list_agents` command) | Initial load + after install/uninstall/clone/create + poll every 30s |
| Session title | `fetchLatestSession(id)` (async fetch latest session title for running Agents) | When Agent becomes running and title not cached |
| Session activity dot | `chatStore.agentStates` per‑session `sessionStatus` not idle | Real‑time (MQTT event‑driven) |

#### Agent Entry

```
┌────────────────────────────────────────────┐
│ [40px avatar]  Weather Agent           🟢 dot│
│             <latest session title / zzzz sleep animation>│
└────────────────────────────────────────────┘
```

| Element | Description |
|---------|-------------|
| Avatar | `AgentAvatar`, prefers manifest `avatar` / `builtin_avatar`, otherwise first‑letter gradient |
| Name | `profile.displayName ?? display_name ?? name`, single‑line truncated |
| Second line | Running: latest session title (skeleton pulse if not loaded); not running/sleeping: `zzzz` sleep animation |
| Activity dot | When running and any session is streaming/waiting_approval/paused, an accent‑coloured dot at the avatar bottom‑right (ADR‑014) |
| Selected state | `bg-[var(--color-accent)]/90 text-white` |
| Divider | Thin `nav-divider` line below all but the last item |

#### Entry Interactions

| Action | Trigger | Behaviour |
|--------|---------|-----------|
| Single click | Left click | Select Agent, Chat Panel loads its sessions |
| Double click | Left click ×2 | Stopped Agent directly starts (`startAgentAndSyncUI`) |
| Right click | Right click | Shows context menu (see below) |

**Right‑click context menu** (conditional rendering):

| Menu item | Condition | Behaviour |
|-----------|-----------|-----------|
| ▶ Start | `!running` | Start (with deduplication to prevent repeated clicks) |
| ▶ Start in Debug | `!running` | Start in DevMode (`startAgentAndSyncUI(id, true)`) |
| ⏹ Stop | `running` | Stop after confirmation |
| Details | Always | Open `AgentDetailDialog` |
| Clone | Always | Open `CloneDialog`, auto‑select cloned Agent on success |
| Publish | Always | Open `PublishWizard` (export .agent package) |
| 🗑 Uninstall | Every agent, System Agent included (ADR‑077 drops the privilege) | Uninstall after confirmation |

**Stop / Uninstall / dangerous action confirmation dialogs**: unified `ConfirmDialog` component with `destructive` red styling; Esc closes, Cancel is default focus.

**Bottom add area**: bottom `[+]` button opens a popover menu:

```
┌────────────────────────────┐
│ ✨ Create Agent            │  → opens CreateWizard (online creation)
│ ＋ Install Agent           │  → opens .agent file picker and installs
└────────────────────────────┘
```

- Multi‑node scenario: before install, `fetchNodes()` resolves online nodes; if >1 online node, the menu switches to a **node selector** (ADR‑055 §6.13.3)
- While installing, button shows loading state; on success, Toast + auto‑select new Agent
- `com.acowork.system` ships as the bundled default agent and installs/starts/uninstalls like any other agent (ADR‑077)

### 4.3 Chat Panel (`ChatPanel.tsx`, elastic width)

#### 4.3.1 Top Structure (top to bottom)

| Area | Content |
|------|---------|
| Session Tab Bar (`SessionTabBar`) | Multi‑session tabs for current Agent's `openSessionIds`; each tab has title + close button; includes "New Session" entry |
| Toolbar | **Model selector** (Layers icon, popover lists models + capability tags + "Add Model"), **Reasoning effort** (Brain icon: Auto/Off/Low/Medium/High), **Workspace** (folder icon), **Skills** (puzzle/skills), **Upload** (file/image, 50MiB pre‑check), **Send/Stop**, etc. |
| Message stream | Virtualised list (`VirtualMessageList`), scroll snapshot restoration |
| Input area | Multi‑line `textarea` (Enter sends / Shift+Enter newline / IME composition 300ms protection), with Send/Stop buttons and context menu |

Toolbar buttons **collapse to icon‑only** on narrow width (from the leftmost button, measured by `data‑toolbar‑btn` DOM), ensuring no overlap.

#### 4.3.2 Empty / Not‑Running States

| State | Placeholder |
|-------|-------------|
| No Agent selected / no sessions | "Select an Agent to start chatting" (centred icon + text) |
| Agent stopped | "Start Agent" button (centred, click to start) |
| Agent running but not ready | "Connecting to agent..." (when MQTT not connected) |

#### 4.3.3 Message Stream and Message Types

Messages are virtualised by `VirtualMessageList`, grouped by session (`messageFolder.ts`). Main message types (see `lib/types.ts`):

| Type | Presentation |
|------|--------------|
| `user` | User bubble (`chat-user` accent 90% mix), right‑aligned |
| `assistant` | Markdown rendered (`chat-bubble` surface), includes code blocks (`CodeBlock`), Mermaid (`MermaidBlock`), tables, LaTeX |
| `think` / `thought` | Think block (foldable) |
| `tool_call` / `tool_result` | Tool call block (expandable parameters/result JSON) |
| `error` | Error message block |
| `system` | System message (session creation/cleanup, small centred text) |
| `compaction` | Context compaction card (`CompactionCard`) |

Additional message components: `AskQuestionCard` (Agent asks user), `RetryWaitBanner`, `DebugPausedBanner`, `ExploreBlock`, `StreamingSourceBlock`, `ThinkBlock`.

**Attachments**: Users can paste/upload files and images (`UserWithAttachmentsBubble`); top `AttachedContextChips` show mounted context; Agent‑referenced files show `DocumentChip`.

#### 4.3.4 Streaming Output

Streaming events are delivered via **MQTT** (not WebSocket, ADR‑033), with `chatStore` maintaining per‑session message deltas:

1. Chunk received → appended to current message incrementally
2. Done received → streaming ends, token statistics updated (`tokenUsage`, `contextUsage`)
3. Tool call events → insert/fill tool blocks

MQTT connection status is maintained by Rust `rumqttc` event loop and pushes `mqtt‑status` events; the frontend only reflects the status (on sleep/wake, the frontend can trigger `force_reconnect_mqtt` to actively reconnect).

#### 4.3.5 Input Area

| Rule | Description |
|------|-------------|
| Send | **Enter** (Shift+Enter for newline); pressing Enter while sending enqueues the message for the next round, does not interrupt streaming |
| Stop | Must click Stop button (Enter does not trigger stop, to avoid accidental interruption) |
| IME protection | Enter within 300ms after `onCompositionEnd` is treated as IME confirmation, not send |
| Multi‑line | Scrolls within `max‑h‑48` |
| Disabled conditions | Disabled when Gateway is disconnected / Agent not running / MQTT disconnected; placeholder changes accordingly |
| Paste | Pasting images/files auto‑uploads (50MiB pre‑check) |
| Context menu | Custom copy/paste menu (Tauri WebView has no native menu) |

### 4.4 Results Panel (`ResultsPanel.tsx` + `RightNavBar.tsx`, default 340px)

The right‑side Results Panel consists of a **40px toolbar (RightNavBar)** + content area, with multiple tabs:

| Tab | Icon | Show condition | Content |
|-----|------|----------------|---------|
| Workspace | folder | Agent running | `WorkspaceExplorer` (file tree / browse / locate) |
| Status | dashboard | Always | Agent runtime status, token stats, model/Provider, session stats (see below) |
| Memory | database | Agent running | `MemoryPanel` (memory management) |
| Debug | bug | Agent running | `DebugPanel` (DevMode debugging, see §4.4.2) |
| Setup | gear | Agent running | `AgentSetupTab` (Agent configuration; hosts the builtin-tools / MCP / prompts-and-skills cards, see §4.4.3) |

**Interaction rules**:
- Clicking an already‑active tab's toolbar button → collapses the panel; clicking again → expands
- When Agent stops, Workspace/Memory/Debug/Setup buttons hide, panel auto‑jumps back to Status (see `AppLayout` lifecycle effect)
- The rail has **5 tabs**: the former Tools tab is gone (its builtin-tools / MCP cards moved into Setup), and the Debug tab's prompts-and-skills card moved there as well
- When Agent enters debug mode, panel auto‑switches to Debug tab
- The top title bar shows the current tab name; left side is the drag‑resize handle

#### 4.4.1 Status Tab

Displays current session/Agent statistics: token usage (`tokenUsage`), context usage (`contextUsage`, with ADR‑028 historical accumulation fallback), iteration count, model/Provider, reasoning effort, temperature, session count, compaction status, etc.

#### 4.4.2 Debug Tab (DevMode)

| State | Presentation |
|-------|--------------|
| Agent not running | "No Agent is in debug mode" empty state |
| Running but DevMode off | "Enable Debug" button (`enable_agent_debug` runtime, no restart needed) |
| DevMode on + remote Gateway | "Debugging is not available in remote mode" (ADR‑048 D6: debug RPC depends on local MQTT) |
| DevMode on + local + disconnected | "Debug connection lost" |
| Ready | `DebugPanel`: control bar (Pause/Resume, Step, Stop, Restart, Exit Debug, Re‑execute) + status cards (iteration/phase/Token/session state) + context snapshot list (expandable sections, inline editing, patch) |

#### 4.4.3 Setup Tab cards

The Setup tab is the single entry point for agent-level configuration — seven level‑1 collapsible cards, top to bottom:

| # | Card | Content |
|---|------|---------|
| 1 | Agent Info | Avatar (ADR-017, server‑side), display name |
| 2 | Session Parameters | Session language, max iterations / sessions, shell approval, compression ratio |
| 3 | Model Parameters | Max output tokens, context window, temperature |
| 4 | Chat Session Router | Router model / timeout / history budget (ADR‑099) |
| 5 | Prompts & Skills | Browse and edit the package's `prompts/` + `skills/`, plus reload (was the top card of the Debug tab, ADR‑063 §3.7) |
| 6 | Builtin Tools | Builtin tool switches (ADR‑029, including ADR‑094's `load_skill`) |
| 7 | MCP Servers | Per‑server switches (ADR‑069 / ADR‑090); each server expands to its tool list |

One more, **conditionally rendered**, card sits between 6 and 7: **MCP servers recommended by the package** (`McpRecommendationGroup`, ADR‑104). It appears only when the package's `manifest.toml` declares `[[mcp_servers]]` recommendations, and installing one drops that row into the switch list below — the two have to be on screen together. It is not one of the seven and takes no part in the "first 3" position rule.

Only the **first 3** cards start expanded; the rest start collapsed (order and defaults are defined once in `agentSetupCards.ts`). Expand state is component‑local, not persisted.

**Known exception**: the recommendation card starts **expanded** (its own component‑local initial value, outside the rule). It only renders when recommendations exist, and then it is the notice that "this agent is still missing something" — collapsing it would reduce that notice to a title bar. The cost is that the panel is then no longer strictly "only the first 3 expanded". Rationale and the future unification: ADR‑108 D3.

### 4.5 File Editor (`FileEditorPanel.tsx`, optional)

- **Only rendered when there are open files** (`openFiles.length > 0`)
- Default width 450px; on first open the last dragged width is restored when one exists, otherwise the pane splits the available space 50/50 with the chat. Drag‑resizable from 200px with no fixed cap (bound: see §2.3 — window − sidebar − right panel − chat minimum 338px)
- Based on Monaco Editor; supports multi‑file tabs, Markdown/image/HTML preview, LSP, global search, symbol search, GoToFile
- Bottom status cluster shows cursor/selection/LSP status (absolutely positioned above the global status bar)

---

## 5. Harness View (Provider / Model Management)

The Harness entry (puzzle icon) in the navigation bar opens `HarnessPage`, with 5 tabs:

| Tab | Content |
|-----|---------|
| Providers | Provider API Key management (`AddProviderFlow` / edit dialog), model capability configuration (`ModelMultiSelect`), global default model (`GlobalCompactModelCard`) |
| MCP | MCP server management (`McpTab`, with `MCP_PRESETS`) |
| Embedding | Embedding model configuration (`EmbeddingModelTab`) |
| LSP | LSP server management (`LspTab`) |

**Data sources**: API Keys via Tauri `list_keys` / `add_key` / `remove_key` / `update_key` commands (vault); Provider list from Gateway's `offline_providers.json` (`fetchProviders()`); model list from `fetchProviderModels(providerId)`.

**Note**: The standalone "Models view" and "Vault/Providers Settings tab" described in v1.0 are **deprecated** — Provider management is now centralised in the Harness view.

---

## 6. Settings View (`SettingsPage.tsx`)

5 tabs (tab switching uses CSS `display` to preserve component state):

| Tab | Description |
|-----|-------------|
| Profile | User identity: display name, avatar (upload / built‑in), language, timezone, city, occupation |
| General | Logs: Gateway log level, frontend log level, log file size/limit, delete logs; data directory (read‑only, from `/api/config`); About; Reset Onboarding |
| Appearance | Theme (light/dark/system), accent colour (`ACCENT_PRESETS` palette), content width (40–100%), font size (S–XXL), window transparency (slider 0–100%) |
| Gateway | Run mode (local/remote), local status & Start/Stop/Restart, remote URL + Test Connection, connected Agents list (with model info, Debug badge) |
| Nodes | Node topology (`GET /api/nodes`): node_id / online status / OS / architecture / version / hostname / Agent count (read‑only table) |

### 6.1 Appearance Details

| Setting | Values | Description |
|---------|--------|-------------|
| Theme | light / dark / system | system follows macOS appearance (`matchMedia` listener, `settingsStore.osTheme` sync) |
| Accent colour | preset palette (`ACCENT_PRESETS`) | writes `--color-accent` + `accent‑{id}` class, affects glassmorphism tint, message bubbles, selection states |
| Content width | 40/50/60/70/80/90/100% | controls max‑width of main content area |
| Font size | S(0.75)/M(0.875)/L(1.0)/XL(1.125)/XXL(1.25) | writes `--ui‑font‑size`; global shortcuts Ctrl+= / Ctrl+- step through |
| Transparency | 0–100% | macOS native `NSVisualEffectView` (`set_window_effect`) + CSS glass tint double‑layered; macOS has a minimum opacity floor for consistency |

### 6.2 Gateway Details

- **Run mode**: local (Tauri manages Gateway subprocess) / remote (connect to user‑configured URL)
- local: status indicator (Running/Starting/Stopped), [Start] (when externally stopped) / [Restart] [Stop] (when Tauri‑managed), version
- remote: URL input (save on blur/Enter), [Apply], status indicator, [Test Connection]
- **Connected Agents**: lists Agents from `/api/agents` where `running || connected`; each row shows name, `provider/model`, and Debug badge

---

## 7. First‑Run Onboarding (`OnboardingFlow.tsx`)

### 7.1 Flow Overview

```
Step 1: Welcome ──→ Step 2: Gateway ──→ Step 3: API Key ──→ Step 4: Identity ──→ Step 5: Install Agent
                                                                    │
                                                           Skip/Finish → Main UI
```

- Progress bar: 5‑segment (`bg‑zinc‑800` for completed, `bg‑zinc‑200` for pending)
- Onboarding state persisted: `localStorage["acowork_onboarding"] = "completed"`
- Top area shows progress bar + "Step X of 5" label
- Modal overlay covers full screen (`fixed inset‑0 z‑50`), centred `max‑w‑md`

### 7.2 Step 1: Welcome

Brand logo + welcome message + **[Start Configuration]** (→ Step 2) + **[Skip Onboarding]** (directly complete, enter main UI).

### 7.3 Step 2: Gateway Connection

**Differences from v1.0**: adds **local/remote mode selection** (RadioGroup):

- **Local mode** (recommended): shows status (Starting/Connected/Not started/Failed); on failure shows ErrorBox + [Start Local Gateway] button; [Next] enabled once connected
- **Remote mode**: URL input (placeholder default `http://127.0.0.1:19876`) + [Apply] + status indicator + [Test Connection]
- `canProceed = status === "connected"` (same for both modes)

### 7.4 Step 3: API Key Configuration

**Differences from v1.0**: Provider dropdown comes from **dynamic Provider list** (Gateway `fetchProviders()`), more configuration fields:

```
┌──────────────────────────────────────────────────────────────┐
│  🔑 [Provider ▼]                                             │
│  API Key    [password input]                                  │
│  Base URL   [text input]                                      │
│  Model multi‑select [ModelMultiSelect: capability filter / custom model entry] │
│                [Save] (shows "Saved" after)                   │
│  ──────────────────────────────────────────────────────────  │
│  🏠 Local Providers (no key needed): list local Provider names│
└──────────────────────────────────────────────────────────────┘
```

- `needsApiKey(provider)` determines whether Key input is shown; local Providers do not need a key
- Base URL auto‑fills the Provider's `api` endpoint on selection
- Model multi‑select reuses the Harness `ModelMultiSelect` component; saves the full model list (not just default)
- Save: Tauri `add_key` command; on success dispatches `models‑added` event (ChatPanel listens to refresh model list)
- **[Next]** is always available (can skip; no v1.0 hard requirement of "at least one Provider available")

### 7.5 Step 4: Identity Information

| Field | Required | Control |
|-------|----------|---------|
| Name | Yes | Text input |
| Language | Yes | Dropdown (zh‑CN / zh‑TW / en / ja / ko) |
| Timezone | Yes | Dropdown (Asia/Shanghai / Asia/Tokyo / America/New_York / America/Los_Angeles / Europe/London / UTC) |
| City | No | Text input |
| Occupation | No | Text input |

- `requiredFilled = name && language && timezone`; [Next] disabled until filled
- On completion, calls `createUser(...)` (fire‑and‑forget), syncs local `userProfileStore`; if no avatar, assigns a random built‑in avatar

### 7.6 Step 5: Install First Agent

**Differences from v1.0**: recommended Agent list is **6 built‑in roles** (instead of v1.0's Weather/Calendar), with **batch multi‑select installation**:

| Resource name | Display name | Role | Description |
|---------------|--------------|------|-------------|
| software‑architect‑agent | Architect | Software Architect | System design, architectural review, technical planning, risk assessment |
| senior‑engineer‑agent | SSE | Senior Software Engineer | Code review, architectural design, debugging, refactoring, testing, documentation |
| quality‑assurance‑agent | QA | Quality Assurance Manager | Quality strategy, test planning, defect management, release validation |
| project‑manager‑agent | PM | Project Manager | Requirements analysis, task breakdown, progress tracking, risk management |
| product‑manager‑agent | Product | Product Manager | Product strategy, user research, PRD writing, roadmap, release planning |
| document‑manager‑agent | Docs | Document Manager | Document collection, organisation, writing, conversion, knowledge base maintenance |

- Each item is a checkbox card (Name · Role / Description); all pre‑checked by default; [Select All] / [Clear All] provided
- **[Install Selected (N)]**: `waitBootstrapReady()` (poll `/api/bootstrap` until READY) → `runBounded(items, 3)` concurrent 3 submissions of `install_bundled_agent` → for each item `wait_agent_installed` polling → per‑item status badges (pending/submitted/completed/failed, including operation_id)
- **[Install from File]**: opens .agent file picker, calls `install_agent`
- **[Finish]**: available whether or not any Agent installed; sets `onboarding_completed`, enters main UI

---

## 8. Error Handling UX

### 8.1 Toast Notifications (`ToastProvider.tsx`)

All non‑fatal errors and successes are shown via Toast.

| Property | Value |
|----------|-------|
| Position | Bottom‑right |
| Types | success / error / warning / info |
| Stacking | Max 3 visible; new pushes in, old ones disappear early |
| Auto‑dismiss | Success: shorter / Error: longer |

### 8.2 Loading States

| Component | Loading state |
|-----------|---------------|
| Agent list initial load | Centred Spinner (`animate‑spin`) |
| Agent session titles | Skeleton second line (`animate‑pulse` rectangle) |
| Install / start | Button loading / disabled + Toast feedback |
| Panel data | Tab‑internal "loading" text |

### 8.3 Network / Connection Errors

- Gateway `error` → `GatewayStatusChip` in the title bar (§3.5) + red status pill in bottom bar
- MQTT disconnected (while Agent running) → warning pill in bottom bar ("Realtime connection lost, retrying…"); sleeping Agents do not show this (expected behaviour)
- Any async operation failure → Toast + error details

---

## 9. Animations and Transitions

| Scenario | Animation |
|----------|-----------|
| Navigation/list selection | Background‑colour transition 150ms |
| Panel collapse/expand | Instant width switch (no transition animation) |
| Toast | Slide‑in from right + fade‑in / fade‑out |
| Streaming typing | No blinking cursor (block‑level rendering) |
| Agent status change | No colour gradient (instant switch) |
| Welcome / startup | SplashScreen fade‑in (700ms translate+opacity), LoadingDots every 400ms |
| Accent colour change | Instant effect |

**prefers‑reduced‑motion**: CSS‑level `transition‑all` can be degraded by Tailwind media queries when system reduces motion.

---

## 10. Keyboard Shortcuts

| Shortcut | Context | Behaviour |
|----------|---------|-----------|
| `Enter` | Input area | Send message (Shift+Enter for newline) |
| `Ctrl/Cmd + =` | Global | Increase font size (step S→XXL) |
| `Ctrl/Cmd + -` | Global | Decrease font size |
| `Escape` | Dialog/popover | Close |
| `F5` / `Ctrl+R` / `Ctrl+N` etc. browser shortcuts | Global | **Blocked** (prevents page refresh/reload, see `main.tsx` BLOCKED_SHORTCUTS) |
| `Ctrl+Shift+P` | Global | Blocked (browser print) |

**Note**: v1.0 shortcuts `Ctrl/Cmd + Enter`, `Ctrl+N` install, `Ctrl+,` Settings, `Ctrl+Shift+D` DevMode, `Ctrl+R` refresh list, etc. are **not implemented**.

---

## 11. Frontend ↔ Backend Contract Summary

### 11.1 Tauri Commands (`invoke(...)`, via `withGlobalTauri`)

| Frontend action | Command | Description |
|-----------------|---------|-------------|
| List Agents | `list_agents` | `agentStore.fetchAgents()` |
| Install Agent | `install_agent` | `{ packagePath, devMode, nodeId }` |
| Install built‑in Agent | `install_bundled_agent` | `{ resourceName, devMode }` → `OperationAck` |
| Wait for install completion | `wait_agent_installed` | `{ agentId, timeoutSecs }` |
| Uninstall Agent | `uninstall_agent` | `{ agentId }` |
| Start Agent | `start_agent` | `{ agentId, devMode }` |
| Stop Agent | `stop_agent` | `{ agentId }` |
| Restart debug | `restart_agent_in_debug` | `{ agentId }` |
| Clone Agent | `clone_agent` | |
| Create Agent | `create_agent` | |
| Publish/export | `prepare_publish` / `build_publish` / `export_package` | |
| Vault Keys | `list_keys` / `add_key` / `remove_key` / `update_key` / `list_search_keys` / `add_search_key` | |
| Debug | `enable_agent_debug` / `disable_agent_debug` / `debug_rpc` | |
| Gateway | `set_gateway_config` / `get_gateway_config` / `init_local_gateway` / `start_local_gateway` / `stop_local_gateway` / `get_local_gateway_status` / `get_bootstrap` | |
| MQTT | `connect_mqtt` / `disconnect_mqtt` / `force_reconnect_mqtt` / `get_mqtt_status` / `mqtt_subscribe_agent_session` / `mqtt_unsubscribe_agent_session` / `mqtt_publish_control` | Real‑time messages/control go via MQTT (ADR‑033), **not WebSocket** |
| File | `upload_file` / `get_file_size` / `upload_agent_file` / `upload_user_avatar_file` / `update_agent_manifest_avatar` | |
| System | `reveal_in_file_explorer` / `set_window_effect` / clipboard | |

### 11.2 Gateway HTTP API (direct `fetch(...)`)

| Frontend action | Method | Path |
|-----------------|--------|------|
| Health check | GET | `/health` |
| Bootstrap status | GET | `/api/bootstrap` |
| Config | GET/PUT | `/api/config` |
| Nodes | GET | `/api/nodes` |
| Agent list | GET | `/api/agents` |
| Agent detail | GET | `/api/agents/{id}` |
| Agent model | GET | `/api/agents/{id}/model` |
| Agent status | GET | `/api/agents/{id}/status` |
| Delete logs | DELETE | `/api/logs` |
| Create user | POST | `/api/users` (`createUser`) |
| Providers | GET | `/api/providers` |
| Provider models | GET | `/api/providers/{id}/models` |

---

## 12. State Management (Zustand Stores Overview)

| Store | Responsibility |
|-------|----------------|
| `settingsStore` | theme/osTheme/accentColor/fontSize/contentWidth/opacity/gatewayMode/gatewayUrl/logLevel, persisted to localStorage |
| `gatewayStore` | status/health/localState + checkHealth/startLocalGateway/stopLocalGateway/checkLocalStatus |
| `agentStore` | agents(including meta/profile/sessions/sessionTitle/tokenTotals), selectedAgentId, fetchAgents/selectAgent/install/uninstall/start/stop/clone |
| `chatStore` | agentStates → sessionStates (messages/tokenUsage/contextUsage/sessionStatus/model/provider/inputValue), mqttConnected, MQTT event handling |
| `debugStore` | debug session state, snapshots, connect/disconnect/resume/pause/step/stop/restart/rewind/reExecute/patchContext |
| `layoutStore` | activePanelTab/resultsCollapsed/filePanelBounds |
| `workspaceStore` | workspace state, locateRequest |
| `fileEditorStore` | openFiles/activeFileId |
| `fileTreeStore` | file tree |
| `editorStatusStore` | cursor/selection/LSP status |
| `statusBarStore` | status bar message/type/visible/setStatus/clearStatus |
| `userProfileStore` | user profile (displayName/avatar) |
| `skillStore` | skills |
| `mcpStore` | MCP servers |

---

## 13. Accessibility

| Rule | Implementation |
|------|----------------|
| Keyboard navigation | All interactive elements focusable via Tab |
| ARIA labels | NavBar `role="navigation"`, Agent list `role="list"/"listitem"`, drag handles `role="separator"`, inputs/buttons `aria‑label` |
| Focus indicators | Focusable elements have `focus‑visible:ring` |
| Contrast | Primary text zinc‑700/zinc‑900 (light) / zinc‑300 (dark), ≥4.5:1 |
| Tooltips | `Tooltip` component (delay, direction controllable) |

---

## 14. Relationship to Design Documents

| Document | Relationship |
|----------|--------------|
| `docs/design/14-desktop-app.md` | Architecture, technology choices, window management — this document refines interaction atop it |
| `docs/design/10-debug-protocol.md` | Developer mode / debug protocol — Debug Tab interaction basis |
| `docs/design/13-skill-system.md` | Skill system — ChatPanel toolbar Skills entry, SkillsPanel |
| `docs/_internal/archive/plan/zh/plan-p5.md` | S1 task definitions (archived) |