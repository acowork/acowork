# ADR-114: Account System Decoupled from Network Topology (Accounts Are Always On)

**Status**: Implemented
**Date**: 2026-11-13
**Deciders**: 大鱼

**Supersedes / revises**:
- [ADR-076](./ADR-076-multi-user-account-system.md) § Decision 12 (`AUTH_MODE` = `local` / `multi_user` deployment-mode split)
- ADR-076 § Decision 12 v2/v3's "the first password never crosses the network; TTY / CLI / toml only" constraint
- The [ADR-084](./ADR-084-user-standalone-process.md) branches that started or injected the account service only in certain modes

**Predecessors**: ADR-055 (Node topology), ADR-076 (account model), ADR-084 (standalone user process / user_proxy)

**Followed by**: [ADR-115](./ADR-115-desktop-single-server-address-model.md) (collapses Desktop's "mode + address" two knobs into one server address; `local` / `remote` become an inference from that address)

---

## 1. Problem

The old design coupled two orthogonal things to one switch:

```
Gateway bind address / --auth-mode
        ↓
AUTH_MODE = local | multi_user
        ↓
whether accounts.json exists / whether /api/auth/* is registered / whether login is required
```

Consequently a **single machine** whose Gateway bind changed from `127.0.0.1` to `0.0.0.0` (LAN debugging, enabling relay, …) also flipped its login regime. In the other direction, an explicit `--auth-mode local` on a publicly reachable bind exposed an unauthenticated Gateway to the network.

Network topology (who can reach the listening socket) and the account system (whether accounts exist on this machine, how many, who can log in) have no causal relationship. Letting a network setting decide whether identity exists is the bug.

Two more concrete problems:

1. **First-boot state was undecidable.** A boolean `requires_setup` cannot distinguish "there is no account system at all" from "accounts exist but no administrator can log in", so the gate had to guess.
2. **First boot required SSH.** `ensure_bootstrap_admin()` seeded a passwordless `admin`, but no HTTP path existed to give it a password; the Desktop's `SetupRequiredView` was just a "go run this on the host" notice, leaving the onboarding flow broken.

## 2. Decision

### 2.1 The account system is always on; there is no mode

`AUTH_MODE` / `auth_mode` / `--auth-mode` are **deleted**. `acowork-user` constructs `AuthService` unconditionally, writes `auth/ed25519.key` unconditionally and registers `/api/auth/*` and `/api/users` unconditionally; the Gateway loads the verifier and proxies the user domain unconditionally.

What the user may still choose is the **number of accounts**: one administrator, or a few more. That is a runtime account-management action, not a deployment mode.

### 2.2 Three states replace the boolean

`acowork_core::account::AccountSystemState` (`acowork-core/src/account.rs`):

| State | Meaning | Gateway serves |
|---|---|---|
| `uninitialized` | no account store / empty store | setup surface |
| `needs_setup` | accounts exist but no login-capable admin (typically the seeded passwordless `admin`) | setup surface |
| `ready` | at least one admin with a real Argon2 hash and not disabled | everything |

`requires_setup()` = `state != Ready`. The predicate reads only `accounts.json`, never configuration and never the bind. `GET /api/status` publishes `account_state`; the user service's `/health` carries the same field; the Gateway supervisor polls it into `UserSnapshot`, and `restricted_mode_middleware` reads that snapshot in O(1) per request.

**"No snapshot" and "untrustworthy snapshot" are handled differently** (post-review correction):

- `user_snapshot == None` (no `/health` reply yet) fail-opens to `ready`: `user_process` is unpublished in that same window and `user_proxy` answers 503 for non-public paths, so claiming `setup_required` would send the Desktop into the first-boot wizard on a machine that is merely still booting.
- `/health` returned details but `account_state` is missing or unrecognised: `parse_account_state()` **fails closed** to `needs_setup` and logs a warning. The old fail-open rationale ("the auth gate still needs a token, and no token can exist before an admin does") is void under ADR-114: setup is a token-less path that creates the admin *and mints the first token pair*. Silently trusting a broken contract could hand the first admin to any reachable peer.
- `UserSnapshot::default()` is fail-closed too (it only exists for struct-update syntax in tests; production never builds a default snapshot).

One predicate: `AccountSystemState::from_accounts()` reuses `UserAccount::is_login_capable()` (`disabled_at` plus an Argon2id PHC shape check). The user service's second, ad-hoc `requires_setup()` free function was deleted so `/health` and the CLI `--check` cannot disagree about the same store.

### 2.3 Bind and peer are different questions (the key point)

> **The bind address answers "can this listener be reached from outside". The peer address answers "did this request originate on this machine". Only the latter may be used for a trust decision.**

- Whether the Gateway binds `127.0.0.1` or `0.0.0.0` changes neither the existence of the account system nor the first-boot state.
- `POST /api/auth/setup` accepts only requests whose **real peer is loopback**: `restricted_mode_middleware` reads `ConnectInfo<SocketAddr>` and calls `is_loopback_peer()`. This runs **unconditionally, first**, and must not be short-circuited by the `Ready` early-out — the snapshot is polled and can be stale or untrustworthy, whereas the peer cannot. A stale `ready` snapshot must never widen who may create the first administrator.
- Both TCP listeners are mounted with `into_make_service_with_connect_info`, so every production request carries `ConnectInfo`. Its absence **fails closed**: an in-process call has no provable network origin and may not bootstrap an admin.
- Besides `127.0.0.0/8` and `::1`, `is_loopback_peer()` recognises the **strict IPv4-mapped** form `::ffff:127.0.0.0/8`: a dual-stack listener reports a local IPv4 connection as an IPv6 socket address, and `IpAddr::is_loopback()` alone returns `false` for it — which would lock the Desktop on that very machine out of setup. The deprecated IPv4-compatible form `::a.b.c.d` and mapped routable addresses are never trusted.
- Forgeable headers such as `X-Forwarded-For` never participate.
- A local connection to a `0.0.0.0` listener still has a `127.0.0.1` peer, so the Desktop on that host can finish setup; a remote peer gets 403 `setup_local_only`.

### 2.4 First-boot setup joins the existing bootstrap flow

`POST /api/auth/setup` (public, no token) takes `{username, password, display_name?}` and returns a `TokenPair`:

1. `AuthService::setup_first_admin()` holds `setup_lock` across load→check→mutate→save.
2. It checks `AccountSystemState` first: `Ready` → 409 `already initialized`. **The service is its own authority**, so a second setup is refused even while the Gateway's health snapshot still lags by one poll.
3. If `ensure_bootstrap_admin()` already seeded a passwordless `admin`, that account is **activated in place** (rename / display name / Argon2 hash / clear invite and disabled markers) rather than creating a second admin or colliding on the username. Only a genuinely empty store goes through `create_account(Role::Admin)`.
4. Hashing runs on the blocking pool (Argon2id ~100ms). On success `sync_profiles()` rebuilds the derived view and `mint()` issues a token pair, so the Desktop needs no second login.
5. `PasswordPolicy` and `is_username_byte` validation is reused; a failure is 422 and writes nothing.

The CLI paths (`admin-setup`, the TTY prompt, a preconfigured `[bootstrap_admin]`) **all remain** as headless / scripted alternatives. A deployment with `bootstrap_admin` boots straight into `ready` and setup returns 409. The Desktop's `SetupRequiredView` changes from a notice page to a working form and keeps polling so a CLI-driven setup on the host is picked up automatically.

### 2.5 The relay tunnel: peer checks are not enough, deny by origin

`relay/remote_listener.rs` binds `127.0.0.1:{remote_http_port}` and is the sole consumer of the relay tunnel client — **a tunnelled remote request also has a loopback peer**. The peer check of §2.3 alone would therefore let it through to setup.

`remote_origin_policy` explicitly adds `/api/auth/setup` to its deny list (every method → 404, so the route's existence is not revealed). Here the trust anchor is the **listener identity**, not an address: the remote surface must never create the first administrator.

### 2.6 Middleware ordering

`restricted_mode_middleware` sits inside CORS and outside `auth_middleware` (axum runs layers bottom-to-top): a token-less first-boot request must get 403 `setup_required` (with `account_state`), not the 401 the auth gate would produce first. `SETUP_PATH` is on the `is_public_path` allowlist on both the Gateway and the user service.

Ordering *inside* the function is a security contract too: setup peer check → `Ready` early-out → `ALWAYS_PUBLIC` → setup pass-through → 403 for everything else. Putting the peer check after the `Ready` early-out lets a stale / fail-open snapshot decide who may create the first administrator; that was the P0 fixed in the post-implementation review.

## 3. Interface changes

| Old | New |
|---|---|
| `--auth-mode` / `auth_mode = "multi_user"` | deleted (no compatibility branch) |
| `/api/status.requires_setup: bool` | `/api/status.account_state: "uninitialized" \| "needs_setup" \| "ready"` |
| `/api/status.auth_mode` | deleted |
| Desktop `AuthMode = "local" \| "multi_user"` | `AccountSystemState` |
| `authStore.mode` / `AuthState."disabled"` | `authStore.accountState` / `"disabled"` removed |
| no HTTP path for the first password | `POST /api/auth/setup` → `TokenPair` |
| `SetupRequiredView` notice only | username / display name / password form + `completeSetup()` |
| Mobile `GatewayStatus.auth_mode` + `requires_setup` | `GatewayStatus.account_state` (derived via `needsSetup()`); the settings "mode" row becomes an account-system state row |

`AuthService` changed from `Option<Arc<AuthService>>` to `Arc<AuthService>`; every "the account system does not exist" branch was removed — `selfProfile.ts`, `authFetch.ts` and the PM/Doc `localMode` governance exemption on Desktop, plus the `auth_mode === "local"` pass-through in Mobile's `authStore`.

## 4. Data and migration

The project has not shipped, so **the production code contains no compatibility or migration branches**.

- The passwordless `admin` from `ensure_bootstrap_admin()` is not dirty data — it *is* `needs_setup`, activated in place by the setup endpoint.
- A legacy `local` install with `user_profiles.json` but no `accounts.json` is rebuilt from an empty account list and does not crash. Operators who want to fold that single-user profile and `user_id = "human"` / `None` session owners into the first administrator can run the one-off `scripts/migrate_legacy_local_data.py` (dry-run by default, takes a backup, part of no build).

## 5. Verification

- `cargo check --workspace --all-targets`: passes with zero Rust warnings (apart from the environment-level ONNX Runtime notice and the Windows incremental-compilation lock `os error 5` notice).
- `acowork-user` unit tests: `setup_creates_the_first_admin_and_is_one_shot`, `setup_activates_the_seeded_admin_in_place`, `setup_enforces_username_and_password_policy` and others pass.
- `acowork-gateway` unit tests: `restricted_mode::tests` (7 cases, including loopback allowed / remote refused under both `127.0.0.1` and `0.0.0.0` binds), `remote_listener::tests::policy_never_exposes_first_boot_setup` and `auth::mode::tests` (4 cases, including IPv4-mapped loopback positives and mapped-routable / IPv4-compatible negatives) pass.
- **Process-level e2e** (real Gateway + user service processes, serial `--test-threads=1`):
  - `account_state_e2e.rs` 4/4: fresh install → 403 `setup_required` → local POST setup → usable token → second setup 409; `[bootstrap_admin]` → ready at boot → setup 409; loopback-only bind still creates the store and signing key; `0.0.0.0` bind changes nothing about the account state.
  - `chat_sessions_e2e.rs` 14/14: the old "local mode refuses chat writes" case became `fresh_install_gates_chat_until_first_admin_setup` — the chat surface is 403 `setup_required` before setup, token-less writes are 401 afterwards (identity gate active), the session surface is reachable, and a second setup is 409.
  - `user_proxy_e2e.rs` 3/3: the old "login is 404 in local mode" case became `fresh_install_gates_the_proxy_until_setup_then_serves_users` — routes are always registered, first boot is gated with 403, login is no longer 404 after setup, and the token minted by setup reaches `/api/users` through the proxy.
  - `route_confirm_e2e.rs` 1/1; `route_confirm_live_e2e.rs` 1/1 (one flaky `timed out waiting for the create decision with a target session` during the first batch run; passes in 5s when run alone — a timing artefact of the live Runtime + scripted provider, unrelated to the account system).
- Desktop: `npx tsc --noEmit` has no errors introduced by this work (the remaining 9 are missing environment dependencies `docx-preview` / `jszip` / `xlsx` / `pdfjs-dist`); `npm run check:i18n` passes (1962 call sites); the 4 affected test files pass 57/57; of 3348 vitest cases 3343 pass, and all 5 failures come from the untouched `GroupChatPanel.blockRef.test.tsx` (it fails identically in isolation — a pre-existing issue).
- Mobile: `tsc --noEmit` passes; vitest 432 passed / 2 skipped (`auth-poll.test.ts` gained `uninitialized → restricted` and an unknown-state fail-open case).
- Repo-wide source scan: zero live `resolve_auth_mode` / `AuthMode::` / `is_local_mode` branches. The remaining `AUTH_MODE` strings appear only in ADR-114's retirement notes, anti-compat test inputs and assertions that the field no longer exists.

### 5.1 Post-implementation code review (this round)

Every finding was double-checked before changing anything. **Confirmed and fixed**:

| Severity | Finding | Fix |
|---|---|---|
| P0 | `restricted_mode_middleware` returned early on `Ready`, short-circuiting the setup peer check; combined with fail-open `/health` parsing, a remote peer could claim the first admin on a fresh install | Peer check moved to the top and made **unconditional**; missing `ConnectInfo` now fails closed; 3 regression tests |
| P0 | `parse_account_state()` fail-opened to `ready` on a missing/unknown value, with a rationale ("no token can exist before an admin does") that the setup endpoint invalidates | Fails closed to `needs_setup` with a warning; `UserSnapshot::default()` likewise; 2 tests |
| P1 | A private `requires_setup()` disagreed with `AccountSystemState::from_accounts()` on `disabled_at` and corrupt hashes | Second predicate deleted; `from_accounts()` reuses `UserAccount::is_login_capable()` (including an Argon2id PHC shape check); 2 tests |
| P1 | `/health` held an `RwLock` read guard across synchronous disk I/O (polled every 2s) | Copy small fields under the lock, read the store outside it |
| P1 | `sync_profiles()` silently returned when it could not reload `accounts.json` | Now logs `tracing::error!`; deliberately **not** escalated to HTTP 500 because the account was already written and a setup retry would 409 |
| P2 | Desktop duplicated the configurable password policy, mapped every 403 to "local only", and could leave `logged_in + account === null` after a timeout | Backend 422 message shown verbatim, 403 matched by error code, `logged_in` set only after `/me` succeeds (otherwise fall back to the login form) |
| P2 | Mobile failed **closed** on an unknown `account_state`, opposite to the Desktop | Both now recognise only `uninitialized` / `needs_setup`; new test |
| P2 | Migration script did not backfill an absent `owner`, and attributed legacy sessions to an ordinary account when no admin existed | Absent/null backfilled; no-admin case refuses to guess and asks for setup first |
| P2 | `dev/ci.sh` still required auth routes to sit behind an `is_multi_user()` branch; `SECURITY.md` and the runbook still documented `AUTH_MODE` as live | Redline now asserts unconditional registration; security model and runbook §0–§2 rewritten |
| P2 | Setup-wizard i18n keys existed only as `defaultValue`, so the Chinese UI showed English | 9 keys added to en / zh-CN |

**Checked and deliberately NOT changed** (with reasons, so they are not re-raised):

- `GatewayConfig` cannot take `deny_unknown_fields`: it has `#[serde(flatten)] timeouts` and serde does not support that combination. `config.rs::the_deployment_auth_mode_knob_is_gone` already pins "a stale `auth_mode` is just an unknown key with no effect".
- The setup deny sitting *after* `has_node_token` in `remote_origin_policy` is intentional: **any** request with a node token must get 403, and returning 404 instead would weaken the "internal machine identity never travels the remote channel" invariant (and diverge from `/api/debug/*`).
- `setup_first_admin()` reusing only the sentinel admin is intentional: it activates exactly the account `ensure_bootstrap_admin()` seeded. Creating a fresh admin when the only existing one is disabled-with-a-real-hash is a sound recovery, not a bug.

Post-review verification: `cargo check --workspace --all-targets` clean; `acowork-core` 14/14; gateway unit tests `restricted_mode` 10/10, `user_supervisor` 2/2, `auth::mode` 4/4, `remote_listener` 5/5; `acowork-user --lib` 239/239; process e2e `account_state_e2e` 4/4, `user_proxy_e2e` 3/3, `chat_sessions_e2e` 14/14; Desktop `authStore.test.ts` 18/18, `check:i18n` OK, `tsc` still shows only the 9 pre-existing missing-dependency errors; Mobile typecheck + `auth-poll.test.ts` 25/25; CI redline syntax and predicates verified; migration script dry-run / apply / idempotency / no-admin smoke checks pass.

### 5.2 Live first-boot review (2026-11-14): a fresh install could not boot

Deleting `.acowork` and letting Desktop spawn the Gateway exposed three stacked defects — any one of them alone stranded the first boot on the splash timeout view ("start gateway timed out → recovery view → Connect still fails", never reaching the login/setup screen):

| Sev | Defect | Fix |
|---|---|---|
| P0 | `cli.rs`'s first-boot interactive prompt had no daemon exemption: Desktop spawns the child from `npm run tauri dev`'s terminal, so stdin/stdout inherited a real TTY, `can_prompt_interactively()` said yes, and `rpassword::prompt_password` blocked forever on a password no human could see or type. The Gateway's log had a single line (the last INFO from `Gateway::new`), no port ever bound, one thread at zero CPU | daemons never prompt (`self.daemon` → `warn_first_boot_restricted()` and keep booting restricted); the Desktop spawn additionally sets `stdin(Stdio::null())` to sever TTY inheritance — belt and braces |
| P0 | `restricted_mode_middleware`'s `ALWAYS_PUBLIC` missed `/api/bootstrap`: the tokenless boot probe was answered with 403 `setup_required`, and `wait_for_bootstrap_ready` burned its whole 30 s budget — **even with the Gateway perfectly healthy**, the Desktop never reached the setup wizard | `/api/bootstrap` added to `ALWAYS_PUBLIC` (subsystems still come up in restricted mode — measured phase=READY within ~2 s; the information exposed is on par with `/api/status`, no new surface) |
| P1 | Desktop's `child_output_is_alive()` used `child.id() > 0`, which is always true: a hung or dead child was never respawned — every retry hit "already running, skipping spawn", leaving no self-healing path | now `try_wait()` (`Ok(None)` = genuinely alive); a stale handle is cleared and the probe→spawn chain re-runs |

Post-fix live verification: the rebuilt binary in daemon mode answers `/health` within ~2 s, listens on `0.0.0.0:19876/19875`, `/api/bootstrap` returns `phase=READY` (3/3 required), `/api/status` reports `account_state=needs_setup`, and `/api/auth/setup` is reachable; `restricted_mode` unit tests 10/10 (including the new bootstrap-public assertion); Desktop `cargo check` clean.

### 5.3 Live first-boot review, round two (2026-11-14): boot fixed but lands on login, not setup

After §5.2 the boot pipeline works (Gateway up, `/api/bootstrap` READY, UI reached), but a fresh install landed on the **login view**, and signing in printed the raw error `setup_required` — a machine with no accounts must never show a login form.

Root cause: a **timing race**. Bootstrap READY's required subsystems do not include the user-supervisor snapshot, so Desktop's `init()` probes `/api/status` immediately after READY and — if the snapshot has not arrived yet — the endpoint fail-opens to `ready` → no session → `logged_out`; the sign-in is then refused by the restricted gate with 403 `setup_required`, which Desktop merely displayed verbatim.

| Sev | Defect | Fix |
|---|---|---|
| P1 | `/api/status` fail-opened to `ready` while the user-service snapshot was pending (routes.rs). `uninitialized` is the honest word for "no snapshot yet"; a false positive self-heals within one setup-poll period, while a false `ready` dead-ended the first boot into the login view | fail-closed: snapshot `None` → `uninitialized`; test updated |
| P1 | Desktop `login()` / `firstLogin()` merely displayed the raw 403 code. This is the decisive fallback: whatever the probe race does, a sign-in attempt itself must route into the first-boot wizard | exact match on `status === 403 && error === "setup_required"` → set `needs_setup`/`setup_required` + start the completion poll instead of showing the code |

Desktop regression tests: 2 new cases (gate conversion + other 403s still surface), `authStore.test.ts` 20/20; gateway `http::routes` 14/14.

### 5.4 Code review, round three (2026-11-14): the always-on bearer gate introduced 34 unit-test regressions

Full review of the accumulated diff found that making `auth_middleware` always-on degraded the Gateway suite from the 5 pre-existing failures to 39 — **34 regressions from this change**, all in six proxy/preview test files (memory_proxy 10, pm_proxy 7, doc_proxy, user_proxy, workspace_preview, permission). Common root cause:

- These tests use the production `routes::build_router` (full middleware stack); with `user_verifier = None` the always-on gate answers 503 (the "service starting" window). The §5.1 fix installed a fixture verifier, which turned the anonymous requests into 401s.
- At baseline these tests relied on the "local mode" pass-through around the gate — a path ADR-114 removed.

**Fix pattern** (identical to the baseline "anonymous passes" semantics, assertions untouched): the test state sets `internal_service_token = Some("test-svc-token")` and requests carry `Bearer test-svc-token` — i.e. the gate's machine-actor branch (by design: internal services calling back over loopback carry no user identity).

Two cases needed individual judgement:

- `workspace_preview::the_mint_route_is_registered_under_the_path_the_desktop_calls`: same pattern; the stale comment "Local mode: the bearer gate is a no-op" now explains the request authenticates as the internal service (machine actor).
- `permission::ownership_is_always_enforced` (renamed from `local_mode_noop`, already rewritten in §5 to expect 404): a request with no AuthContext passes through at the permission layer — **not a security regression**. That pass-through is baseline code whose meaning is "machine actor (internal service token / node token), or a path the auth gate already answered; ownership is a user concept, per-route credential checks still apply". In production an anonymous request never reaches the permission layer (the always-on gate 401s first). Fix: the test mounts the full production stack (`auth_middleware` + `permission_middleware`) and authenticates as a non-owner user `bob` with a token from `test_support::access_token` — the "local mode is gone" pin now rides the real identity path, and the machine-actor pass-through stays documented as design.

After the fix the gateway suite reads **911 passed / 5 failed**, the five failures identical to baseline (embedding_api, lsp_admin_proxy, proxy×2, probe::runner — environment-dependent: they need a live Runtime / external services).

## 6. Consequences

**Gains**: changing the bind, enabling relay or otherwise changing the network topology no longer affects login; first-boot state is explicit and consumable by the UI; a new machine no longer needs SSH or a TTY to finish onboarding; the attack surface is narrowed to "real loopback peer AND not the relay origin".

**Costs / caveats**:
- Every request now consistently carries a bearer token when a session exists; `authFetch` no longer has a "local pass-through" short-circuit. With no token `acquire()` returns `null` and behaviour is unchanged.
- The Gateway's `account_state` snapshot is driven by a `/health` poll and can lag by up to one interval (2s). `completeSetup()` therefore waits for `/api/status` to report `ready` before calling `/api/auth/me`, avoiding a stale 403 gate. It only sets `logged_in` after `/me` succeeds; on timeout or repeated `/me` failure it drops the local session and falls back to the login form with an explanation, rather than leaving a half-initialised `logged_in / account === null` state (a second setup would 409, so the wizard is not a recovery path).
- The Desktop setup form no longer duplicates the server-side, configurable `PasswordPolicy`; it checks only non-empty + matching confirmation and shows the backend's 422 policy message verbatim. A 403 shows the "local only" hint only when the error code is exactly `setup_local_only`; other statuses show their real reason.
- `/api/auth/setup` is public on the user service too; its trust boundary is "the user service binds loopback only AND the Gateway already checked the real peer", consistent with the existing `/internal/*` surface.
- Mobile connects from a remote peer, so it **cannot** perform first-boot setup (403 `setup_local_only`). Its `restricted` screen only points at the host; setup still happens on the Gateway machine via the Desktop form or the CLI.
