# AGENTS.md — ACowork

## Overview

ACowork is a decentralized, high-security, scalable AI Agent runtime platform. Each Agent is a declarative `.agent` package (config + prompts + skills, no binary), loaded by a universal Runtime binary and managed by a Gateway.

## Build & Test

### Core (Rust workspace)

```bash
cd core
cargo build --release          # all workspace members
cargo clippy --all-targets -- -D warnings
cargo test                     # unit + integration tests
# ./dev/ci.sh all              # check + clippy + test + integration
```

### Desktop App (Tauri v2)

Needs the core binaries (gateway / runtime / embed / doc / lsp-relay / node / pm / user / memory) — `core:build:debug|release` produces them.

```bash
# Release — beforeBuildCommand auto-runs core:build:release + npm build + tauri bundle
cd apps/acowork-desktop && npm run tauri build

# Dev — build core debug once, then HMR + Tauri window
cd apps/acowork-desktop
npm run core:build:debug
npm run tauri dev
```

### Mobile App (Tauri v2)

Same core binaries as Desktop. Mobile has no local Gateway process (v1).

```bash
cd apps/acowork-mobile
npm run dev          # browser dev (Vite :19877 — use desktop devtools for touch)
npm run tauri:dev    # native shell
```

Android: use `dev/android/build_mobile.ps1` (`-Install`, `-ReleaseApk`) — the script owns `tauri android init`, the launcher icons, the CAMERA permission and the signing config. Details: [apps/acowork-mobile/README.md](apps/acowork-mobile/README.md)

## Logs

`{HOME_DIR}` = current user home folder.

- Gateway: `{HOME_DIR}\.acowork\acowork-gateway\data\logs`
- Desktop: `{HOME_DIR}\.acowork\desktop-app\logs`
- Services: `user.log` / `memory.log` in the Gateway log dir; memory instances under `{HOME_DIR}\.acowork\acowork-memory\instances\{instance_id}`

Operator runbooks: [docs/runbooks/](docs/runbooks/)

## Project Structure

```
core/                  # Rust workspace — members are the source of truth: core/Cargo.toml [workspace]
  acowork-core/        # Shared types, errors, config, MQTT proto
  acowork-eval/        # Retrieval-eval kernel — dev-dependency only, never shipped
  acowork-embed/       # ONNX-Runtime embedding model runner
  acowork-gateway/     # HTTP API, embedded MQTT broker, reverse proxy, lifecycle, package mgr
  acowork-lsp-relay/   # LSP protocol relay (Node-spawned loopback sidecar)
  acowork-mcp/         # MCP (Model Context Protocol) wrapper
  acowork-memory/      # Memory service — lib + bin: MCP + panel REST (ADR-101)
  acowork-mqtt-session/# MQTT session / event multiplexing (Gateway <-> Runtime)
  acowork-pm/          # Project & task management service (REST API + MCP tools)
  acowork-node/        # Node Agent — per-machine daemon hosting Runtime processes (ADR-055)
  acowork-doc/         # Online document library service (REST API + MCP tools)
  acowork-user/        # User domain service — accounts, credentials, roles, profiles (ADR-084)
  acowork-relay/       # Cloud thin relay — BYO self-hosted tier, not bundled into Desktop builds
  acowork-runtime/     # Agent runtime (main loop, tools, providers, sessions)
  acowork-sign/        # Package signing & verification
  acowork-sqlite/      # Runtime-side SQLite sidecars — session meta + conversation index (ADR-082)
  acowork-tool-sdk/    # WASM custom tool SDK (Wasmtime host)
  acowork-vault/       # Encrypted key/value store

apps/
  acowork-desktop/     # Tauri v2 desktop app (frontend + thin Rust backend, system tray)
  acowork-mobile/      # Tauri v2 mobile app (ADR-086) — IM-style chat + multi-session

docs/                  # Public architecture docs
  design/{zh,en}/      # Design docs
  module-design/{zh,en}/ # Rust crate specs
  adr/{zh,en}/         # Architecture decision records
  prd/{zh,en}/         # Platform + Desktop UI/UX PRD
  protocols/{zh,en}/   # HTTP + MQTT + RAG protocol reference
  runbooks/            # Operator runbooks

examples/              # Example .agent packages
dev/                   # Build / package / CI scripts
```

Crate specs: [docs/module-design/](docs/module-design/) · Design: [docs/design/](docs/design/)

## Architecture

```
Desktop App (Tauri v2) — separate process
  React/TS UI (no state persistence) · system tray · HTTP :19876 · MQTT :19875
        |
        v
Gateway (keep-alive process, Rust)
  HTTP API :19876 (Axum, localhost) · embedded MQTT broker :19875 (rumqttd)
  Reverse proxy -> Runtime localhost HTTP · package mgr · lifecycle mgr
  Memory service (acowork-memory, ADR-101): supervised process :18084
  LSP reverse proxies: /lsp (relay passthrough) + /lsp-admin (Node registry, ADR-091)
        |
        | MQTT pub/sub :19875
        v
Node Agent (acowork-node, ADR-055) — per-machine daemon
  MQTT control plane (acowork/nodes/#) · process table (spawn/kill/reap Runtime)
  Local package mgr · identity/enrollment · reverse proxy :19900
        |
        | MQTT pub/sub :19875
        v
Agent Runtime (universal binary, Rust)
  MQTT client + localhost HTTP · system agent · user agents
  Zero memory business code (ADR-101): memory tools come from the injected memory MCP
```

Deployment topology (single-machine remote, accounts, relay): [docs/runbooks/](docs/runbooks/)

## Conventions

- Design docs zh + en; Rust code comments (`//`, `//!`, `///`) MUST be English
- ADRs: the Chinese document is the source of truth, and every Chinese ADR has an English counterpart that must be updated in the same commit (rule and conventions: [docs/adr/en/README.md](docs/adr/en/README.md))
- Desktop App = presentation/interaction only — no business logic, no state persistence
- **Gateway boundary = communication + resource management + reverse proxy.** Agent-private data (skills, prompts, conversations, per-agent embedding, `{install_path}/assets`) is read and written **only through Runtime HTTP** (`core/acowork-gateway/src/http/proxy.rs`), never by direct filesystem access — `install_path` is node-local, so a Gateway-side `fs` read breaks across hosts. Enforced by `run_gateway_fs_redline` in `dev/ci.sh`; rationale and the frozen ceilings: ADR-009 §5.
- **Memory is the service's, not the Runtime's** (ADR-101). Panel data reaches the memory service only through `memory_proxy` (`core/acowork-gateway/src/http/memory_proxy.rs`); the Gateway MUST NOT open `instances/*/memory.db`. The one exception is the Node-side migration pass (node-local fs owner → the service's `/admin/import`).
- **Chat transcript rendering is windowed and memoized.** Surfaces render through `apps/acowork-desktop/src/components/common/VirtualRowList.tsx`; rows stay `React.memo` with stable prop identities; prepend anchoring stays at the scroll-container level. Never locate a transcript row by DOM selector (`[data-seq]`) — off-viewport rows are not in the DOM. Rationale: `docs/plan/zh/chat-transcript-render-perf-plan.md`.
- **Identity-preserving store updates.** A store write path whose new content equals what subscribers already hold must not produce a new object identity. Rationale: `docs/plan/zh/chat-transcript-render-perf-plan.md`.
- **Git commit messages follow Conventional Commits, one line, English only:** `feat(<scope>): <summary>` / `fix(<scope>): <summary>` — the scope is the crate or app touched (`desktop`, `pm`, `runtime`, `gateway`, `node`, `memory`, `session`, `providers`, `ui`); types in use: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`. A bug fix is `fix(...)`, **not** `bugfix(...)`. Subject in the imperative mood, ≤ 72 chars, no trailing period; the rationale goes in the body.

## Rules (Do NOT)

- Do NOT act before the user confirms your plan.
- Do NOT kill gateway or runtime process when testing — you are running inside it.
- Do NOT commit in Chinese.
- Do NOT wrap file paths in quotes — use Markdown links relative to the working directory: `[src/lib/foo.ts](src/lib/foo.ts)`.
