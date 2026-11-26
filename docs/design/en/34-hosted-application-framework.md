# Design Doc 34: Hosted Application Framework

**Version**: 1.0 (2026-11-15)
**Status**: Implemented (PM / DOC validated as the first cohort)
**Authoritative decision**: [ADR-116](../../adr/en/ADR-116-hosted-application-framework.md)

> The Chinese document is the source of truth. This English counterpart is kept in the same commit by convention.

---

## 1. Positioning

An **application** = standalone process + standalone loopback HTTP service + its own front-end bundle + its own data directory + an MCP HTTP server. The **platform** = registration, supervision, routing, identity injection, MCP catalog publication, a navigation slot, and a narrow set of platform APIs. The platform does **not** inject host UI and does **not** expose a host API that mutates platform state.

First cohort: `pm` (18082), `doc` (18081). `memory` is **not** part of this framework — its UI is an embedded right-hand navigation panel, a different contribution contract.

---

## 2. Architecture and data flow

```
Desktop (Tauri)                        Gateway (:19876)                 App process (127.0.0.1:{port})
┌────────────────────┐                ┌──────────────────────┐        ┌──────────────────────────┐
│ ExtensionsView     │  GET /api/apps │ app::api             │        │ /ui/*  app-owned bundle │
│  ├ list / detail   │◄──────────────►│  app::registry       │        │ /api/* app-owned REST   │
│  └ AppFrame iframe │  ticket        │  app::proxy          │───────►│ /mcp   MCP HTTP         │
│    (no same-origin)│◄──────────────►│  app::platform       │        │ /health                 │
└────────────────────┘                │  app::ticket         │        └────────────────────────┘
                                       │  app::supervisor ───┼── spawn / probe / restart
Agent Runtime ── {mcp.gateway_path} ──►│  *_proxy (aliases)   │
                                       │  build_available_mcps│──► MQTT acowork/global/mcps
                                       └──────────────────────┘
```

`/api/pm/*` and `/api/doc/*` keep being handled by the existing `pm_proxy` / `doc_proxy`: the byte-level contract is unchanged. The new framework routes live under `/api/apps/{id}/*`.

---

## 3. `app.toml` declaration

| Section | Fields | Notes |
|---|---|---|
| top level | `id` / `version` / `name` / `description` / `icon` / `builtin` | `id` must match `[a-z][a-z0-9_-]{0,63}`; used as a URL segment, directory name and cookie path |
| `[process]` | `binary` / `port_hint` / `runtime` / `legacy_slot` / `port_file` / `health_path` / `extra_args` / `env` | `binary` allows an executable file name only (no `/`, no `..`); `runtime = legacy \| generic`; template placeholders `{gateway_url}` / `{data_dir}` / `{internal_service_token}` / `{publisher_token}` / `{app_id}` |
| `[mcp]` | `path` / `protocol` / `auto_inject` / `gateway_path` | `gateway_path` is the Gateway-side URL used by Bots, e.g. `/api/pm/mcp` |
| `[ui]` | `path` / `mode` / `[ui.nav]` | `mode = webview \| native`; nav provides one slot with `label` / `icon` / `order` |
| `[activation]` | `events` | `onStartup` / `onNavigate` / `onMcpCall` (currently hints only; built-ins start with the Gateway) |
| `[permissions]` | `platform[]` | Default deny; allowed values: `identity.read` / `context.read` / `agents.read` / `notify` |

Built-in declarations are compiled into `core/acowork-core/src/app.rs` (`PM_APP_TOML` / `DOC_APP_TOML`). User-installed declarations are layered from `{data_dir}/apps/*.toml`; invalid declarations are skipped with a warning. `AppRegistry::load()` is called from `GatewayState::new()`.

---

## 4. HTTP contract

### 4.1 Control plane (bearer protected)

| Method | Path | Description |
|---|---|---|
| GET | `/api/apps` | `{ "apps": [AppDto...] }` with enabled / status / port / mcp_url / ui_path / permissions |
| GET | `/api/apps/{id}` | A single AppDto |
| POST | `/api/apps/{id}/enable` \| `/disable` | Platform admission switch; writes `apps_disabled.json` and triggers an `acowork/global/mcps` hot push |
| POST | `/api/apps/{id}/session-ticket` | Mints an 8h ticket for the logged-in identity; returns `{ ticket, expires_at, ui_url }` |

### 4.2 App channels (ticket verified)

| Path | Upstream mapping | Auth |
|---|---|---|
| `GET /api/apps/{id}/ui/*` | `/{ui.path}/*` | Public static bytes |
| `*/api/apps/{id}/api/{tail}` | `/{tail}` | `X-App-Session: <ticket>` or a scoped cookie |
| `POST /api/apps/{id}/session` | — | One-time ticket → cookie exchange |
| `mcp` / `enable` / `disable` / `session-ticket` | — | Explicit 404 from the wildcard handler; each has its own exact route |

`auth_middleware::is_public_path()` exempts only the first segments `ui` / `api` / `session`; the control plane stays behind the bearer gate.

### 4.3 Platform API (App → Platform)

| Path | Capability | Response |
|---|---|---|
| GET `/api/platform/identity` | `identity.read` | `user_id` / `role` / `app_id` |
| GET `/api/platform/context` | `context.read` | instance, advertise_host, http/mqtt ports, boot time |
| GET `/api/platform/agents` | `agents.read` | id/name list of installed bots |
| POST `/api/platform/notify` | `notify` | `{ accepted: true }` (structured log only, no side effects) |

Caller = the app backend; credential = the supervisor-injected internal service token + `X-App-Id`. The Gateway default-denies against the registry's declared permissions (403 `capability_not_declared`). The webview never calls these endpoints directly.

### 4.4 Identity injection

Stripped before forwarding: `x-actor` / `x-actor-admin` / `x-mcp-actor` / `x-app-id` / `x-app-user` / `x-app-session` / `x-app-platform-url` / `authorization` / `cookie` / `x-acowork-node-token`, plus all hop-by-hop headers.

Injected: `X-App-Id`, `X-App-User`, `X-Actor` (= the user from the ticket), `x-actor-admin: false`, `X-App-Session-Verified: true`, `X-App-Platform-Url` (the app uses this to call the platform API).

---

## 5. Session ticket

```
ticket := v1.<expires_at_unix>.<user_id_hex>.<mac_hex_16bytes>
mac    := HMAC-SHA256(secret, "acowork/app-session/v1|<exp>|<len>:<app_id>|<len>:<user_id>")
```

- The secret is random per process, never persisted or printed; it lives in a **different domain** from the ADR-112 preview ticket, so the two cannot cross-verify.
- The MAC covers `app_id`, so replaying a ticket against another app fails. MAC verification runs before expiry checks so no expiry information leaks.
- Transport: Desktop → iframe URL `#ticket=...` (fragment: absent from query strings, logs and Referer); iframe JS → `X-App-Session` header.
- Optional cookie: `HttpOnly; SameSite=Strict; Path=/api/apps/{id}/; Max-Age=...`. The header path takes precedence and avoids SameSite restrictions on cross-site iframes.
- A Gateway restart invalidates all tickets; Desktop mints fresh ones.

---

## 6. Packaging and Installation

> This chapter defines where applications come from and how they get in. Sections 1–5 only cover how an already-present application is hosted; they say nothing about distribution. What follows is a contract written before the code exists — implementation order is in §6.13.

### 6.1 Scope and ownership

| Dimension | Decision | Rationale |
|---|---|---|
| Install target | **The Gateway host** (under `{data_dir}`) | The supervisor runs inside the Gateway process (`gateway/mod.rs`); a remote node hosts Runtimes today, not application processes or application data |
| Distribution form | A single `.app` file (ZIP + Signing Block) | Reuses the `.agent` signing system; the cryptography is not rewritten |
| Trust default | **Reject unsigned packages** | Installing an app means allowing a local arbitrary process to run indefinitely |
| memory | Still excluded | Its panel-style UI contract is undefined (§1) |

Remote distribution (pushing an app onto a node) is **not** covered: it would require a node-side app supervisor plus new data-directory and admission semantics. That is a separate ADR, not a footnote here.

### 6.2 The `.app` package format

A `.app` is a ZIP carrying everything the application needs to run. The key difference from `.agent` is that it **contains an executable**:

```
<app_id>-<version>.app
├── app.toml                # required: §3 fields + the [package] section of §6.3
├── bin/                    # required: platform binary
│   ├── acowork-pm          #   file name = [process].binary
│   └── acowork-pm.exe      #   Windows build; one package holds one platform
├── ui/                     # optional: the app's front-end bundle ([ui].path points here)
│   ├── index.html
│   └── assets/
├── assets/                 # optional: icons and static assets
│   └── icon.png
└── LICENSE                 # optional
```

| Item | Rule | Checked at |
|---|---|---|
| Entry paths | No `..`, no absolute paths, no symlinks, no encrypted entries, no nested ZIP | Every entry, before extraction |
| `bin/` | Must exist and `bin/{process.binary}` must be executable | Install |
| Platform | `[package].platforms` must contain the current `(os, arch)` | Install |
| Size | ≤ **200 MB** per package (`.agent` is 50 MB; an app ships a binary and a front end) | While streaming the upload — counted as it arrives, never buffered whole |
| Compression | `deflate` / `store` only | Extraction |
| Unknown top-level entries | Ignored (forward compatibility), never part of any contract | Extraction |

**No multi-platform fat packages.** One package, one platform, declared explicitly in `[package].platforms` — never guessed at runtime.

### 6.3 The new `[package]` section

```toml
[package]
# Host contract version: the Gateway only accepts an app_api it supports
app_api = 1
# Target platforms, compared against the current (os, arch) at install time
platforms = ["windows-x86_64"]
# Entry point inside the package (relative to the package root); defaults to bin/{process.binary}
entry = "bin/acowork-pm"
```

`app_api` is the **host contract version**, not the application version. An app may move its own semver freely; as long as it only uses the five contract surfaces of §4 it stays at `app_api = 1`. A Gateway that sees a higher `app_api` rejects the package outright instead of installing something it cannot drive.

### 6.4 On-disk layout: packages and data must be separate

This is a **breaking correction** to the current implementation. `app::supervisor::config_from_manifest` computes the app data directory as `{data_dir}/apps/{id}` today, while packages would also land under `apps/` — the two collide, and upgrades cannot tell "package content" from "user data".

```
{data_dir}/
├── apps/                          # platform install ledger + package content (read-only to the app)
│   ├── .staging/{uuid}/           # install scratch; deleted on failure
│   ├── .trash/{id}.{ts}/          # uninstall, kept 7 days
│   └── {id}/
│       ├── state.json             # current / fingerprint / origin / installed_at
│       └── versions/
│           ├── 1.0.0/             # package content, extracted verbatim
│           └── 1.0.1/
└── app-data/
    └── {id}/                      # app-private data; upgrade / rollback / reinstall leave it alone
```

- `apps/{id}/state.json` is the **platform ledger**; the app may neither read nor write it. `versions/*/` is package content and is semantically read-only.
- `app-data/{id}/` is created by the Gateway and passed to the process as `ACOWORK_APP_DATA_DIR`; the Gateway **never reads its contents** (ADR-009 red line).
- Constants: `APPS_DIR_NAME = "apps"` stays, `APP_DATA_DIR_NAME = "app-data"` is added, and `config_from_manifest` switches its `data_dir` to `{data_dir}/app-data/{id}`.

`AppRegistry::load()` must understand both layouts:

| Layout | Source | Precedence |
|---|---|---|
| `apps/{id}/versions/{current}/app.toml` (with `state.json`) | Installed | Higher |
| `apps/*.toml` | Manual / local development (today's behaviour) | Lower; on id conflict the directory package wins and a `warn` is logged |

Keeping the flat TOML is deliberate: built-in apps and local development can still "drop a TOML pointing at an existing binary" without packaging anything.

### 6.5 Binary resolution

`gateway/mod.rs` currently calls `lifecycle::process::sibling_binary(&bin_name)`, which requires the binary to sit next to the Gateway executable. That holds for built-in apps (they ship with the build) and fails outright for **installed** apps — this is the blocking prerequisite for installation.

Resolution becomes three tiers, first hit wins:

1. `{data_dir}/apps/{id}/versions/{current}/{package.entry}`
2. `{data_dir}/apps/{id}/{process.binary}` (flat-TOML local development)
3. `sibling_binary(&bin_name)` (built-in / shipped-with-the-Gateway legacy behaviour)

A hit that resolves outside the allowed directory, or is not executable, means the app **refuses to start** (`503`, absolute path in the log) and does **not** fall through to the next tier. Falling through would make "the wrong version got installed but an old one is running" impossible to diagnose.

### 6.6 Signing and trust

The generic layer of `acowork-sign` is reused; no cryptography is rewritten:

| Capability | How it is reused |
|---|---|
| Signing Block (Local Files + Central Directory + EoCD all covered) | `signing_block::{SigningBlock, Signer, SectionDigest}` as-is |
| ZIP-level insertion / lookup | `zip_utils::{find_eocd, insert_block_before_cd, find_binary_signing_block}` as-is |
| Sign / verify | `sign::sign_package` / `verify::verify_package` as-is (independent of package semantics) |
| Packaging | **Not** `packager::build_agent_package` (it hardcodes `manifest.toml`, `AgentManifest`, and the conversations/memory exclusions). Add `build_app_package`, reusing the three layers above |

Signing identities stay the two kinds used by `.agent` (Developer self-signed / Platform), plus one app-specific rule:

| Check | Rule | Error code |
|---|---|---|
| Integrity | SHA-256 section digests match | `package_tampered` |
| Provenance | Fingerprint recorded at first install; upgrades must match | `signer_mismatch` |
| Platform app | `[package].platform_app = true` requires a Platform signature | `platform_signature_required` |
| Unsigned | Rejected by default; accepted with a `warn` only when `apps.allow_unsigned = true` | `unsigned_package` |
| Built-in apps | No package, no signature; shipped with the Gateway build | — |

> Unlike `.agent` Phase 1 ("unsigned is allowed"), `.app` **rejects unsigned packages by default**: installing an app means allowing a local process to run for the lifetime of the machine, a much larger surface than a bot package. `allow_unsigned` exists for development and should be off in production.

### 6.7 Install flow

Hard requirement: no failure may leave a half-installed state, and no failure may disturb other running apps.

```
1  receive   → {data_dir}/apps/.staging/{uuid}.app (streamed, size counted as it arrives, aborted on overflow)
2  structure → entry allowlist; no .., no absolute path, no symlink, no encrypted/nested entries
3  verify    → verify_package() → certificate fingerprint + identity kind
4  extract   → .staging/{uuid}/ (paths and sizes re-checked per entry)
5  declare   → app.toml::from_toml() + validate_manifest() + the three [package] checks (app_api / platforms / entry)
6  assert    → bin exists and is executable; when ui/ exists, [ui].path must resolve
7  conflict  → same id already installed? (builtin refuses overwrite) → matching fingerprint means upgrade, otherwise signer_mismatch
8  place     → .staging/{uuid}/ → apps/{id}/versions/{version}/ (an existing version is refused, never silently overwritten)
9  ledger    → apps/{id}/state.json (atomic write)
10 reload    → AppRegistry::reload()
11 spawn     → start/restart the supervisor when enabled; on failure keep the install, mark status=error
12 publish   → trigger the acowork/global/mcps hot push
```

- The step-8 `rename` is same-filesystem with `apps/{id}` (both under `{data_dir}`), so it holds.
- A step-11 failure does **not** roll back the install: the package is fine, the process is a runtime problem. Rolling back would discard what the user just installed and hide the real error. The app is marked `error` and the UI shows the log path.
- Any failure in steps 1–7 must clean up `.staging/{uuid}`. Content already under `versions/` is never removed — that is a different, successfully installed version.

### 6.8 HTTP contract

All on the control plane (bearer protected), alongside §4.1:

| Method | Path | Semantics |
|---|---|---|
| POST | `/api/apps/install` | `Content-Type: application/octet-stream`, body = `.app`; returns `AppDto` + `installed_version` |
| POST | `/api/apps/install-url` | `{ "url": "...", "sha256": "..." }`; `sha256` is **required** (a URL is not trusted) |
| GET | `/api/apps/{id}/versions` | `{ "current": "1.0.1", "versions": [{ version, installed_at, fingerprint }] }` |
| POST | `/api/apps/{id}/rollback` | `{ "version": "1.0.0" }`; switch current + restart + hot push |
| DELETE | `/api/apps/{id}` | `?purge_data=false` (default); builtin returns 403 |

The boundary against existing endpoints, to avoid semantic drift:

| Endpoint | Does | Explicitly does not |
|---|---|---|
| `POST /api/apps/{id}/enable` / `disable` | Platform **admission** (catalog / proxy / navigation) | Install or remove packages, touch files |
| `DELETE /api/apps/{id}` | **Uninstall** (move to trash + drop the ledger) | Delete `app-data/{id}` by default |
| `POST /api/apps/{id}/rollback` | Switch version | Re-download or re-verify |

Error codes are **stable strings** so the Desktop can map them to user-facing copy instead of forwarding backend English:

| code | HTTP | Meaning |
|---|---|---|
| `package_too_large` | 413 | Over 200 MB |
| `package_malformed` | 400 | Bad ZIP structure / path traversal / encrypted entry |
| `package_tampered` | 400 | Digest mismatch |
| `unsigned_package` | 400 | Unsigned and `allow_unsigned` is off |
| `signer_mismatch` | 409 | Upgrade fingerprint differs from the installed one |
| `app_api_unsupported` | 400 | `app_api` newer than the host supports |
| `platform_unsupported` | 400 | `platforms` excludes the current platform |
| `app_id_reserved` | 409 | Attempt to overwrite a builtin |
| `version_exists` | 409 | Same version installed twice |
| `binary_missing` | 400 | `bin/` missing or not executable |
| `app_not_installed` | 404 | Uninstall: install directory is missing |
| `invalid_app_id` | 400 | Uninstall/operate on an id that is not a valid app id |
| `uninstall_failed` | 500 | Could not move the install directory to `.trash` (Windows directory move / file in use) |

### 6.9 Upgrade / rollback / uninstall semantics

| Operation | Package content | `app-data/{id}` | Process | Reversible |
|---|---|---|---|---|
| Upgrade (same id, new version) | Adds `versions/{new}`, `current` points at it | Untouched | Restarted | Yes |
| Rollback | `current` points back | Untouched | Restarted | Yes |
| Uninstall (default) | `apps/{id}` → `.trash/{id}.{ts}` | **Kept** | Stopped | Recoverable by hand within 7 days |
| Uninstall `purge_data=true` | Same | Deleted | Stopped | No (reinstallable, data gone) |

`.trash` is retained for 7 days and swept by a Gateway startup task (same policy as PM's `.trash`, but a **separate directory** — the two never share storage).

### 6.10 Runtime integration

Three things must change, or the app "installs but never works":

1. **`AppRegistry::reload()`**: `load()` currently runs once from `GatewayState::new()` and never re-reads the disk. Add `reload()` (read + replace the registry contents) and call it after install / uninstall / rollback.
2. **Dynamic supervisor**: generic app supervisors are started only at Gateway boot today. Add `start_app(id)` / `stop_app(id)` for post-install start, pre-uninstall stop, and post-rollback restart. Failure must not panic; it lands as `status=error`.
3. **Catalog hot push**: reuse the `mqtt_publisher_trigger.trigger()` already used by `enable/disable`, fired once after a successful install, so Bots see the app without restarting.

### 6.11 Desktop entry points

`ExtensionsView` needs:

- An "Install application" action → file picker for `.app` → `POST /api/apps/install` (streamed, with progress).
- Detail panel showing `installed_version` / signing fingerprint / install time / package size / origin (upload or url).
- A "Versions" block: version list, current marker, rollback.
- Uninstall with **inline confirmation** (no modal, per project UI rules) and a "also delete application data" checkbox.

Every failure maps §6.8's `code` to user-facing copy.

### 6.12 Packaging toolchain

```bash
# Produce an unsigned package from an app source directory (uses the acowork-sign generic layer)
dev/common/build-app.sh <app-src-dir> --version 1.0.1 --platform windows-x86_64

# Sign (existing binaries, same semantics as .agent)
acowork-sign --key ./keys/app-key.pem --cert ./keys/app-key.crt \
             --input ./build/pm-1.0.1.unsigned.app \
             --output ./build/pm-1.0.1.app

# Verify
acowork-verify ./build/pm-1.0.1.app
```

`build-app.sh` does exactly three things: check the binary architecture against `platforms`, pack `ui/`, and call `build_app_package` to emit an unsigned package. It does **not** sign — keys never enter CI plaintext or build scripts.

### 6.13 Phased delivery

| Phase | Content | Depends on |
|---|---|---|
| **P3.1** | Package format + `[package]` section + three-tier binary resolution + `app-data` separation + `reload()` | Nothing (smallest change, and it clears the install blocker) |
| **P3.2** | `build_app_package` + `build-app.sh` + local upload install + uninstall | P3.1 |
| **P3.3** | Mandatory signing + fingerprint pinning + `install-url` + version list / rollback + Desktop entry | P3.2 |

P3.1 is independently verifiable: package PM, install it under a different id (e.g. `pm-local`) into `apps/pm-local/` alongside the built-in `pm`; it must appear in `/api/apps`, start, and expose its MCP server to Bots. That step needs neither signing nor a resolver, yet it pins the two contracts most likely to be reworked later (binary resolution and data-directory separation).

### 6.14 Verification checklist

1. Install a `.app` → it appears in `GET /api/apps` with `status=running` and a non-empty `port`.
2. Install the same package again → `409 version_exists` (no silent overwrite).
3. Flip any byte inside the package → `400 package_tampered`.
4. Sign a new version of the same id with a different key → `409 signer_mismatch`.
5. Upgrade → `current` moves, `app-data/{id}` is byte-identical, the process restarts.
6. Rollback → `current` returns to the old version, data still untouched.
7. Uninstall → gone from the catalog, `apps/{id}` in `.trash`, `app-data/{id}` still present.
8. `platforms` excludes the current platform → `400 platform_unsupported`.
9. A package containing `../evil` → `400 package_malformed`, and `.staging` is left clean.
10. After installing, **without any restart**, the Bot side's `acowork/global/mcps` already contains that app's MCP server.

### 6.15 Explicitly out of scope

- Remote node distribution (§6.1).
- An app marketplace / online index / update checks.
- Delta upgrades (every install is a full package).
- Inter-app dependency resolution (no `depends_on`).
- Multi-platform fat packages.
- Install hooks inside the package (`preinstall` / `postinstall`) — **no package-supplied code runs at install time**, the same red line as `.agent`.

---

## 7. Verification

### 7.1 Build and tests

```bash
cd core
cargo check -p acowork-core -p acowork-gateway -p acowork-pm -p acowork-doc
cargo test -p acowork-core    app:: --lib
cargo test -p acowork-gateway app:: --lib     # includes the 5 framework_tests router integration tests
cargo test -p acowork-pm      ui::  --lib
cargo test -p acowork-doc     ui::  --lib
```

The router integration tests live in `core/acowork-gateway/src/app/framework_tests.rs` — they are **not** a separate integration test under `tests/`. They issue requests against the real `build_router()` and cover: app listing, UI/API channel identity injection, cross-app ticket replay, enable/disable admission, and platform capability default-deny.

Desktop side:

```bash
cd ../apps/acowork-desktop
npx tsc --noEmit
# Application framework client / iframe host contract (10 tests)
npx vitest run src/lib/apps-api.test.ts src/components/apps/AppFrame.test.tsx
# View-structure regressions (existing capsule suite)
npx vitest run src/components/common/capsule.test.tsx \
               src/components/common/listHeaderHeight.test.ts \
               src/components/common/sidebarRowFontSize.test.ts \
               src/components/common/overlayScrollCoverage.test.ts
```

### 7.2 Startup smoke (isolated instance; does not touch the running Gateway)

```bash
# Separate home and ports; never kill the existing gateway / runtime processes.
# There is no top-level --data-dir flag: an isolated instance uses --home
# (alias --work-dir) as the root of its config/ and data/ directories.
# A repo-relative path works on both Windows and WSL.
./target/debug/acowork-gateway --addr 127.0.0.1:29976 \
    --mqtt-addr 127.0.0.1:29975 --home ./target/app-smoke-home
```

After obtaining `$TOKEN` (ADR-114 accounts are always on: `POST /api/auth/setup`, then `POST /api/auth/login`):

```bash
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:29976/api/apps
curl -H "Authorization: Bearer $TOKEN" -X POST \
     http://127.0.0.1:29976/api/apps/pm/session-ticket      # → ticket
curl http://127.0.0.1:29976/api/apps/pm/ui/                   # → PM's own HTML
curl -H "X-App-Session: $TICKET" \
     http://127.0.0.1:29976/api/apps/pm/api/app-context      # → injected identity + platform context
curl -H "X-App-Session: $TICKET" \
     http://127.0.0.1:29976/api/apps/doc/api/app-context     # → 401 invalid app session (cross-app replay rejected)
curl -H "Authorization: Bearer $TOKEN" \
     http://127.0.0.1:29976/api/platform/context
curl -H "Authorization: Bearer $TOKEN" \
     http://127.0.0.1:29976/api/pm/projects                   # legacy alias unchanged
```

Expected: `/api/apps` returns two AppDtos (pm/doc); `ui` returns each app's own page; `app-context` shows the correct `app_id` / `actor` and a non-empty `platform` section; a cross-app ticket yields 401; after disabling an app, the next `acowork/global/mcps` push no longer contains that server.

---

## 8. Boundaries and known limitations

- **Packaging and installation are not implemented**: the contract is defined in §6 (package format, signing, install/uninstall/rollback, error codes, phased delivery) but no code exists. The only "user app" shape today is a hand-placed `apps/*.toml` whose binary already sits next to the Gateway executable — no package, no version, no uninstall. Delivery order is §6.13.
- **Front-end coexistence**: PM / DOC primary business UIs are still built-in Desktop React components; `/ui/` is an app-owned diagnostic page used to validate the full channel. Full migration is ADR-116 §6 P2.
- **Enable/disable semantics**: `enable` / `disable` control platform admission (catalog / navigation / reverse proxy). Process lifetime is decided at Gateway boot; dynamic spawn/kill and install integration fall under §6.10's P3 scope.
- **No SSE**: platform event subscription (`/api/platform/events`) is not implemented; apps can only poll `/api/platform/*` today.
- **legacy slot**: PM/DOC map to the existing `pm_process` / `doc_process` state fields via `legacy_slot`, avoiding changes to the existing identity-injection tests. This is an intentional migration bridge, not the long-term structure.
- **`navigation_guard.rs`**: the Desktop iframe targets a Gateway reverse-proxy path (`http(s)` + deep path); deployment must confirm the navigation firewall allows this prefix. The existing `UrlPreviewView` iframe exception uses the same mechanism.
- `ExtensionsView` still has no `extensions` i18n namespace (pre-existing TODO).
- **User canvas vs bot working state** (ADR-116 does not say which copy of state a bot mutates): see [ADR-121](../../adr/en/ADR-121-hosted-app-dual-canvas.md) and [design doc 35](./35-hosted-app-dual-canvas.md). Left nav is the user canvas; MCP does not attach by default; a `visual` app's chat preview is a durable-pane iframe, not a transcript row.
