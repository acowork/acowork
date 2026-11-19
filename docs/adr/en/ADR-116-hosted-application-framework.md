# ADR-116: Hosted Application Framework (Applications, Not Plug-ins) — PM / DOC as the Blueprint

**Status**: Accepted (2026-11-15; the user decided "it is more like an application than a plug-in" and authorised building the full framework with PM / DOC as the validation pair)
**Date**: 2026-11-15
**Decider**: User

**Related**:
- [ADR-064](./ADR-064-pm-standalone-process.md), [ADR-070](./ADR-070-doc-standalone-process-and-tree-storage.md), [ADR-101](./ADR-101-memory-standalone-mcp-service.md), [ADR-084](./ADR-084-user-standalone-process.md) — four applications of the standalone-process pattern this ADR generalises
- [ADR-009](./ADR-009-gateway-workspace-isolation.md) — Gateway boundary; extended here to "the Gateway never reads an app's data directory"
- [ADR-104](./ADR-104-agent-manifest-declaration-surface.md) — declarative-manifest precedent; this ADR deliberately does **not** reuse `AgentManifest`
- [ADR-112](./ADR-112-workspace-preview-capability-ticket.md) — HMAC capability ticket reused as the app session ticket model
- [ADR-069](./ADR-069-mcp-tool-level-optin.md), [ADR-090](./ADR-090-mcp-lsp-install-ownership-node.md) — catalog + activation two-layer structure
- [Design doc 34](../design/en/34-hosted-application-framework.md) — full contract, API surface and verification steps
- `AGENTS.md` §Conventions — Desktop is a presentation layer; Gateway boundary = communication + resource management + reverse proxy

> The Chinese document is the source of truth. This English counterpart is kept in the same commit by convention.

---

## 1. Decision summary

### 1.1 One sentence

**Generalise the "standalone process + standalone HTTP service + MCP over HTTP + Gateway reverse proxy" shape already shipped by PM / DOC into a declarative Hosted Application framework.** Each application owns its process, HTTP service, front end and data directory. The platform owns registration, supervision, routing, identity injection, MCP catalog publication, a navigation slot, and a deliberately **narrow** set of platform APIs. The platform does **not** expose a `vscode.*`-style host SDK that lets an app mutate platform state — that is the line between an application and a plug-in.

### 1.2 Key decisions

| # | Decision | Outcome |
|---|---|---|
| 1 | Positioning | **Application, not plug-in.** No `contributes.menus/commands/views` into host UI; no host API to change agent config, stop a Runtime or rewrite the catalog |
| 2 | First cohort | **PM + DOC**, already standalone (ADR-064/070), used as blueprint and end-to-end validation. **memory is excluded**: its UI is an embedded right-hand panel, a different UI contract; its supervisor/proxy/catalog paths are untouched |
| 3 | Declaration | New `acowork-core::app`: `app.toml` (`AppManifest`) + `AppRegistry`. Built-in PM/DOC declarations compiled in; user-installed ones layered from `{data_dir}/apps/*.toml`; enable state in `apps_disabled.json`. Ids, paths and binaries validated at the trust boundary |
| 4 | Process | `runtime = "legacy"` (PM/DOC) keeps the proven ADR-064 supervisors with their bespoke args and identity-injection tests; `runtime = "generic"` uses the framework's configuration-driven supervisor. Both project into one `resolve_app_port()` |
| 5 | Network | Apps bind `127.0.0.1` only. Desktop and Agents always go through the Gateway: `/api/apps/{id}/ui/*`, `/api/apps/{id}/api/*`, and `{mcp.gateway_path}`. `/api/pm\|doc/*` remain byte-compatible aliases — zero changes in Desktop and agent manifests |
| 6 | MCP | Still HTTP (stateless Streamable HTTP subset, single `POST /mcp`). The two hardcoded pm/doc pushes in `build_available_mcps()` become a registry walk: disabling an app removes it from the catalog on the next hot push; a new app with `auto_inject` needs no code change. memory keeps its ADR-101 branch |
| 7 | Front end | The app serves `/ui/`; Desktop contributes only an iframe container and a narrow `postMessage` bridge (theme / locale / accent / ready / notify). The sandbox deliberately omits `allow-same-origin` |
| 8 | Auth | The authenticated main window mints a short-lived HMAC **app session ticket**, passed through the iframe URL **fragment** (never query string, logs or Referer). The app page sends it in `X-App-Session`; the Gateway verifies per request and injects trusted identity. Optional one-time exchange for an `HttpOnly; SameSite=Strict; Path=/api/apps/{id}/` cookie |
| 9 | Identity | The proxy strips client-asserted `X-Actor` / `x-actor-admin` / `x-app-id` / `Authorization` / cookie and injects `X-App-Id` / `X-App-User` / `X-Actor` / `x-actor-admin=false` / `X-App-Platform-Url`, preserving ADR-064 Phase 3 anti-spoofing |
| 10 | Platform API | Minimal `/api/platform/{identity,context,agents,notify}`, **default-deny** against `permissions.platform` in `app.toml`. Called by the app backend with the supervisor-injected internal service token + `X-App-Id`; the webview never holds a platform credential |
| 11 | Failure isolation | No process / restarting → `503 + Retry-After: 2`; an app crash never affects Gateway or Runtime; enable changes trigger an `acowork/global/mcps` hot push |
| 12 | Rollback | Disable the app (platform admission) or remove its declaration; `/api/pm\|doc/*` aliases keep existing consumers intact |

### 1.3 Invariants (red lines, proposed for `AGENTS.md`)

1. **Zero app business logic in the Gateway**: no app data-directory reads, no interpretation of tool semantics, no hosted app REST implementation.
2. **Zero build-time coupling in Desktop**: no importing app front-end code; pages load at runtime only through the Gateway-proxied iframe.
3. **Apps never touch platform internals**: no direct Runtime HTTP, MQTT broker `:19875`, Vault, or another app's data directory; the only egress is `/api/platform/*`.
4. **Identity is injected, never self-asserted**; the webview holds no user Bearer token; tokens never enter localStorage or URL query strings.
5. **Apps do not depend on each other** (no direct calls, no shared data directories); collaboration flows through the platform catalog / events only.
6. **No host API that mutates platform state** — the core guard-rail of the "application" positioning.
7. memory's embedded right-panel UI is out of scope; it must not be declared as an `app.toml` app until that UI contract exists.

---

## 2. Background

### 2.1 The framework existed as facts, not as a framework

Before this change PM / DOC / user / memory were standalone processes with four near-identical supervisor files (sharing `acowork-core::supervisor`). But "which services exist" was hardcoded in five places: `gateway.toml` sections, three `if let` blocks in `build_available_mcps()`, three `http/*_proxy.rs` modules, Desktop's static `NavBar`, and `ExtensionsView.tsx`'s permanently empty `EXTENSIONS = []` (whose comment already names a planned `extensions.rs`). Adding one more capability meant editing five Rust/TS locations.

### 2.2 Positioning correction

The user clarified: "it is more like an application than a plug-in". A VSCode extension's primary surface is a host API (`vscode.*`) that mutates editor state and injects contributions into host UI. ACowork's units are complete applications with their own UI, data and business logic, merely hosted by the platform. The platform interface therefore has to be narrow — context reads plus a few explicit capabilities — rather than a host SDK.

---

## 3. Design

### 3.1 Five contract surfaces

| # | Channel | Direction | Auth |
|---|---|---|---|
| ① | `/api/apps/{id}/ui/*` | Desktop iframe → app bundle | public static bytes |
| ② | `{mcp.gateway_path}` (`/api/pm/mcp`, `/api/doc/mcp`) | Agent Runtime → app MCP | node token / `X-MCP-Actor` validation (ADR-064) |
| ③ | `/api/apps/{id}/api/*` | Desktop / iframe → app REST | app session ticket |
| ④ | `/api/platform/*` | app backend → platform | internal service token + `X-App-Id` + declared capability |
| ⑤ | supervisor ↔ `/health` | platform → app process | spawn / port-file / reverse watchdog (ADR-018) |

MCP is deliberately **not** under the `/api/apps/{id}/` wildcard: Agents keep the proven `{mcp.gateway_path}`, so machine-credential and ticket paths never mix.

### 3.2 Implementation map

| Layer | File |
|---|---|
| Declaration + validation | `core/acowork-core/src/app.rs` (`AppManifest`, `AppRegistry`, `app_shell_html`) |
| Runtime projection | `core/acowork-gateway/src/app/registry.rs` |
| Generic supervisor | `core/acowork-gateway/src/app/supervisor.rs` |
| Proxy + identity injection | `core/acowork-gateway/src/app/proxy.rs` |
| Discovery + control plane | `core/acowork-gateway/src/app/api.rs` |
| Platform API | `core/acowork-gateway/src/app/platform.rs` |
| Session ticket | `core/acowork-gateway/src/app/ticket.rs` (separate HMAC domain from ADR-112) |
| App-owned pages | `core/acowork-pm/src/ui.rs`, `core/acowork-doc/src/ui.rs` |
| Desktop | `src/lib/apps-api.ts`, `src/components/apps/AppFrame.tsx`, `src/views/ExtensionsView.tsx` |

### 3.3 Security

- Ticket = `v1.<expires>.<user_hex>.<mac_hex>`; MAC covers `(expires, app_id, user_id)` with length prefixes; a ticket cannot be replayed against another app; the in-memory key rotates on every Gateway restart.
- `auth_middleware::is_public_path()` exempts only `/api/apps/{id}/{ui|api|session}/...`. `/api/apps`, `/api/apps/{id}/enable|disable|session-ticket` stay behind the bearer gate. Every exempt channel re-verifies inside `app::proxy`.
- The cookie variant scopes `Path=/api/apps/{id}/`; the header variant (`X-App-Session`) is preferred and avoids SameSite restrictions in cross-site iframes.

---

## 4. Borrowed vs deliberately not borrowed (VSCode)

**Borrowed**: declarative manifest; lazy activation hints (`onStartup` / `onNavigate` / `onMcpCall`); process isolation (the extension-host idea); declared, default-deny permissions; signing and versioning (later reuse of `acowork-sign`'s APK v2 Ed25519 block).

**Deliberately not borrowed**: `contributes.menus / commands / views` into host UI; `vscode.*` host-state mutation; in-process `activate()` hooks; shared DOM/window; tight extension-market ↔ host-version coupling. An app contributes exactly one navigation slot, one MCP server and its declared platform capabilities.

---

## 5. Delivery and verification

- Shipped: declaration layer, generic supervisor, generic proxy, apps API, platform API, tickets, registry-driven catalog, PM/DOC `/ui/` + `/app-context`, Desktop workbench and iframe bridge.
- PM / DOC are the first registered apps; `/api/pm|doc/*` aliases are unchanged; `acowork/global/mcps` is registry-driven. memory's supervisor / proxy / catalog branches are untouched.
- Verification: `cargo check` over the four crates; unit tests `acowork-core::app` (4), `acowork-gateway::app` (11, including 5 `app::framework_tests` router integration tests against the real `build_router()`), `acowork-pm::ui` (1), `acowork-doc::ui` (1); Desktop view-structure tests (42), plus 10 new framework client / iframe-host contract tests (`apps-api.test.ts` 6, `AppFrame.test.tsx` 4). The isolated-instance live smoke commands and their expected results are listed in design doc 34 §6 and are a post-delivery verification item, not part of this batch's unit-test results.

---

## 6. Deferred

- **P2 front-end migration**: PM / DOC primary views are still built-in React components (`src/views/pm|doc`). The webview channel exists, but migrating full business UI is the riskiest step — pilot with DOC first. The coexistence is an intentional transition.
- **P3 packaging and installation** (contract defined in design doc 34 §6; no code yet): the `.app` package format (ZIP + Signing Block, carrying `bin/` and `ui/`), a `[package]` section (`app_api` / `platforms` / `entry`), signing and certificate-fingerprint pinning via the generic `acowork-sign` layer, `POST /api/apps/install` plus uninstall/rollback, separation of `apps/{id}/versions/{v}` (package content) from `app-data/{id}` (app-private data), three-tier binary resolution replacing the single `sibling_binary` source, and three runtime integrations (`AppRegistry::reload()`, a dynamic supervisor, catalog hot push). Delivery order is P3.1 → P3.3, where P3.1 is independently verifiable (package PM under a different id, e.g. `pm-local`, alongside the built-in `pm`).
- **Other P3 items**: SSE event channel, Vault secrets, app marketplace, dynamic process start/stop (enable/disable is currently platform admission; process lifecycle is decided at Gateway boot).
- **memory**: embedded right-panel apps need a separate UI contribution contract before being folded in.
- `ExtensionsView`'s i18n namespace is still missing (pre-existing TODO); no locale files were changed.
- Proposed CI red lines in `dev/ci.sh::run_all_redlines`: Gateway must not read app data directories; Desktop must not import app front-end code.
- **Dual canvas** (this ADR defined the hosting shell, not which copy of state an agent mutates): [ADR-118](./ADR-118-hosted-app-dual-canvas.md) / [design doc 35](../../design/en/35-hosted-app-dual-canvas.md). Left nav = user canvas; MCP does not attach by default; chat preview is a durable-pane iframe.
