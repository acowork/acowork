# AGENTS.md — ACowork

## Overview

ACowork is a decentralized, high-security, scalable AI Agent runtime platform. Each Agent is a declarative `.agent` package (config + prompts + skills, no binary), loaded by a universal Runtime binary and managed by a Gateway.

## Build & Test

### Core (Rust workspace, 17 crates)

```bash
cd core
cargo build --release          # all 17 crates
cargo clippy --all-targets -- -D warnings
cargo test                     # unit + integration tests
# ./dev/ci.sh all              # check + clippy + test + integration
```

### Desktop App (Tauri v2)

Required core binaries (built by `core:build:debug|release`):
`acowork-gateway`, `acowork-runtime`, `acowork-embed`, `acowork-doc`, `acowork-lsp-relay`, `acowork-node`, `acowork-pm`, `acowork-user`.

```bash
# Release — beforeBuildCommand auto-runs core:build:release + npm build + tauri bundle
cd apps/acowork-desktop && npm run tauri build

# Dev — build core debug once, then HMR + Tauri window
cd apps/acowork-desktop
npm run core:build:debug
npm run tauri dev
```

### Mobile App (Tauri v2)

Same core binaries as Desktop (see above). Mobile has no local Gateway process (v1).

```bash
# Browser dev (Vite :19877 — use desktop browser devtools to simulate touch)
cd apps/acowork-mobile && npm run dev

# Native shell dev
cd apps/acowork-mobile && npm run tauri:dev
```

**Android**: use the build script — it handles `tauri android init`, launcher icons (`src-tauri/icons/android/`), CAMERA permission (配网扫码), and the release signing config. Do not run `tauri android init` directly (it wipes `src-tauri/gen/android/` and resets icons to the template robot).

```powershell
.\dev\android\build_mobile.ps1              # debug APK
.\dev\android\build_mobile.ps1 -Install     # also `adb install` to connected device
.\dev\android\build_mobile.ps1 -ReleaseApk  # release APK
```

Full details: [apps/acowork-mobile/README.md](apps/acowork-mobile/README.md)

## Logs

`{HOME_DIR}` = current user home folder.

- Gateway: `{HOME_DIR}\.acowork\acowork-gateway\data\logs`
- Desktop: `{HOME_DIR}\.acowork\desktop-app\logs`
- Runtime: `{HOME_DIR}\.acowork\acowork-gateway\config\packages\com.acowork.senior-engineer\workspace\logs`
- User service: `{HOME_DIR}\.acowork\acowork-gateway\data\logs\user.log`

## Project Structure

```
core/                  # Rust workspace (17 crates; source of truth: core/Cargo.toml [workspace] members)
  acowork-core/        # Shared types, errors, config, MQTT proto
  acowork-embed/       # ONNX-Runtime embedding model runner
  acowork-gateway/     # HTTP API, embedded MQTT broker, reverse proxy, lifecycle, package mgr
  acowork-lsp-relay/   # LSP protocol relay (Node-spawned loopback sidecar; stateless since ADR-091 — it only reads the registry and probes)
  acowork-mcp/         # MCP (Model Context Protocol) wrapper
  acowork-memory/      # Memory manager (trait, middleware)
  acowork-mqtt-session/# MQTT session / event multiplexing (Gateway <-> Runtime)
  acowork-pm/          # Project & task management service (REST API + MCP tools)
  acowork-node/        # Node Agent (ADR-055) — per-machine daemon hosting Runtime processes; owns the LSP server registry (ADR-091)
  acowork-doc/         # Online document library service (REST API + MCP tools)
  acowork-user/        # User domain service (ADR-084) — accounts, credentials, roles, profiles/avatars, user chat
  acowork-relay/       # Cloud thin relay: SNI-routed byte pipe + yamux tunnel (design 24; BYO self-hosted tier, protocol frozen per ADR-088). Not bundled into Desktop builds.
  acowork-runtime/     # Agent runtime (main loop, tools, providers, sessions)
  acowork-sign/        # Package signing & verification
  acowork-sqlite/      # SQLite storage backend — memory + session meta + conversation index (ADR-082)
  acowork-tool-sdk/    # WASM custom tool SDK (Wasmtime host)
  acowork-vault/       # Encrypted key/value store

apps/
  acowork-desktop/     # Tauri v2 desktop app (frontend + thin Rust backend, system tray)
  acowork-mobile/      # Tauri v2 mobile app (ADR-086) — IM-style chat + multi-session
  cli/                 # Gateway CLI (planned, directory does not exist yet)

docs/                  # Public architecture docs
  design/{zh,en}/      # 23 zh / 17 en design docs
  module-design/{zh,en}/ # 8 zh / 7 en Rust crate specs
  adr/{zh,en}/         # 84 ADRs (zh: 84, en: 84 — en is the maintained translation set)
  prd/{zh,en}/         # Platform + Desktop UI/UX PRD
  protocols/{zh,en}/   # HTTP + MQTT + RAG protocol reference
  mcp-server-research/{zh,en}/
  runbooks/            # operator runbooks (single-machine remote topology, accounts, relay deploy)
  plan/ review/ prompt-audit/ prototypes/  # working documents (zh)

examples/              # Example .agent packages
dev/                   # Build / package / CI scripts
```

## Architecture

```
Desktop App (Tauri v2) — separate process
  React/TS UI (no state persistence) · system tray · HTTP :19876 · MQTT :19875
        |
        v
Gateway (keep-alive process, Rust)
  HTTP API :19876 (Axum, localhost) · embedded MQTT broker :19875 (rumqttd)
  Reverse proxy -> Runtime localhost HTTP · package mgr · lifecycle mgr
  LSP reverse proxies: /lsp (relay passthrough) + /lsp-admin (Node registry, ADR-091)
  Intent router · global resources · budget tracker · rate limiter
        |
        | MQTT pub/sub :19875
        v
Node Agent (acowork-node, ADR-055) — per-machine daemon
  MQTT control plane (acowork/nodes/#) · process table (spawn/kill/reap Runtime)
  Local package mgr · identity/enrollment · reverse proxy :19900
  LSP sidecar supervisor · LSP server registry + install executor (ADR-091) · node-local fs browse
        |
        | MQTT pub/sub :19875
        v
Agent Runtime (universal binary, Rust)
  MQTT client + localhost HTTP · system agent · user agents (private Grafeo)
  DevMode debug protocol (ADR-048)
```

## Conventions

- Design docs zh + en; Rust code comments (`//`, `//!`, `///`) MUST be English
- ADRs: the Chinese document is the source of truth, and every Chinese ADR has an English counterpart that must be updated in the same commit (rule and conventions: `docs/adr/en/README.md`)
- Workspace members source of truth: `core/Cargo.toml [workspace] members`
- Desktop App = presentation/interaction only — no business logic, no state persistence
- **Gateway boundary = communication + resource management + reverse proxy.** Agent-private data (skills, prompts, conversations, memory, per-agent embedding, `{install_path}/assets`) is read and written **only through Runtime HTTP** (`core/acowork-gateway/src/http/proxy.rs`), never by direct filesystem access. Since ADR-055 `install_path` is a node-local path, so a Gateway-side `fs` read works in a single-machine setup and returns 5xx the moment Gateway and Node are on different hosts. Enforced by `run_gateway_fs_redline` in `dev/ci.sh`; rationale and the frozen ceilings in [ADR-009 §5](docs/adr/zh/ADR-009-gateway-workspace-isolation.md).

## File Reference
Point to files with Markdown links only: e.g. `[src/lib/foo.ts](src/lib/foo.ts)` or `[src/lib/foo.ts:42](src/lib/foo.ts#L42)`. 
Do not wrap paths in quotes — they become unclickable.
Do not use absolute paths; use paths relative to the working directory

## Rules (Do NOT)

- Do NOT commit in Chinese
- Do NOT act before the user confirms your plan.
- Do NOT kill gateway or runtime process when testing — you are running inside it