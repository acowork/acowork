# ADR-076: Multi-User Account System

**Status**: Draft (the business semantics are valid; **the implementation shape has been superseded by [ADR-084](./ADR-084-user-standalone-process.md)**)
**Date**: 2026-10-15
**Deciders**: 大鱼

**Predecessors**:
- [ADR-009](./ADR-009-gateway-workspace-isolation.md) (§5.4 Gateway boundary rules — user chat data belongs to Gateway itself and is not within the scope of that prohibition, but it must be explicitly written down in this document)
- [ADR-024](./ADR-024-merge-metadata-into-index.md) (the conversation persistence paradigm — the meta.json + jsonl two-file layout — is the reuse template for user chats)
- [ADR-055](./ADR-055-remote-runtime-node-topology.md) (Node topology — Node owns `install_path` and session data physically lands on the Node; this ADR does not break that, but the session filter dimension expands from agent_id to `(instance_id, user_id)`)
- [ADR-073](./ADR-073-agent-instance-identity-decomposition.md) (the agent instance / node / user three-layer identity paradigm — this document promotes user to an identity dimension on the same level as instance and node)
- [ADR-059](./ADR-059-parallel-onboarding-handshake.md) (the Vault Argon2id + ChaCha20-Poly1305 KDF chain — this ADR reuses the same vault's master key to derive per-user encrypted entries rather than building a new password system)

> **Supersession note (2026-10-20)**: the **business semantics and data model of this ADR remain valid** (the account model, session isolation, Desktop UI, deployment-mode split, and other decisions are retained). But the **implementation shape that this ADR landed** — embedding accounts / user chat inside the Gateway process — has been **superseded** by [ADR-084](./ADR-084-user-standalone-process.md): the user domain (accounts, credentials, roles, profile / avatar, user↔user chat) has been moved out into a standalone process `acowork-user`, and Gateway retains only the reverse proxy + the authentication gate.
>
> What is affected is **code ownership**, item by item:
>
> - **§ Decision 8 (user-to-user chat, Gateway-side conversation.json / jsonl)** → moved into `acowork-user` (data and semantics unchanged).
> - **§ Decisions 1 / 2 (the account model and credential storage)** → data and semantics unchanged; the storage location moves into the `acowork-user` data directory.
> - **§ Decision 3 (HTTP authentication)** → **issuance** (login / refresh) moves into `acowork-user`; **per-request validation** stays in Gateway, and switches to local Ed25519 public-key signature verification (HS256 → EdDSA).
> - **§ Decisions 5 / 6 (the admin role / account lifecycle)** → moved into `acowork-user`.
> - **§ Decision 12 (the deployment-mode split)** → semantics unchanged; the `[multi_user]` configuration ownership moves into `acowork-user`.
> - **§6.4 (the core/acowork-gateway change list)** → ADR-084 §6 takes precedence (the user-domain code moves out and Gateway becomes a supervisor + user_proxy).
>
> **Unaffected and still valid**: § Decision 4 (session isolation), § Decision 7 (Desktop UI), § Decision 10 (PM / Doc proxy identity injection), § Decision 11 (the PM member model), and the identity delivery of [ADR-042](./ADR-042-mqtt-user-identity-delivery.md).
>
> The implementation plan is at [docs/plan/zh/user-dev-plan.md](../../plan/zh/user-dev-plan.md).

---

## 1. Decision Summary

### 1.1 In one sentence

**Promote "user" from a pure display preference to a first-class identity dimension**: `UserProfile` is upgraded to `UserAccount` (with account / password / role / admin flag), account credentials are encrypted at rest with the existing Vault's master key, session metadata gains a `user_id` field plus a `visibility` switch, the Runtime parses the caller's scope from the Gateway-injected `x-user-id` header and filters `GET /sessions` and validates per-session read/write **before pagination**; the session control plane (create / open / close / delete) moves from MQTT to HTTP, because MQTT control messages carry no identity; a new "system administrator" role bypasses all session isolation; the Agent list sidebar gains a "User List" collapsible group rendered side by side (reusing the `partitionAgentsByNode` grouping paradigm); and the Gateway side gains user-to-user chat persistence (the conversion.json / jsonl two-file layout, splitting in the style of ADR-024).

### 1.2 Key decision table (detailed rationale in §4)

| # | Decision | Conclusion |
|---|---|---|
| 1 | Account data model | Upgrade `UserProfile` → `UserAccount`, adding `password_hash` (Argon2id), `password_salt`, `role` (`user`/`admin`), `created_at`, `disabled_at?`; the display fields display_name / language / avatar etc. are retained |
| 2 | Credential storage | **Reuse the existing Vault's master key**; the account file is encrypted at rest as `vault://accounts/{user_id}.enc`; both account creation and password change require the Vault unlocked; **no second password system is introduced** |
| 3 | HTTP authentication | A single bearer token becomes **login tokens** (a short-lived access_token + a long-lived refresh_token), with the token payload carrying `user_id` + `role`; the middleware parses it and injects an `AuthContext` into `AppState`; an admin token is distinguished by an extra flag |
| 4 | Session isolation | `SessionMeta` gains `user_id: Option<String>` + `visibility: Option<SessionVisibility>` (`None` = public; the default does not change old-data behaviour); identity is delivered by the Gateway-injected `x-user-id` header (`*` = admin, unfiltered), the Runtime parses it into a `SessionScope` and filters **before pagination**; reads go through `is_readable_by` (unreadable → 404), writes go through `is_writable_by` (owner / admin only); **the session control plane moves from MQTT to HTTP** (see the implementation record in this section) |
| 5 | The admin role | A `role = "admin"` user bypasses `user_id` filtering, and `GET /api/users` sees all accounts (including password-hash metadata but not plaintext); an ordinary user can only `GET /api/users/{self}` |
| 6 | Desktop account switching | The top bar gains a "current user" menu whose dropdown contains switch account / change password / deregister / log out / register a new account (if allowed); account switching is equivalent to "clear the local cache + reconnect the Gateway + re-fetch the agent list + reconnect MQTT" |
| 7 | The Sidebar User collapsible group | Render `partitionAccountsByAccountType` collapsible items at the same level as AgentList — a single item "Users (N)" collapsed by default, clicking expands to list all accounts; in the admin view, clicking an account name enters that user's session-list filter mode |
| 8 | User-to-user chat | The Gateway side adds `data_dir/users/{user_a_id}/chats/{user_b_id}/conversation.json` + a same-named `.jsonl` (ordered lexicographically as `(min(a,b), max(a,b))` to avoid duplication); it references the ADR-024 meta/jsonl split; group chat is not supported |
| 9 | Storage ownership | User chat data belongs entirely to the machine hosting the Gateway (`data_dir/users/...`) and does **not** go through the Runtime HTTP reverse proxy; this is explicitly written into the ADR-009 §5.4 exception clause |
| 10 | Proxy identity injection (PM/Doc) | The REST proxy `X-Actor` changes from the hardcoded `"human"` to `AuthContext.effective_user_id` (the token identity from decision 3); the MCP path's `X-MCP-Actor` validation is unchanged (agent identity and user are orthogonal, see § Decision 10) |
| 11 | A multi-user PM member model | `ProjectMember` gains `kind` (`Agent`/`User`), making the human operator symmetric with agent members; the `assignee = "human"` special case is removed and the invariant tightens to `assignee ∈ ∅ ∪ members` (see § Decision 11) |
| 12 | Deployment-mode split | Adds `AUTH_MODE ∈ {local, multi_user}`, auto-inferred from the bind address (`127.0.0.1` → `local`; `0.0.0.0` → `multi_user`), and explicitly overridable; in local mode §1-§11 degrade to no-ops (no login page / Argon2id / admin / session filtering / user chat). **Handling of an empty account store in multi_user mode (v2 revision)**: instead of "refuse to start when missing", seed a passwordless account with `username=admin` (`password_hash=DISABLED_PASSWORD_HASH`) and have the Gateway enter **restricted mode** — only `/health` and `/api/status` are reachable, and every other `/api/*` returns 403 `{error: setup_required}`. The operator completes the first-time setup on the Gateway host (TTY prompt / `admin-setup` subcommand / the `[multi_user].bootstrap_admin` toml section), after which restricted mode is lifted. **No HTTP endpoint accepts a first-time password** — the password only travels via stdin / file / TTY / toml, **never over the network**. **(v3 revision: the daemon always starts HTTP first, and restricted mode genuinely serves externally; the prompt is only best-effort and a non-TTY never blocks startup — see § Decision 12 v3)**. See § Decision 12 v2 / v3 for details. |

### 1.3 Invariants (must be satisfied)

1. **Session isolation is the mandatory default in multi_user mode**: except for admin, all session-dimension reads and writes (list / messages / state / files) must go through user_id filtering; missing any single one is a data leak. Session filtering is not enabled in local mode (see § Decision 12). **(Current status: ✅ satisfied — the Runtime read-path filter + the write-path owner check + the HTTP-ization of the control plane have all landed, see the § Decision 4 Phase D implementation record.)**
2. **admin cannot forge a user_id**: in the admin view, "see sessions as user A" is implemented through the `?as_user=<user_id>` query, but `as_user` is never usable by an ordinary user; the `role` field in the token is fixed at issuance time and in-request override is not accepted.
3. **Account credential encryption does not depend on the Vault being unlocked**: the account read path degrades to 401 when the Vault is locked (cannot decrypt → cannot log in), but **account-list metadata (without passwords) is allowed to be displayed while the Vault is locked** (metadata only, such as username / role / created_at), so that account selection still works in a lock-screen scenario.
4. **Writing `session.user_id` is immutable**: once a session is created and bound to a user_id, it is **never modified** (except in migration / import scenarios, which must be an admin operation); this guarantees the immutability of a session history's "owner".
5. **Both sides of a chat are peers**: a message from user A → user B is stored under the `min(a,b)/chats/max(a,b)/` directory; both sides' GET / POST are symmetric, with no per-user state machine maintained inside the Gateway.
6. **Password changes require the old password**: the change-password API accepts `old_password + new_password`, preventing arbitrary password changes after a token leak; admin cannot change another person's password (must reset first and then go through the first-login password-change flow).

### 1.4 Deployment-mode behaviour comparison table (a quick reference for § Decision 12)

| Dimension | `AUTH_MODE=local` (bind `127.0.0.1`, the default) | `AUTH_MODE=multi_user` (bind `0.0.0.0`) |
|---|---|---|
| HTTP authentication | The existing bearer token (`data_dir/http_token`) | access_token (HS256, 15 min) + refresh_token (30 d) |
| Login flow | None (Desktop uses the token directly) | `/api/auth/login` + LoginView |
| `UserAccount` shape | `user_profiles.json` keeps using the existing `UserProfile` fields | `UserAccount` (including `password_hash` / `role` / `disabled_at`) |
| `accounts.json` (the authoritative account table) | Not created | Created (plaintext); `vault/accounts/*.enc` extension is optional |
| The first account | Keeps the current state (no account concept, `user_profiles.json` unchanged) | Seeds a passwordless `admin` + restricted mode (**v2 revision**, see § Decision 12 v2 / v3; it was originally "refuse to start when unconfigured") |
| The admin role | None (the OS user is admin) | `role = Admin`, `GET /api/users` returns everything |
| Session isolation | `SessionMeta.user_id` is written but **reads are not filtered** (no `x-user-id` header → `Unfiltered`) | The read path filters forcefully (except for admin); when `visibility = Private`, a non-owner sees it as non-existent (404) |
| The session control plane | The same HTTP route (the Runtime treats it as `Unfiltered`) | HTTP + token authentication; create records the owner |
| The session `visibility` default | `None` (public, unfiltered) | **An owned session = `Private` (written to disk at creation); an unowned session = `None` (public)** — see § Decision 4 "The two meanings of the default value" |
| `?as_user=` | The route is not registered | An admin-only read-only view |
| The `/api/auth/*` routes | **Not registered** | All registered |
| User-to-user chat | **Not registered**, `data_dir/users/` is not created | The full `/api/users/{self}/chats/*` |
| The PM/Doc proxy `X-Actor` | The constant `"human"` | `auth.effective_user_id` |
| The PM `assignee == "human"` special case | **Retained** (there is no token identity to inject) | Removed, `assignee ∈ ∅ ∪ members` |
| The Desktop top bar | "User preferences" (the current state is unchanged) | "Account menu" (switch / change password / deregister / log out) |
| The Desktop sidebar User group | Not displayed | The `partitionAccounts` collapsible group |
| The upgrade path | → multi_user: add `password_hash` + activate via `invite_token` (no data migration needed) | — |
| The downgrade path | — | → local: `--auth-mode local`; the account files are retained but cannot be used to log in |

> **Configuration channel (settled during implementation)**: this document uses `AUTH_MODE` as the **conceptual name** of the deployment mode and does not map it to any specific environment variable. There are actually three configuration channels (priority CLI > TOML > bind inference > default `local`):
> - CLI: `--auth-mode <local|multi_user>` (environment variable `ACOWORK_GATEWAY_AUTH_MODE`, see [cli.rs](../../../core/acowork-gateway/src/cli.rs))
> - TOML: the top-level `auth_mode = "local" | "multi_user"` (see [config.rs](../../../core/acowork-gateway/src/config.rs))
> - Inference: the HTTP bind address (loopback → `local`, anything else → `multi_user`)
>
> The inference only looks at the HTTP bind address, never at any `AUTH_MODE` string; everywhere this document writes `AUTH_MODE=local/multi_user` on its own it means the conceptual mode.

---

## 2. Background and Motivation

### 2.1 The current state: one user, one configuration, shared globally

```text
                        ┌────────────────────────┐
                        │       Desktop App      │
                        │   (a single localStorage)  │
                        └──────────┬─────────────┘
                                   │ HTTP + Bearer Token (a single shared one)
                                   ▼
                        ┌────────────────────────┐
                        │       Gateway          │
                        │  HttpAuth (1 token)    │
                        │  user_profiles.json    │  ← all users flattened, no authentication
                        └──────────┬─────────────┘
                                   │ MQTT
                                   ▼
                        ┌────────────────────────┐
                        │   Runtime (per inst)   │
                        │  conversations/meta/   │  ← no user_id field
                        │  conversations/*.jsonl │
                        └────────────────────────┘
```

**Key defects**:
- `UserProfile` is a "display preference", not an "account" — any frontend can `POST /api/users` to register a new user, and can also push its own profile to the Runtime's `last_user_profile` when `is_active=true`.
- The `HttpAuth` bearer token is a randomly generated 32-byte hex at Gateway startup, and all Desktop instances **share the same token**; once the `http_token` file leaks, every machine has equal power. In other words: what logs in is "this Desktop", not "this user".
- `SessionMeta` carries no user_id, and `GET /api/agents/{id}/sessions` returns **all** sessions; on Desktop, "sessions I created vs. sessions someone else created" cannot be distinguished.
- There is no "administrator" concept — all users have equal power, and nobody can see the whole picture.

### 2.2 Existing building blocks that can be reused

| Building block | Current state | Reuse point in this document |
|---|---|---|
| `Vault` (Argon2id + ChaCha20-Poly1305) | [core/acowork-vault/src/vault.rs](../../../core/acowork-vault/src/vault.rs) derives all `.enc` entries from one master key | The user account file reuses the same master key as `vault://accounts/{user_id}.enc` |
| `partitionAgentsByNode` (the Node collapse paradigm) | [apps/acowork-desktop/src/components/agent-list/partitionAgentsByNode.ts](../../../apps/acowork-desktop/src/components/agent-list/partitionAgentsByNode.ts) | The new `partitionAccounts` copies it entirely — a single collapse group vs. multiple node groups is an isomorphic problem |
| `SessionMeta` + jsonl (ADR-024) | meta.json at 400 bytes + jsonl stream append | User chat = `conversation.json` (meta) + `conversation.jsonl` (stream); the schema fields differ |
| `HttpAuth::validate_token` (constant-time comparison) | [core/acowork-gateway/src/http/auth.rs:60](../../../core/acowork-gateway/src/http/auth.rs#L60) | The token becomes a signed JWT-ish value, and the verification code is replaced by a signature check |
| `OperationAck` + `expected_version` (ADR-059 §7.3) | [core/acowork-gateway/src/http/users_api.rs:147](../../../core/acowork-user/src/http/profile_api.rs#L147) optimistic concurrency | Account creation / password change / role change all go through the same optimistic concurrency protocol |

### 2.3 Approaches already tried / rejected (to avoid repeating mistakes)

- **Building a second password system decoupled from the Vault**: ❌ the user is forced to remember two passwords; salt / iteration parameters are split in two; operations cost doubles.
- **Stuffing account info into `UserProfileListFile`** (the existing json): ❌ that file's path `data_dir/user_profiles.json` currently lands in plaintext; mixing account credentials in would break the "non-sensitive display metadata" semantics of ADR-059 §7.3.
- **Having the Runtime host its own user store**: ❌ the Runtime is an ownerless process, and user views of the same agent instance across Nodes cannot be merged; moreover ADR-009 §5.4 forbids the Gateway from directly reading Runtime private data, so the direction is reversed.
- **Implementing session filtering in the Gateway via an `instance_id` index file**: ❌ this introduces a second index source (coexisting with the Runtime's scan_sessions), maintaining two sources of truth; session persistence paths all live in the Runtime, so this is reinventing the wheel.

---

## 3. Goals

1. **Account layer**: the full register / log in / change password / deregister flow; Argon2id password hashing + Vault-encrypted storage; no second password system is introduced.
2. **Session isolation**: each session binds the creator's user_id at persistence time; an ordinary user's `GET /api/agents/{id}/sessions` by default only sees their own; admin sees everything.
3. **Administrator**: a built-in admin role; the first admin is created when the Gateway is installed, via an environment variable / first-start interaction; admin can see all data but cannot forge a user_id to operate ("view as user X" goes through the `?as_user=` query rather than identity impersonation).
4. **Desktop UI**: top-bar account switching / change password / deregister / register entry points; a User collapsible group beside the Agent list in the sidebar (in the admin view, clicking an account enters "see sessions from that user's perspective" mode).
5. **User-to-user chat**: text + images + documents; conversion.json (meta) + conversion.jsonl (stream) persisted in the Gateway's local `data_dir/users/{a}/chats/{b}/`; it does not go through the Runtime HTTP.
6. **Storage**: accounts + user chats all live on the machine hosting the Gateway (under `data_dir/`), independent of the Node agent filesystem; the session.user_id dimension does not change the Runtime's physical data layout (still landing under `{install_path}/workspace/conversations/`).

---

## 4. Decisions

### Decision 1: the account data model — `UserProfile` → `UserAccount` (an in-place upgrade, with the migration script converting within the same table)

```rust
// core/acowork-core/src/account.rs (new file)
pub struct UserAccount {
    // ── ADR-076: identity fields ──
    pub user_id: String,                  // UUID v4, reusing the existing user_id semantics
    pub username: String,                 // new: the unique handle used for login (lowercase + digits + -_)
    pub display_name: String,             // the old UserProfile.display_name
    pub role: Role,                       // User / Admin

    // ── ADR-076: credential fields ──
    /// Argon2id PHC string: "$argon2id$v=19$m=...,t=...,p=...$<salt>$<hash>"
    /// Stored separately — independent of the Vault — so that login verification can
    /// complete even while the Vault is locked
    /// (the Vault is for encrypting sensitive extension fields, not the password hash itself)
    pub password_hash: String,
    pub password_changed_at: String,      // ISO8601, for the forced password-change policy
    pub password_expires_at: Option<String>,

    // ── the old UserProfile fields moved over ──
    pub language: String,
    pub timezone: String,
    pub city: Option<String>,
    pub country: Option<String>,
    pub occupation: Option<String>,
    pub avatar: Option<String>,
    pub builtin_avatar: Option<String>,
    pub communication_style: Option<String>,
    pub custom: HashMap<String, String>,

    // ── lifecycle ──
    pub created_at: String,
    pub updated_at: String,
    pub last_login_at: Option<String>,
    pub disabled_at: Option<String>,      // soft delete: preserves historical session ownership
}

pub enum Role { User, Admin }
```

**The relationship with the existing `UserProfileListFile`** — three files, each with its own duty:

| File | Content | Encryption | Authority |
|---|---|---|---|
| `data_dir/accounts.json` (new, `AccountListFile`) | **The authoritative account table**: `user_id` / `username` / `role` / `password_hash` / lifecycle / display fields | Plaintext | **The source** (in multi_user mode) |
| `data_dir/user_profiles.json` (retained, `UserProfileListFile`) | The `UserProfile` public view (display fields, no username / role / password) | Plaintext | **Derived** (the source of the Runtime `last_user_profile` push) |
| `data_dir/vault/accounts/{user_id}.enc` (new, optional) | Sensitive extensions: `api_secrets` / `recovery_codes` / `encrypted_notes` | Vault master key | Extension |

**Key point**: `password_hash` lands in the **plaintext** `accounts.json` — it is a one-way Argon2id PHC string that contains no plaintext password, so it is safe to persist in plaintext; keeping it in plaintext is precisely so that login verification can complete while the Vault is locked (see below). `vault/accounts/*.enc` does **not** contain password_hash, only genuinely confidential extension fields. `user_profiles.json` is derived from `accounts.json` and serves the Runtime `last_user_profile` push (a redacted copy).

**The residual ceiling of the derived view** (`account_api::sync_profiles`): under multi_user every session carries its own owner (`x-user-id`), so the notion of a "global active user" is itself meaningless by then; but that legacy `last_user_profile` push topic still pushes on behalf of a **single** account, so every account change must still rebuild this derived view from `accounts.json` and pick one (`is_active`) by "the most recently logged-in account, falling back to the first enabled admin". It now **only feeds** that legacy topic and affects no authentication decision (authentication reads `accounts.json` / token claims). The upgrade path = Runtime learns to read the profile by the owner carried per request, at which point this derived view and the whole of `sync_profiles` disappear. Trigger condition: the Runtime side needs "per-account profile pushes" (no consumer needs it today).

**Why Argon2id does not go through the Vault**: the Vault is symmetric encryption (encrypt/decrypt require the same master key) and is used for "secrets that must be decryptable in the short term". A password hash is **one-way** (the plaintext cannot be derived from it), and it must be verifiable even while the Vault is locked (a typical scenario: the user's first login after boot, before the Vault is unlocked). The two have different cryptographic properties, and forcing the password hash into the Vault would make the login path depend on the Vault's unlock state (violating §1.3 invariant 3).

**Why not simply use bcrypt/scrypt**: the project Vault has already chosen Argon2id (ADR-059); keeping a single KDF algorithm reduces the cryptographic surface.

### Decision 2: Vault reuse — encrypting "sensitive extension fields"

**The layout** — the authoritative account table `accounts.json` (plaintext), the public view `user_profiles.json` (derived), and encrypted extensions `vault/accounts/{user_id}.enc` (optional):

```text
data_dir/
├── accounts.json                       # the authoritative account table (plaintext; password_hash is a one-way PHC string)
│   └── accounts[] = [{user_id, username, role, password_hash, display_name, ...}]
├── user_profiles.json                  # the public view (derived; the source of Runtime last_user_profile)
│   └── users[] = [{user_id, display_name, avatar, ...}]   # no username / role / password
│
└── vault/                              # the existing Vault directory
    ├── salt                            # the Argon2id master salt (unchanged)
    ├── openai.enc                      # the existing LLM key
    └── accounts/                       # a new subdirectory (optional — absent when there are no extension fields)
        ├── {user_id_1}.enc             # encrypted sensitive extension fields (api_secrets / recovery_codes)
        └── ...
```

**The embedded JSON structure inside `accounts/{user_id}.enc`**:

```json
{
  "schema_version": 1,
  "user_id": "...",
  "encrypted_notes": "...", // a ChaCha20-Poly1305-encrypted note-to-self (similar to a memo)
  "api_secrets": {         // the user's own API keys (if any)
    "openai": "sk-...",
    "anthropic": "sk-..."
  },
  "recovery_codes": [...]  // two-factor recovery codes
}
```

**Key design**:
- Account creation / password change / login does **not require** the Vault to be unlocked — only `password_hash` verification is needed.
- When the Vault is locked you can still log in, change your password, and view the public user list; only "reading the account's encrypted extension fields" requires an unlock.
- This happens to match user expectations: "my password = my account" is the primary credential, and the Vault is a secondary protection layer.

### Decision 3: HTTP authentication — a short-lived access_token + a long-lived refresh_token

```text
                    ┌───────────────────────────────────────┐
                    │            Gateway                    │
                    │  POST /api/auth/login                 │
                    │   → verify password_hash               │
                    │   → issue access_token (15 min, HS256) │
                    │   → issue refresh_token (30 day)        │
                    │                                       │
                    │  POST /api/auth/refresh               │
                    │   → verify refresh_token                │
                    │   → re-issue access_token              │
                    │                                       │
                    │  POST /api/auth/logout                │
                    │   → revoke the current refresh_token   │
                    │                                       │
                    │  middleware: extract AuthContext      │
                    │   → {user_id, role, as_user?}          │
                    │   → inject into AppState               │
                    └───────────────────────────────────────┘
```

**The token payload (HS256 + an independently persisted signing key)**:

```json
// access_token
{
  "sub": "user_id_xxx",
  "role": "user" | "admin",
  "iat": 1700000000,
  "exp": 1700000900,
  "jti": "..." // used for the blacklist
}

// refresh_token
{
  "sub": "user_id_xxx",
  "token_family": "...", // rotation detection: each refresh mints a new family and revokes all the old ones
  "iat": ...,
  "exp": ...
}
```

**Why HS256 + an independently persisted signing key (not derived from the Vault)**: this avoids introducing asymmetric key management overhead; the signing key is a 32-byte random secret generated on first startup and persisted at `data_dir/auth/secret` (Unix `0600`), **independent of the Vault master key**.

**The middleware path**:

```rust
// core/acowork-gateway/src/http/auth_middleware.rs (new file)
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // 1. Skip the whitelist: /api/health, /api/auth/login, /api/auth/refresh
    // 2. Extract the token from Authorization: Bearer
    // 3. Verify the HS256 signature + exp
    // 4. Inject the payload into req.extensions_mut::<AuthContext>()
    // 5. next.run(req)
}
```

**Implementation record (Phase C-2 complete)**:

- **Middleware location**: `auth_middleware` is mounted as a **global layer** inside CORS and outside the routes — if it were inside CORS the 401 responses would lack `Access-Control-Allow-Origin` (the browser cannot read the error body); if it were not outside the routes, a newly added handler could slip past authentication. When `state.auth_service == None` it directly calls `next.run(req)` (a local-mode no-op).
- **The whitelist** (the actually-measured paths; note that the real liveness endpoint is `/health` rather than the `/api/health` this ADR originally wrote): `/health`, `/api/status`, `/api/bootstrap`, `/api/auth/login`, `/api/auth/refresh`, `/api/auth/logout`, `/api/auth/first-login`. `OPTIONS` (the CORS preflight) is unconditionally allowed — the browser does not send `Authorization`.
- **`AuthContext`**: `{ user_id, role: Role, as_user: Option<String> }`; `effective_user_id()` only honours `as_user` when `is_admin()` is true. `as_user` validation is moved **forward into the middleware**: a non-admin carrying `as_user` gets an immediate 403 (rather than being silently ignored), eliminating the illusion of "it looks like it took effect".
- **Access token validation is stateless** (signature + `exp` only, no `accounts.json` read): this choice pins the upper bound of the "token still works after the account was disabled" window to `ACCESS_TTL_SECS` (15 minutes), in exchange for not doing a disk parse on every proxied request. **Refresh is the strong-consistency enforcement point**: `AuthService::refresh` re-reads `accounts.json` and validates `is_login_capable()` every time, so a disabled account survives at most 15 minutes.
- **Refresh rotation + reuse detection (RFC 9700 §4.14.2)**: a refresh token is **single-use** — each refresh marks the "presented family" as `rotated` (the `r:{family}` prefix in `revoked_families.txt`) and mints a new family. Receiving an already-`rotated` family is judged to be **a leak or a client replay**, and **all families of that user are revoked** (`{user_id}.*`), so the "attacker who refreshed first" also gets kicked off the line on the victim's next refresh. The `x:{family}` prefix denotes **explicit revocation** (logout) — replaying it only returns 401 and **does not** take down the user's other devices (logging out the phone should not kill the desktop). The two prefixes must be distinguished; this is the watershed between "logout" and "rotation" semantics.
- **Grace window + time-bounded revocation (implementation record, fixes "kicked to the login screen ~13 minutes after every login")**. Two defects in the strict mode above were closed in this round — ① a retry after a lost response is judged as reuse (the former `ponytail:` item), and ② `{user_id}.*` was an **unbounded** entry: once written it applied forever, and `login()` only mints, it never clears the register. A single misjudgement therefore **locked the user in a loop**: login → refresh 401 ~13 min later (15-min TTL minus the 2-min proactive lead) → login screen → login again → 401 again. The real trigger was two refreshes 3 ms apart inside one webview, not multiple devices. The fix:
  - Every entry in `revoked_families.txt` becomes `entry\t{unix-seconds}`. Timestamp-less legacy lines inherit the file mtime (**fail closed** — existing revocations do not lapse), so no migration of existing files is needed.
  - `{user_id}.*` changes meaning from "forever" to "**revoke every family with `iat <= cutoff`**". A newly minted login always has `iat` after the cutoff, so the wildcard is naturally immune to the next login — a misjudgement no longer needs manual cleanup; logging in *is* the recovery path (`a_misjudged_replay_recovers_on_the_next_login`).
  - `r:` entries carry a timestamp; a duplicate presented within `REFRESH_REUSE_GRACE_SECS = 10` rejects **that request only** and does not take down the chain. Past the window, `revoke_user` runs. The grace cannot be stretched indefinitely: legitimate rotations are ~13 minutes apart, far beyond 10 seconds, so an attacker replaying an old token still lands outside it (`refresh_rotates_and_replay_inside_grace_spares_the_chain` / `refresh_replay_outside_grace_kills_the_whole_chain`).
  - The `r:` stamp is written **exactly once per family** in the normal flow: every rotation mints a *fresh* family (not a successor in the same one), and a replay is caught at `rotated_at` before the family could be marked twice. `mark_rotated` still keeps a monotonic guard (never move a stamp backwards, latest write wins) — purely defensive against a retried persist or clock skew, since regressing the stamp would shorten the reuse window an attacker replaying this token has to clear. The grace window itself does not rely on the stamp advancing: two refreshes a few ms apart share one already-set stamp. Regression test `rotation_stamp_tracks_the_latest_spend`.
  - `revoke_user` / `revoke_family` take a new `now` parameter; all four call sites (password change / disable / reset / reuse cascade) pass it explicitly.

**How a downstream handler is written after `AuthContext` injection**:

```rust
pub async fn list_sessions(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Query(params): Query<ListSessionsQuery>,
    headers: HeaderMap,
) -> Response {
    let effective_user = auth.effective_user_id(); // an ordinary user = themselves; admin + as_user query = as_user
    let mut q = params;
    q.user_id = Some(effective_user);
    proxy_to_runtime_with_query(...)
}
```

### Decision 4: session isolation — `SessionMeta.user_id` + a `visibility` switch + owner verification on the Runtime side

**Division of duties**: ownership and visibility are adjudicated by the **Runtime** (it holds `meta.json`), while identity is provided by the **Gateway** (it holds the token). The Gateway does exactly one thing — write the caller's scope into the `x-user-id` header before proxying; the Runtime reads that header to filter and authorize. **The Runtime is the sole authorization decision point**, because it is the sole holder of the data the decision needs (`user_id` / `visibility`); adding a second owner check on the Gateway side would only create a second source of truth (it has to read the same meta, and it would also have to handle the race where "the meta was just deleted"). This supersedes the original draft's "the Gateway verifies owner when proxying" design.

**The shape** (`core/acowork-runtime/src/conversation.rs`):

```rust
pub struct SessionMeta {
    // ... existing fields ...
    /// ADR-076: the user_id that created this session. None = old data / no-account mode.
    /// Never changed after creation (write-once).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// ADR-076: the visibility switch. None is equivalent to `Public` — but **only for
    /// unowned** sessions; the creation path explicitly writes `Private` for owned
    /// sessions (see § Decision 4 "The two meanings of the default value").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<SessionVisibility>,
}

pub enum SessionVisibility { Public, Private }
```

**The two adjudication predicates** (`SessionMeta::is_readable_by` / `is_writable_by`):

| scope \ session | public (including `visibility = None`) | private |
|---|---|---|
| admin (`x-user-id: *`) | readable and writable | readable and writable |
| owner | readable and writable | readable and writable |
| another user | readable | **404** (neither readable nor writable) |
| local (no header) | readable and writable | readable and writable |

**The two meanings of the default value** (this revision):

In the predicates `visibility = None` reads as public, yet it originally carried two meanings at once: ① "sessions predating ADR-076" ② "nothing was declared at creation time". The second meaning is a real hole under multi-user — in a deployment bound to `0.0.0.0`, **every** conversation alice creates (including with the system agent) is **readable by bob by default**, until someone notices that the lock icon is not lit.

They are now split apart:

- **An owned session**: the creation path (`create_frontend_session`) writes `Some(Private)` at the same time as it writes `user_id`. It is **hard-coded on disk** rather than inferred by the read path — because `SessionListView.visibility` ships that value down to the Desktop to draw the 🌐/🔒 icon, and if a session is "actually private while the field is empty", that switch would show "public" for a session nobody can read, turning the only manual escape hatch into a lie.
- **An unowned session**: still writes `None` (= public). This both preserves zero changes in local mode (under local, `user_id` is always `None`) and preserves the fact that pre-upgrade historical data is not retroactively hidden — `is_readable_by` ignores the flag for unowned sessions anyway (with no owner there is nobody to restrict access to; honouring it would only hide the session from everyone).

**Unowned sessions also come in two kinds** (the other half of the same hole):

What was just closed is "owned but undeclared". What actually causes cross-user id collision is **unowned sessions** — `None` reads as public for them, and `is_writable_by` likewise unconditionally allows unowned sessions (that is for pre-upgrade historical data: a user must not be locked out of their own old sessions). Hence:

- **The agent cold-start session**: when the Runtime starts and finds no session on disk, it **creates one automatically** (`session_init`, so that `/latest-session` has something to return immediately and the ChatPanel is not blank). It comes into existence before any account has spoken, so **nobody can be its owner**. It previously landed in the "unowned = public" column → under multi_user **every account could read it, and every account could write to it**, and the Desktop's `selectAgent → /latest-session` happens to return it to every account: **the first user and the second user type in the same conversation**.
- **The fix**: mark it `Some(Private)`, and let the predicates acknowledge that "**unowned + Private = unclaimed**" — apart from admin, nobody can read it and nobody can write to it. The semantics are honest: a private session with no owner belongs to nobody, so it should not be handed to any account. Unowned + `None` / `Public` remains "public", so pre-upgrade historical data and local mode stay unchanged (under local the caller's scope is always `Unfiltered` and the predicates short-circuit on their first branch).
- **The companion change**: the claiming right is narrowed. `authorize_write` is deliberately lenient toward unowned historical data (otherwise users could not write their own old sessions), but "**who may re-share it**" is a narrower question — otherwise any account could flip a shared unowned session to private and hide it, or flip an unclaimed private session back to public and share it. `PUT .../visibility` now recognizes only admin for unowned sessions (403), with the rule living in one place, `may_change_visibility`.
- **Client companion**: `/latest-session` answers from an agent-level cache, and under multi_user as soon as two accounts have used the agent it will return 404 (it refuses to leak the id, see the 404 semantics in § Decision 4). This is not "the service has not come up yet", and retrying cannot cure it — the behaviour the ADR originally wrote is "**have the client fall back to the filtered list**". The Desktop now does so: when `/latest-session` is unavailable it first looks at its own scope-filtered list (if it has rows it opens the newest one, **and no longer spins for 10 seconds**), and only when the list is also empty does it create one of its own (owned + private).

Item-by-item adjudication is unaffected (the semantics of the table's three columns are unchanged); what changes is only "which column a newly created owned session lands in" and "unowned sessions split into two columns by the flag". An explicit `visibility` can still override the creation default in the `POST /sessions` body, and the per-session switch remains the escape hatch as before.

- **Reads** (`GET /sessions`, `/sessions/{sid}`, `/sessions/{sid}/messages`, `/sessions/latest`, `/sessions/{sid}/config`): go through `is_readable_by`. Anything unreadable is **404**, not 403 — a 403 would turn this endpoint into a "does session X exist" probe, which is exactly the information the list filtering is trying to hide.
- **Writes** (`open` / `close` / `DELETE` / `visibility` / `workspace` / `config` / all session actions): go through `is_writable_by`, **owner or admin only**. Public ≠ modifiable: making a session public means "let others read it", not "let others delete it". Old sessions with `user_id = None` (created before ADR-076) are writable **only** by admin / local — one cannot delete everyone's sessions just because they have "no owner".
- **A viewer reading a public session does not "activate" the backend session** (see § Decision 4 "the viewer does not activate" and "session memory reclamation" at the end of this section): `POST .../open` is a **write** operation, and the frontend **does not send it at all** when `can_write === false`. The reason is not to save a request, but that `Active` / `Closed` is **per-session global state** (not per-connection): if a viewer could activate it, they would create a resident session that "they have no right to close (close is a write authorization, deliberately not letting a bystander dismantle the owner's session), and the owner does not know who is holding it" — the lifecycle loses an accountable party. Read-only browsing **does not need** activation: history goes through `GET /messages` (read authorization), the event stream goes through the Desktop's wildcard MQTT subscription, and when the owner is using it (=Active) they naturally receive it; when the owner closed it (=Closed) there was nothing running anyway.

**Filtering must happen before pagination**: `scan_sessions_async` filters by scope first, then paginates — otherwise `total_count` / `total_pages` would count the rows the caller cannot see, which is equivalent to leaking "you have N more sessions belonging to others" through the pagination metadata.

```rust
// the caller's identity scope, parsed from the x-user-id header injected by the Gateway
pub enum SessionScope { Unfiltered, User(String) }
// "*" → Unfiltered (admin); a concrete id → User(id); header absent → Unfiltered (local mode)
```

`Unfiltered` covers both "admin sees everything" and "local mode has no account system" — the two are fully isomorphic on the data plane, so there is no need for two variants (one more variant means one more branch someone can forget to handle).

**The session control plane moves from MQTT to HTTP** (a reversal of ADR-034 §11.2):

The chain the original draft assumed, `POST /api/agents/{id}/sessions` → Gateway proxy → Runtime reads `X-User-Id`, **did not exist at the time**: the Desktop creates sessions over the MQTT control plane — it sends a `CreateSession` (with an empty body) straight to the broker, without passing through Gateway HTTP, so there is no proxy hop at which to attach a header; and the MQTT ACL is a shell (`can_publish` discards the topic parameter), so the broker cannot mark a message with an identity. **Identity cannot enter the MQTT control plane, so the control plane is moved to a place that can carry an identity.**

create / open / close / delete move to HTTP (all the usecases already exist; only the interface layer changes):

| Operation | HTTP route (Gateway proxy → Runtime) | Runtime usecase |
|---|---|---|
| create | `POST /api/agents/{id}/sessions` | `create_frontend_session` + `set_user_id` + `set_visibility` |
| open | `POST /api/agents/{id}/sessions/{sid}/open` | `resume_session` (the ADR-038 activation state machine) |
| close | `POST /api/agents/{id}/sessions/{sid}/close` | `close_session` |
| delete | `DELETE /api/agents/{id}/sessions/{sid}` | `delete_session` |
| switch workspace | `PUT /api/agents/{id}/sessions/{sid}/workspace` | `route_workspace_switch` |
| share / unshare | `PUT /api/agents/{id}/sessions/{sid}/visibility` | `set_visibility` + `write_meta` |
| switch model | `PUT /api/agents/{id}/sessions/{sid}/config` (`{model, provider}`) | `apply_config` |
| reasoning effort | `PUT /api/agents/{id}/sessions/{sid}/config` (`{reasoning_effort}`) | `apply_config` |
| rename title | `PUT /api/agents/{id}/sessions/{sid}/config` (`{title}`) | the `title` branch of `apply_config` |

**Why only workspace needs a new endpoint, while the other three reuse `PUT .../config`**: those 4 MQTT write commands already landed on exactly these two paths inside the Runtime (in `gateway_loop`, `ModelSwitchAction` / `ReasoningEffortAction` first try `svc.apply_config`, and `WorkspaceSwitchAction` directly calls `route_workspace_switch`). The `title` branch of `apply_config` is behaviourally equivalent to `update_title_force` (truncate + `title_set` + `write_meta` + `notify_config_change` + `config_version++`). But the **`workspace_id` branch of `apply_config` only does an in-memory assignment + `write_meta`**; it does not update `current_work_dir` and does not re-push the per-session workspace context / prompt files — using it as an equivalent would let tools work in the old directory while the meta claims the switch happened, so workspace goes through `route_workspace_switch` on its own.

**Key: the user-operation commands on the MQTT side have all been "deleted" — not deprecated, and not rejected.** In two batches: the **first batch** is 8 session-scope write commands (`create_session` / `delete_session` / `close_session` / `open_session` / `update_session_title` / `model_switch` / `reasoning_effort` / `workspace_switch`); the **second batch** is 8 session actions (`chat_message` / `stop` / `continue_execution` / `approval_decision` / `question_answer` / `cancel_tool` / `compress_action`, plus the duplicate command `compact_context` of `compress_action`). The corresponding proto fields of `ControlCommand` have been removed and **renumbered as a whole to be contiguous** (no compatibility requirement during development, so no gaps are left), and the corresponding variants of `ControlAction` / `InboundMessage`, the mapping arms of `control_action_to_inbound`, and the command-name mapping tables in the Gateway's `mqtt/client.rs` and the Tauri `chat_mqtt.rs` / `mqtt_client.rs` have all been deleted. The project is still in development with no compatibility requirement, so even the "send it and have it rejected" step is unnecessary — **the capability is inexpressible at the type level**:

| Deleted MQTT command | Why it cannot stay on MQTT |
|---|---|
| `create_session` | The message carries no identity → the created session is unowned (`user_id: None`), and an ownerless session is "writable by any logged-in account" (see `is_writable_by` in § Decision 4), which is equivalent to publicly deletable |
| `delete_session` / `close_session` | No identity → owner cannot be verified, and any broker client can delete someone else's session; and MQTT is fire-and-forget, so the victim cannot even see the error |
| `open_session` | Same (it can activate someone else's session) |
| `update_session_title` / `model_switch` / `reasoning_effort` / `workspace_switch` | Same: it can silently rewrite someone else's session title / model / reasoning depth / workspace (and workspace additionally means letting tools read and write in an attacker-specified directory) |

**Why the second batch (session actions) also moves**: the first version judged these commands as "acting on the already-open session, not carrying an ownership decision, and keeping them on MQTT has lower latency so it buys no authorization benefit" — but **the cost of the missing identity was underestimated**: which session they act on is entirely declared by the message itself, and any client on the broker can push an `approval_decision{approved:true}` into someone else's session (= executing arbitrary commands in their workspace), or push a `chat_message` into someone else's session. After the move the division of labour is completely clear: **HTTP = user-initiated (necessarily carrying an identity), MQTT = backend reporting**. Latency loses nothing — the HTTP handler only does "authorize + enqueue" and returns `202` immediately, and the real token stream still travels over MQTT events. The corresponding HTTP endpoints: `messages` / `stop` / `continue` / `approval` / `answer` / `cancel-tool` / `compress` (see [http.md §5.6](../../protocols/zh/http.md)).

What remains on MQTT are only **non-user actions**: `intent` (Runtime → Runtime, for cron / cross-agent) and `active_heartbeat` (the Desktop → Runtime presence heartbeat) — they carry no ownership decision at all and have no "throw it into someone else's session" semantics. The first version's anti-over-migration guard unit test `gateway_loop.rs::chat_traffic_still_maps_over_mqtt` has therefore **been deleted**: it was guarding precisely this batch of relocated commands; the two remaining commands need no guard — `ControlAction` only has `IntentReceived` + `ActiveHeartbeat`, and the compiler is the boundary.

**Identity delivery on the MQTT side is deferred for now** (originally the subject of ADR-077) — after the control plane moves away, there is no longer any command on MQTT that **requires per-account authorization** (only `intent` / `active_heartbeat` remain), so it is no longer a **write-side** blocker; but **per-user subscription authorization on the read side** (event-plane confidentiality) remains a known gap, see §5.5 and [mqtt.md §10](../../protocols/zh/mqtt.md).

**`can_write` delivery (the sole basis for disabling the frontend UI)**: `SessionSummary` (Runtime) and `SessionInfo` (TS) gain `can_write = SessionMeta::is_writable_by(&scope)`. The frontend **does not** infer permissions from `visibility`: public means "others can read", and admin / local mode can still write sessions they do not own. When the field is absent the frontend treats it as `true` (an old Runtime degrades to "try it, get a 403 from the backend" rather than locking every control down).

`create` incidentally solved another old problem: an MQTT command cannot return the new session id, but an HTTP response can. The Desktop still waits for the `session_created` MQTT event to get the sid, so the downstream logic changes not at all — `ponytail:` this migration **deliberately** did not clean that up in passing (the larger the change surface, the harder the rollback); to save that event round trip, just use the POST response body instead.

**Key security checkpoints** (the grep coverage list) — all landed:

- `Runtime::GET /sessions`: the `scope` parameter + filtering **before pagination**
- `Runtime::GET /sessions/{sid}` / `.../messages` / `.../latest` / `.../config`: `authorize_read` (**governed by `visibility`** — private and non-owner → 404)
- `Runtime::POST .../open`: `authorize_write` (**not governed by `visibility`** — `open` is a write operation: it activates the session into memory. A non-owner of a public session does **not** activate, and the frontend does not send the request at all when `can_write === false`, so read-only viewers are deliberately not accepted here. See § Decision 4 "the viewer does not activate")
- `Runtime::POST .../close` / `DELETE /sessions/{sid}` / `PUT .../visibility` / `PUT .../workspace` / `PUT .../config` / `POST .../files` / the second batch of 7 session actions: `authorize_write` (**not governed by `visibility`** — see below)
- **`visibility` only affects reads, never writes**: `is_writable_by` does not look at that field at all. The semantics of public is "let others read", not "let others modify" — otherwise making a session public would amount to handing over the close / delete / config-change buttons. All write paths are fixed to owner or admin.
- `GET /files/{document_id}` is **the only exception, and it currently has no check**: the blob is stored globally by `document_id` (`<work_dir>/files/`), and the read path cannot get the sid, so ownership cannot be traced back. **This is a known ceiling, see §5.5.**
- `Runtime::POST /sessions` (create): the owner is written from the `x-user-id` header and **does not accept an owner from the body** (the body only accepts `workspace_id` / `model` / `provider` / `visibility`)
- `Runtime::GET /sessions/latest`: the cache is agent-level (written at startup) and may point at a session the caller cannot read → it goes through `authorize_read`, and a 404 failure lets the frontend fall back to the list (`ponytail:` a non-owner pays one extra round trip, in exchange for "not leaking the id"; the alternative is a full scan on every startup call)
- Every Gateway → Runtime `/api/agents/{id}/sessions/*` proxy: must pass through `auth_middleware` (a global layer)

**The admin `as_user` mechanism** ("see it as user X", but **not identity impersonation**):

```text
GET /api/agents/{id}/sessions?as_user=<user_id>
# an admin token + as_user query → returns that user's session list
# an ordinary user token + as_user query → 403
# an ordinary user token + without as_user → only their own sessions
```

**Why `as_user` is not made into a "temporary identity switch"**: to avoid an XSS / CSRF attack chain — if the frontend could switch identity arbitrarily to perform write operations, cookie/header injection could walk straight through. `as_user` is only for **read-only views** (list / get_messages / get_state), and write operations (POST / DELETE) always use the token's actual identity.

#### Phase D implementation record (landed)

**Landed**:

1. **The Runtime shape (backward compatible)**: `SessionMeta.user_id: Option<String>` + `SessionMeta.visibility: Option<SessionVisibility>` (both `#[serde(default, skip_serializing_if = "Option::is_none")]`), an old `meta.json` without the field loads as `None` (= public, unowned); `ConversationSession::set_user_id` (**write-once**: the first write takes effect, and afterwards an attempt to **change it to a different value** is rejected with a warn; rewriting the same value idempotently is not an error — the meta's `user_id` is an immutable fact) + `set_visibility` / `is_private`. Tests: `session_meta_user_id_is_backward_compatible`, `set_user_id_is_write_once`.
2. **Gateway header hygiene (a security invariant)**: the proxy **forwards inbound headers verbatim** (`proxy_to_runtime_with_method` filters only hop-by-hop headers), so a client-supplied `x-user-id` would reach the Runtime ahead of the Gateway and impersonate another user's session. The middleware therefore **strips** `x-user-id` on **every** request first, and after authorization succeeds re-injects it from the token:

   | Identity | The `x-user-id` injected |
   |---|---|
   | an ordinary user | their own `user_id` |
   | admin + `as_user=<id>` | that `id` (a non-admin carrying `as_user` → 403; an ill-formed id is also 403 and never degrades to "unfiltered") |
   | local mode | strip only, no injection (the whole account system is a no-op; on the Runtime side = `Unfiltered`) |

   `x-user-id` therefore has **exactly one trusted writer: the Gateway**. Test: `middleware_strips_client_scope_and_injects_the_token_scope` (going through the real `build_router` layers: the forged header is stripped, no token → 401, admin gets `*`, `as_user` narrows the scope, an ill-formed `as_user` → 403).
3. **The Runtime scope-aware read path**: `SessionScope::from_header_value` (`*` → `Unfiltered`, a concrete id → `User`, header absent → `Unfiltered`); `scan_sessions_async` gains a `scope` parameter and filters **before pagination** (`total_count` / `total_pages` reflect the rows visible to the caller); `SessionInfo` gains a `visibility` field; `SessionMetadataService::list_sessions` gains a `scope` parameter; `GET /sessions/{sid}` / `/messages` / `/latest` go through `authorize_read` (unreadable → 404). Tests: scope determination + the three visibility states + pagination counts.
4. **The HTTP session control plane** (replacing MQTT, the first batch): the new `core/acowork-runtime/src/http/session_control.rs` (7 handlers in the first batch + the `authorize_read` / `authorize_write` helpers + `PUT .../visibility` + `PUT .../workspace`); `SessionManager::create_frontend_session` gains `user_id` / `visibility` parameters; a new `SessionManager::resume_session` (encapsulating the ADR-038 activation state machine — the MQTT `open_session` in `gateway_loop` now calls it); on the Gateway side `proxy.rs` gains 6 proxy routes (with body / method pass-through); on the Desktop side the new `src/lib/session-control.ts` wraps the HTTP calls, replacing `invoke("mqtt_publish_control")` — `create` / `open` / `close` / `delete` / `visibility` / `workspace` / `patchSessionConfig` (model / reasoning / title in one), replacing 5 call sites in total, and deleting `setSessionWorkspaceMqtt`, an old name that is no longer accurate. Tests: `visibility_and_ownership_gate_read_and_write` / `session_scope_from_header_value` / `scan_filters_by_scope_before_paginating` in `conversation.rs` (including the `can_write` delivery assertion: the owner can write, another user's public session is readable but not writable, ownerless is still writable, admin can write everything); handler wiring is proven by the existing HTTP server tests (`test_session_config_get_unknown_session` now returns 404, `test_http_upload_file_docx_lands_with_real_extension` requires the session to exist first) + Desktop `chatStore.test.ts`.

5. **Deleting the MQTT write path (two batches)**: the proto fields, `ControlAction` / `InboundMessage` variants and command-name mapping tables of the first batch of 8 lifecycle write commands + the second batch of 8 session actions (`chat_message` / `stop` / `continue_execution` / `approval_decision` / `question_answer` / `cancel_tool` / `compress_action` / `compact_context`) are all deleted; the `ControlCommand` field numbers are renumbered as a whole to be contiguous, leaving only `Intent` + `ActiveHeartbeat`, two non-user actions. The original guard unit test `gateway_loop.rs::chat_traffic_still_maps_over_mqtt` was deleted along with the second batch migration (it was guarding precisely the relocated commands); the boundary is now carried by the type system — `ControlAction` only has `IntentReceived` + `ActiveHeartbeat`.

6. **The frontend disables write controls based on read permission**: `SessionInfo.can_write` → `ChatPanel` derives `readOnlySession` → it is passed as `readOnly` to `ModelMenu` / `ReasoningEffortMenu` / `WorkspaceSelector`; `ToolbarDropdownTrigger` gains `disabled` (`disabled` + `aria-disabled` + `cursor-not-allowed opacity-50`), and that one change covers all three controls. The controls are **disabled rather than hidden** — a shared read-only session still has to show "which model / which workspace is currently in use". The i18n key `chatPanel.readOnlySession` is complete in five languages.

7. **The second batch of session actions migrated (ADR-076 § Decision 4 wrap-up)**: `core/acowork-runtime/src/http/session_control.rs` appends 7 action handlers (`messages` / `stop` / `continue` / `approval` / `answer` / `cancel-tool` / `compress`) + the shared `dispatch_session_action` helper — which only does "authorize + enqueue" synchronously (`202` / `403` / `404`), while the execution result still flows back over MQTT events; the Gateway's `proxy.rs` gains 7 proxy routes; the Desktop's `session-control.ts` gains 7 functions, and 8 call sites across `chatStore.ts` / `ChatPanel.tsx` / `ContextUsageIcon.tsx` change from `invoke("mqtt_publish_control")` to `fetch`. `idle_watcher.record_inbound()` moved from the MQTT control loop to the shared HTTP→MQTT forwarding point, fixing the "self-sleep" caused by HTTP-initiated actions not refreshing session activity. `compact_context`, being a duplicate command of `compress_action` (the SessionTask branch is byte-for-byte isomorphic and has no caller), is deleted as well. Tests: `session_actions_are_owner_gated_and_keep_their_payload` (owner gating + payload preservation) + 3 e2e tests switched to HTTP-driven; after the protocol field renumbering, the 6 golden hex values in `node_proto_golden.rs` are recomputed.

8. **Read-only browsing of public sessions (the viewer does not activate)**: the `ChatPanel` input box is disabled in `readOnlySession` + a dedicated placeholder; `SessionVisibilityToggle` (the 🌐/🔒 in the composer tool row) is wired to `PUT .../visibility`; `chatStore.closeTab` / `openSession` **each skip** `POST /close` and `POST /open` respectively when `can_write === false`. The key decision in item 8 is "the viewer does not activate" — the reasoning is above in "a viewer reading a public session does not 'activate' the backend session": `Active` / `Closed` is per-session **global** state, and a viewer activating it would create a resident session that "the viewer has no right to close and the owner does not know who is holding it", while correctly reclaiming it would require observer reference counting. Read-only browsing does not need activation (history goes through `GET /messages`, events go through the wildcard MQTT subscription, and when the owner is active it is real-time). Tests: Desktop `src/stores/sessionSharing.test.ts` (optimistic visibility flip + rollback; zero requests for open / close when `can_write === false`).

**`ponytail:` ceilings intentionally left behind**:

- **The MQTT control commands are deleted (this item is settled)**: the proto fields, Runtime variants and mapping tables of all user-operation commands (8 lifecycle + 8 session actions) are fully deleted, and the field numbers are subsequently **renumbered as a whole to be contiguous** (no compatibility requirement during development, so no gaps are left). `ControlCommand` now has only the two non-user actions `Intent` + `ActiveHeartbeat`.
- **`PUT .../config` does not fall back to `route_*`**: in the MQTT era, `ModelSwitchAction` / `ReasoningEffortAction` fell back to `SessionManager::route_model_switch` / `route_reasoning_effort` when `apply_config` errored (covering the scenario where "the session is not in the config service's in-memory table"). The HTTP `PUT .../config` has no such fallback and returns 500 directly. In practice it is unreachable: all three controls are bound to `activeSessionId`, and an active session must be in the table; and before the migration `setSessionContextWindow` was already a bare `PUT .../config` with no fallback, so being consistent with the neighbours is better than being consistent with a dead MQTT path.
- **`GET /sessions/latest` costs one extra round trip for a non-owner**: see the security checkpoints above. The cached value is not scope-resolved. The original reason was "resolving the scope requires a full scan" — but that premise has been overturned by the in-memory `meta/` index (see §5.5 "settled: the full scan of the `meta/` directory"), so resolving the scope is now just an in-memory filter over already-sorted rows and needs no new cache. **The behaviour is not changed for now**: this round is a pure performance change, and changing the visibility semantics of `/latest` needs its own tests and documentation; the upgrade path is open — take the newest row visible to the caller via `with_meta_index`.
- **The `visibility` switch already has UI (this item is settled)**: `SessionVisibilityToggle` hangs on the input box (composer) tool row, and only the owner can click it (when `can_write === false` it renders as disabled). **The default for new sessions is also settled (this round)**: an owned session is `Private` at creation, while unowned (local / pre-upgrade data) stays `None` = public — see § Decision 4 "The two meanings of the default value". The original sentence "per-agent default visibility is still not done and is deliberately left blank" is **resolved**: what is left blank is "whether admin can force a given agent's sessions to be visible", which is an admin-level policy ("this agent's conversations are a team-shared log") and not the same layer as "my new sessions default to private"; if it is ever needed, add an admin field to `accounts.json` / the configuration, and open a separate decision then.
- **Decoupling session memory reclamation from session lifecycle (unresolved, an independent topic)**: `SessionManager::evict_idle_sessions` is defined but has **no caller in the whole repository** (dead code); actual reclamation only exists at the agent level via automatic sleep (`process::exit`), and its renewal contains a **global** `ActiveHeartbeat` (any Desktop that has selected that agent renews it, carrying no user identity). Therefore "a viewer should not activate a session" is a design constraint directly related to that gap: we do not add another participant to a system that has no per-session GC. See §5.5.

### Decision 5: the admin role — `role = "admin"` bypasses filtering + special privileges

**The admin creation flow**:
1. On first Gateway startup, the configuration file `gateway.toml` contains `bootstrap_admin = { username, password }`
2. If `accounts.json` is empty → force the creation of that admin account at startup
3. Subsequent admins are created by an admin through an admin token (they need `username` + `display_name`, and the password is set by the created person on first login — the first-login flow: `POST /api/auth/login?invite_token=<xxx>`)

> **Review revision (during implementation)**: `bootstrap_admin` is a **first-boot-only** bootstrap credential, not a permanent "must always be configured" item. During implementation it is settled as:
> - `accounts.json` **empty** + `AUTH_MODE=multi_user` → ~~`bootstrap_admin` must be configured, otherwise refuse to start (fail-fast)~~ (**v2 revision**: an empty store instead seeds a passwordless admin + restricted mode, see § Decision 12 v2 / v3; it was originally "refuse to start");
> - `accounts.json` **non-empty** → `bootstrap_admin` is **ignored** (if still configured, a warn is logged).
>
> Rationale: requiring it on every startup would turn the bootstrap password into a **permanent second set of admin credentials** — it lies in plaintext `gateway.toml`, bypasses the password-change flow, and cannot be revoked, which is a long-term exposure. Gitea / Jenkins / GitLab bootstrap credentials are likewise first-boot-only. After creation, that account is fully governed by the normal password-change / disable flows.
> The checkpoint lives in `Gateway::new` (a constructor-period `Result`), so it is a genuine "refuse to start", not "log it after starting".

**The admin capability list**:
- ✅ `GET /api/users` sees all accounts (including `last_login_at`, `disabled_at`, but not `password_hash`)
- ✅ `GET /api/users/{any_id}` sees any account's metadata
- ✅ `GET /api/agents/{id}/sessions?as_user=<any>` sees any user's session list
- ✅ `GET /api/agents/{id}/sessions/{sid}/messages?as_user=<any>` sees any user's session messages
- ✅ `POST /api/users/{id}/disable` soft-deletes the account (`disabled_at` = now)
- ✅ `POST /api/users/{id}/reset-password` generates a one-time invite_token (24 h expiry)
- ❌ cannot change someone else's password (must go through reset → first-login password change)
- ❌ cannot `as_user` perform write operations (POST / DELETE are still validated against the token's actual identity)

**Admin soft delete** (`disabled_at`): `user_id` and `session.user_id` are retained unchanged; a disabled account's sessions remain readable (from the admin perspective), but a disabled account cannot log in. The distinction between deregistration and soft delete is covered in decision 6.

### Decision 6: the account lifecycle — register / log in / change password / deregister

**The API table**:

```text
POST   /api/auth/login              {username, password} → {access_token, refresh_token}
POST   /api/auth/refresh            {refresh_token}      → {access_token, refresh_token}
POST   /api/auth/logout             {refresh_token}      → 204
POST   /api/auth/change-password    {old_password, new_password} → 204  # requires an access_token
GET    /api/auth/me                 → UserAccount (redacted)
POST   /api/users                   {username, display_name, password} → UserAccount
                                       # admin-created; or an open-registration mode (see below)
POST   /api/users/{id}/disable      → 204  # admin only
POST   /api/users/{id}/reset-password → {invite_token}  # admin only
POST   /api/auth/first-login        {invite_token, new_password} → {access_token, refresh_token}
DELETE /api/users/{self}            → 204  # deregistering yourself (a soft delete)
```

**The registration-mode switch** (gateway.toml):

```toml
[multi_user]
registration_open = false   # false by default: only admin can create accounts
allow_public_signup = false # an extremely open mode (demo use only)
```

**Who can create accounts + the Desktop entry point** (settled in this round):
- `POST /api/users` always requires an **authenticated** caller — `allow_public_signup` (anonymous registration) is **not wired**, because it requires moving `/api/users` out of the authentication middleware's whitelist, which is a net expansion of the unauthenticated attack surface (anyone without credentials could spam accounts or guess invite_tokens), and there is no corresponding scenario in this ADR's target deployment (this machine / a small team) — YAGNI.
- Therefore the semantics of `registration_open = true` is "**any logged-in account can invite a new account**", not "anyone can register". The created account's role is **hard-coded to `user`** (`Role::Admin` can only be explicitly designated by an admin), so a non-admin cannot escalate through it.
- This switch previously only ran on the backend (the handler already returned 403 for `!ctx.is_admin() && !registration_open()` and has tests), and **the Desktop had no entry point**: the "+" button of the `Users (N)` group was hard-coded to `isAdmin`. That is now filled in — `GET /api/status` gains `registration_open: bool` (readable without authentication, on the same deployment-policy plane as `auth_mode`; it is constantly `false` when the account system is not running, to avoid the frontend rendering a button that is bound to return 403), and the Desktop's `fetchAuthPolicy()` retrieves `auth_mode` + `registration_open` in a single probe, so a non-admin sees the "+" only when the switch is open. Regression test: Desktop `UserList.registration.test.tsx` (a non-admin + open → there is a button; a non-admin + closed → no button; admin → always).
- After a non-admin creates an account they **cannot see the account they created** (their `Users (N)` lists only themselves, and `GET /api/users` is admin-only) — this is deliberate: that flow is "create an account + hand over the `invite_token`" (`InviteTokenModal` displays it), not account administration. A non-admin's `onCreated` therefore does **not** trigger `reload()` (that would hit the admin-only endpoint, get a 403 and raise a "load failed" banner).

**Deregistration vs. soft delete**:
- Your own `DELETE /api/users/{self}` → `disabled_at = now`; all sessions, chat history, avatar etc. are retained (the data can be restored by admin).
- admin `POST /api/users/{id}/disable` → the same, but the admin needs a second operation to restore.
- **No hard delete is provided**: account-related sessions / chats are historical data, and a hard delete would break referential integrity.

**The forced password-change flow**:
1. Changing a password requires `old_password`, and the new password is re-hashed with Argon2id.
2. After a successful password change: **all** of that user's refresh_tokens are revoked (all `token_family` values are killed), forcing a fresh login.
3. First-login password change: `invite_token` is single-use (burned on use) and is bound to the `user_id`.
4. Password policy (gateway.toml): `min_length = 8`, `require_digit = true`, `require_mixed_case = false`.

### Decision 7: Desktop UI — account switching + the sidebar User collapsible group

**The top-bar account menu** (replacing the current "user preferences" entry):

```text
[avatar] 大鱼 ▾
       ├─ Switch account...    → pops the login modal (clears chatStore + reconnects MQTT)
       ├─ Deregister this account  → a second confirmation → DELETE /api/users/{self}
       ├─ ─────────
       ├─ User preferences      → the old UserProfile editor (language/timezone/avatar)
       └─ Log out                → clears the token + returns to the login page
```

**The account switching implementation**:

```ts
// apps/acowork-desktop/src/stores/authStore.ts (new)
async function switchAccount(username: string, password: string) {
  // 1. POST /api/auth/login → tokens
  // 2. localStorage["acowork.auth.tokens"] = tokens
  // 3. reset(): chatStore, agentStore, sessionStore, userProfileStore
  // 4. mqttClient.disconnect() + reconnect() (with the new token)
  // 5. fetchAgents() / fetchUsers() / fetchSessions()
}
```

**The sidebar User collapsible group** (rendered at the same level as [AgentList.tsx](../../../apps/acowork-desktop/src/components/agent-list/AgentList.tsx)):

```text
┌─ Agent (12) ──────────┐
│  ▶ Node A (5)         │  ← the existing partitionAgentsByNode
│  ▶ Node B (7)         │
├─ Users (3) ───────────┤  ← the new partitionAccounts
│  ▶ 大鱼 (admin)       │     in the admin view, clicking enters "view as this user"
│  ▶ Alice              │
│  ▶ Bob                │
└───────────────────────┘
```

**`partitionAccounts` reuses the partition paradigm** (the same structure as [partitionAgentsByNode.ts](../../../apps/acowork-desktop/src/components/agent-list/partitionAgentsByNode.ts)):

```ts
// apps/acowork-desktop/src/components/user-list/partitionAccounts.ts (new)
export function partitionAccounts(accounts: UserAccount[]): AccountGroup[] {
  // Isomorphic to partitionAgentsByNode: a single collapse group "Users (N)",
  // collapsed by default, expanding on click; in the admin view each row can be
  // clicked to trigger "see sessions from that user's perspective"
}
```

**Why the User list is not grouped by Node like the Agent list**: user accounts are inherently a Gateway dimension — there is no such thing as "a user's Node"; users and nodes are orthogonal dimensions (one admin can manage agents on many Nodes). Forcing a Node-based collapse would only create noise.

### Decision 8: user-to-user chat — the Gateway-side conversation.json / jsonl

**The storage layout** (**all on the Gateway machine**):

```text
data_dir/
└── users/
    ├── {user_a_id}/
    │   ├── account.enc                    # the account's encrypted extension fields (decision 2)
    │   └── chats/
    │       ├── {user_b_id}/               # min(a,b) lexicographic order
    │       │   ├── conversation.json      # meta (analogous to SessionMeta)
    │       │   └── conversation.jsonl     # the message stream (analogous to jsonl)
    │       └── {user_c_id}/
    │           └── ...
    └── {user_b_id}/
        └── chats/
            └── {user_a_id}/               # the same min(a,b) path, no duplication
                ├── conversation.json
                └── conversation.jsonl
```

**Why lexicographic `(min, max)`**: it avoids bidirectional duplication (A→B and B→A write to the same directory) and simplifies the sync logic. Group chat is **not** supported (out of scope for this round; if needed later it can be extended to `groups/{group_id}/`).

**The conversation.json shape**:

```json
{
  "schema_version": 1,
  "chat_id": "min_user_a__max_user_b",
  "participants": ["user_a_id", "user_b_id"],
  "created_at": "2026-10-15T...",
  "last_active_at": "2026-10-15T...",
  "last_message_preview": "...",
  "unread_count_a": 0,
  "unread_count_b": 3,
  "version": 42
}
```

**One line per entry in conversation.jsonl**:

```json
{"ts":"2026-10-15T...","from":"user_a_id","kind":"text","body":"..."}
{"ts":"...","from":"user_b_id","kind":"image","body":"...","attachments":[{"id":"...","filename":"...","mime":"image/png","size":12345}]}
{"ts":"...","from":"user_a_id","kind":"document","body":"...","attachments":[{"id":"...","filename":"spec.pdf","mime":"application/pdf","size":67890}]}
```

**The kind set**: `text` / `image` / `document` (voice / video / reaction / edit / delete are **not supported this round**, following YAGNI).

**The attachment transfer details (implemented)**:

- **Limits**: `image/*` 25 MiB, everything else 100 MiB, tiered **by the client-declared mime** — so the mime is normalized before being stored (a bare `type/subtype` token, otherwise falling back to `application/octet-stream`), which is also what later made it eligible to be echoed back as a response header.
- **Body limit**: the `GLOBAL_BODY_LIMIT` of the Gateway root route is 64 MiB, below the 100 MiB document allowance, so the upload route **raises its own limit to 101 MiB** (`DefaultBodyLimit` hung on `chat_routes()`, rather than raising the global one). Regression test `the_upload_route_raises_the_global_body_limit`.
- **Download**: the response carries `Content-Disposition: attachment` + `X-Content-Type-Options: nosniff`. The stored mime is the client-declared one, so if inline rendering were allowed, a `text/html` would be a script running on the Gateway's origin. Images are still returned with their original mime (`<img>` loading is unaffected by `attachment` and displays normally).
- **Reads**: an attachment download has the same permissions as message reading (self-or-admin + must be a participant); anyone who is neither a participant nor admin gets a 404 — this does not leak whether a given attachment exists. The id is validated as a UUID before being used to build a path.

**Attachment storage (implemented; the deviation from the original draft is described below)**:

```text
data_dir/users/{min(a,b)}/chats/{max(a,b)}/files/{id}       the attachment blob ({id} = UUIDv4)
data_dir/users/{min(a,b)}/chats/{max(a,b)}/files/{id}.json  the attachment metadata (filename / mime / size)
```

**Why not the original draft's `{message_id}_{filename}`**: ① the upload happens **before sending** (the client uploads the file first, gets an `id`, and then sends a message referencing that `id`), so at that moment `message_id` does not yet exist; ② splicing a user-supplied string into a path introduces both traversal and name-collision problems at once. The on-disk name is switched to an opaque UUID and the metadata moved to a sidecar, and **the blob is written first and the metadata second** — a crash in between only leaves an orphan blob that nobody can reference, and never leaves metadata pointing at an empty file.

**Attachments inside a message carry only an id**: the `attachments` of `POST .../messages` is an **array of ids**, not an array of objects. The name / mime / size are all resolved from `files/{id}.json` and back-filled, and the client cannot declare something it never uploaded; `kind` is likewise derived server-side from the mime (`image/*` → `image`, otherwise → `document`) and the client-supplied `kind` is not accepted — otherwise a PDF could be labelled as an image.

**Why it lives in the Gateway rather than the Runtime**: user chat is unrelated to agents and is horizontal Gateway-dimension data; putting it in the Runtime would trigger the ADR-009 §5.4 boundary problem — it would need a brand-new Runtime entry point, which is complex and brings no benefit. **This is the explicit exception to ADR-009 §5.4**, explicitly written down in §5.4 of this document.

**The APIs**:

```text
GET    /api/users/{self}/chats                              → the chat list (including each chat's last_message_preview + unread_count)
GET    /api/users/{self}/chats/{other_user_id}/messages    → paginated messages for that conversation (offset/limit as in ADR-050)
POST   /api/users/{self}/chats/{other_user_id}/messages    {body, attachments: [id]} → 201 + the full message (including the server back-filled attachments)
POST   /api/users/{self}/chats/{other_user_id}/read        → clears your own unread_count
POST   /api/users/{self}/chats/{other_user_id}/files       multipart → upload an image/document, returns attachment_id
GET    /api/users/{self}/chats/{other_user_id}/files/{aid} → download the attachment
```

**Admin privileges**:
- ✅ admin can read any `chats/` (needed for emergency investigation)
- ❌ admin cannot POST a message impersonating someone else (the `from` field is forced to equal token.sub)
- ❌ admin cannot modify unread_count (read only)

### Decision 10: PM / Doc proxy identity injection — `X-Actor` changes from the hardcoded `human` to the real `user_id`

**The current state** (a constant introduced under the single-user assumption, in [pm_proxy.rs](../../../core/acowork-gateway/src/http/pm_proxy.rs#L145), with [doc_proxy.rs](../../../core/acowork-gateway/src/http/doc_proxy.rs#L145) isomorphic):

| Path | Policy | The injected value |
|---|---|---|
| `/api/pm/*`, `/api/doc/*` (REST, Desktop) | Drops the client-declared `X-Actor` and injects a trusted value | always `"human"` |
| `/api/pm/mcp`, `/api/doc/mcp` (MCP, Agent) | Validates that `X-MCP-Actor` ∈ the Gateway's `installed_agents` before passing it through; otherwise strips it (→ anonymous, read-only tools only) | the agent instance_id |

**The decision**: the security semantic of **"the REST side = the human operation side"** is **retained**, but the representation of "human" is upgraded from a global singleton constant to an account identity — the value injected by the proxy changes from `"human"` to `AuthContext.effective_user_id` (the token identity from decision 3, injected by auth_middleware):

```text
POST /api/pm/projects  →  auth_middleware parses the token → AuthContext
                       →  the proxy injects X-Actor: <effective_user_id>   (no longer hardcoded "human")
```

**The consumer-side semantic coupling in PM** (acowork-pm; the value comes from the header):

| Consumption point | Current (`"human"` constant) | After multi-user (user_id) |
|---|---|---|
| `create_project.created_by` | `"human"` | the real user_id |
| `create_project` self-bootstrapping ([tree.rs:505](../../../core/acowork-pm/src/store/tree.rs#L505)) | `created_by != "human"` → automatically added to members | both User and Agent creators are automatically added to members (see decision 11) |
| `create_task` review_status ([tree.rs:762](../../../core/acowork-pm/src/store/tree.rs#L762)) | `created_by == "human"` → NotRequired | a logged-in user creating it → NotRequired (the determination is by `kind`, see decision 11) |
| `ensure_assignee_is_member` (linked assignment) | `assignee ∈ ∅ ∪ members ∪ {"human"}` | `assignee ∈ ∅ ∪ members`, with no `"human"` special case (see decision 11) |

**The MCP path is unchanged**: the thing `X-MCP-Actor` is validated against is the agent instance_id (the ADR-073 identity), which is orthogonal to a user account; multi-user does not change that validation semantic.

**Security checkpoint**:
- The REST branch of `build_trusted_headers` must take its value from `Extension(auth).effective_user_id`, and **must not** fall back to a constant nor accept a client-declared value; when auth_middleware is not in effect, the PM/Doc REST proxy should return 401 (consistent with the global enforcement in decision 3).
- The `X-Actor` value domain changes from `"human" | instance_id` to `user_id | instance_id` — the PM side distinguishes by value domain and the `"human"` literal must not reappear; during the migration period a compatibility parse may be retained (receiving `"human"` is treated as an older Gateway).

### Decision 11: a multi-user PM member model — human operators become members (the §9 open question 8 decision: option B)

**The decision**: `ProjectMember` is extended to carry both kinds of identity, the human operator and the agent member are managed **symmetrically**, and the `assignee = "human"` arbitrary-human special case is removed.

**The data model** ([types.rs:210](../../../core/acowork-pm/src/types.rs#L210)):

```rust
pub enum MemberKind { Agent, User }

pub struct ProjectMember {
    pub instance_id: String,  // the field name is kept for compatibility: Agent → instance_id; User → user_id
    pub kind: MemberKind,     // new; #[serde(default)] = Agent → old project.json needs zero migration
    pub added_at: DateTime<Utc>,
}
```

- **Keep the `instance_id` field name + add `kind`**: the JSON contract is unchanged (the frontend `pm-types.ts` / `normalizeProject` need no field rename), and `kind` defaulting to `Agent` makes old data read out as Agent, with zero migration; the semantic extension is written into comments and into this document.
- **Why an explicit `kind` rather than a prefix encoding** (`user:` / `agent:`): it avoids conflating the semantics of user_id with instance_id; the schema migration intent is explicit; it aligns with the three-layer identity of ADR-073 / the user_id dimension of ADR-076. Prefix encoding stuffs the type into the value domain, destroys the readability of the UUID, and cannot be expressed with a serde default.

**The updated linked-assignment invariant**:

```text
old: task.assignee ∈ ∅ ∪ project.members ∪ {"human"}     (an arbitrary-human special case)
new: task.assignee ∈ ∅ ∪ project.members                  (humans and agents are isomorphic, no special case)
```

- `claim` / `submit` / `review`: the actor value domain = user_id (REST `X-Actor`) ∪ instance_id (MCP `X-MCP-Actor`), and it must be ∈ members — the validation logic is identical, with no branch.
- `create_project` self-bootstrapping ([tree.rs:505](../../../core/acowork-pm/src/store/tree.rs#L505)): whether `created_by` is a User or an Agent, it is **always** added to members. A human creator joining members is the self-consistent premise of "human members" (otherwise the creator themselves cannot be assigned / claim).
- `review_status` ([tree.rs:762](../../../core/acowork-pm/src/store/tree.rs#L762)): the determination changes from `created_by == "human"` to `kind(created_by) == User` → NotRequired; Agent → Pending.

**Migration** (executed once when multi-user goes live):
- All existing `members[]` are agent instance_ids → `kind: Agent` (the serde default holds automatically, no data rewriting required).
- `task.assignee == "human"` (the old "arbitrary human" assignment) → migrated to the creator's own user_id; during the migration period the PM side retains a `"human"` compatibility parse (treated as an older Gateway, see the security checkpoint in decision 10).

**The relationship with decision 10**: decision 10 solves "the Gateway injects a real identity" (`X-Actor` = user_id); this decision solves "the PM-side consumer is symmetrized". They are implemented together and neither can be missing — doing 10 without 11 leaves "arbitrary human" still relying on the `"human"` special case as a fallback; doing 11 without 10 leaves human member identity indistinguishable from the header.

### Decision 12: the deployment-mode split — `AUTH_MODE` is auto-inferred from the bind address

**The core**: avoid forcing the full multi-user chain in a single-machine self-hosted scenario. `AUTH_MODE` (a conceptual name; the configuration channel is in the §1.4 note) is auto-inferred from the bind address (and can be explicitly overridden), and in local mode the complete §1-§11 decisions degrade to no-ops. **This is not the "rollback switch" of §5.4 — it is a first-class configuration.**

**The mental model** (corresponding to [runbook §0](../../runbooks/single-machine-remote-topology.md) "there is no local/remote dual topology"): the architecture is always a single one; the difference lies in "external reachability" → "authentication strength". Loopback-only = the physical OS user management is the fallback = a trust domain; LAN exposure = an untrusted domain = a full account system.

**The inference rules**:

| bind configuration | Inferred AUTH_MODE | Triggering reason |
|---|---|---|
| `127.0.0.1` / `::1` (the default) | `local` | Only loopback can connect = the physical OS user management is the fallback = a single-machine trust domain |
| `0.0.0.0` / a LAN IP / a domain name | `multi_user` | Cross-machine / cross-user reachable = a full account system is mandatory |
| an explicit `--auth-mode local` / `multi_user` | Overrides the inference | Exceptional scenarios (forcing local behind a reverse proxy + internal-only access; forcing multi_user on loopback to demo it) |

**Priority**: CLI `--auth-mode` > the top-level TOML `auth_mode` > automatic bind inference > the default `local`. (The `auth_mode` key sits at the **top level** of `GatewayConfig`, not inside the `[multi_user]` section; the `[multi_user]` section carries `bootstrap_admin` / `password_policy` / `registration_open`, and is **implemented** — see §6.4.)

**The multi_user minimum configuration** (required on first startup, when `accounts.json` is empty):

```toml
auth_mode = "multi_user"          # or bind to a non-loopback address and let the system infer it

[multi_user]
bootstrap_admin = { username = "root", password = "change-me-1", display_name = "Administrator" }

[multi_user.password_policy]      # the defaults are the three values below
min_length = 8
require_digit = true
require_mixed_case = false
```

After the first startup `bootstrap_admin` can be deleted from the configuration — the account already exists; it only has meaning while `accounts.json` is empty (see the implementation revision in decision 5).

**Behaviour in local mode** (`AUTH_MODE=local`, **all of the §1-§11 decisions degrade to no-ops**):
- `HttpAuth` keeps the existing bearer token (the `data_dir/http_token` file; the access/refresh of decision 3 is not introduced)
- `user_profiles.json` keeps the current state (a plaintext display preference) and is not upgraded to the `UserAccount` shape (the user_id field keeps the existing logic; password_hash / role / disabled_at are not introduced)
- The `SessionMeta.user_id` field is **still written** on the write path (to keep the shape unified), but the read path **does not filter** — the `?user_id=` query is accepted but has no effect
- No admin role, no `bootstrap_admin`, no `as_user`, and the `/api/auth/*` routes are not registered
- The Desktop top bar keeps the existing "user preferences" entry and does not show an account switching menu
- User chat (decision 8) does not create the `data_dir/users/` directory; the `/api/users/{self}/chats/*` routes are not registered
- The PM/Doc proxy (decision 10) `build_trusted_headers` REST branch still injects `X-Actor: human` (the constant path, **not** going through the token identity)
- The PM member model (decision 11) **retains** the `assignee == "human"` special case (opposite in direction to decision 11 removing it — but since local mode has no token identity to inject, falling back to the original constant is the only reasonable path)

**Behaviour in multi_user mode** (`AUTH_MODE=multi_user`):
- All the §1-§11 decisions are fully enabled
- Startup check: ~~`bootstrap_admin` must be configured, otherwise refuse to start (fail-fast; it counterbalances the "missing configuration warning" of decision 5 — a warning can be ignored, a refused start cannot be bypassed)~~ (**v2 revision**: an empty store instead seeds a passwordless admin + restricted mode, see § Decision 12 v2 / v3; it was originally "refuse to start")
- The bind is automatically adjusted to `0.0.0.0` (if it is still loopback → warn only, **not forced** — so that multi_user can be demoed locally)

**The existence of UserAccount in local mode** (YAGNI: zero changes in local mode):
- `user_profiles.json` **keeps the current state** (the `UserProfile` shape is unchanged, with no `password_hash` / `role` / `disabled_at` fields); the credential table `accounts.json` is **not created**
- `AUTH_MODE=local` = the current state unchanged; the `DISABLED_PASSWORD_HASH` sentinel is **not used for local** — it is dedicated to an account under multi_user where "the admin has been created but the owner has not yet activated it on first login"
- When upgrading to multi_user there is a **one-time migration**: each `UserProfile` of `user_profiles.json` becomes a `UserAccount` in `accounts.json` (`password_hash = DISABLED_PASSWORD_HASH`), and the owner activates it by setting a password on first login via `invite_token`

**Rollback / downgrade** (aligned with §5.4, but at a raised level):
- multi_user → local: set `AUTH_MODE=local` or `--bind 127.0.0.1`; the frontend skips LoginView, token verification falls back to bearer, and the admin routes are not registered; the account files are retained but cannot be used to log in (the password hash is still there but no login UI triggers verification)
- local → multi_user: reuse the `invite_token` flow of decision 6 — existing users activate through "set a password on first login"; the first user created by an admin goes through the same invite flow

**Mainstream references**: GitLab / Gitea / Jenkins / Outline / Wiki.js / Plausible and other self-hosted products all tier their authentication strength by bind / external accessibility — local mode trusts physical access, and multi-user mode requires a full account system.

**Coupling with the earlier decisions**:
- **Decision 1**: all `UserAccount` fields are retained, with `password_hash` filled with a sentinel in local mode (the shape does not split)
- **Decision 5**: in local mode `bootstrap_admin` is not mandatory (the first admin = the physical OS user); in multi_user mode it is upgraded from "a missing-configuration warning" to "a missing configuration refuses startup", and then revised to "seed a passwordless admin + restricted mode" (see § Decision 12 v2 / v3)
- **The §5.4 rollback section**: `AUTH_MODE` is no longer merely an environment variable inside the rollback section but a first-class configuration — the rollback section degrades to "a downgrade within a mode"
- **§9 open question 1**: in local mode the notion of a "first admin" does not exist (the physical OS user is admin), while `bootstrap_admin` is retained in multi_user mode; the question is resolved by the mode split

**ponytail marker**: the bind inference logic only covers the `127.0.0.1` vs. `0.0.0.0` binary; IPv6 link-local (`fe80::/10`) is treated as multi_user (defaulting to the safe side). Super-scale scenarios (behind a reverse proxy + internal-only access) need a manual `--auth-mode local` override; the mixed trust-domain judgement of bind + reverse proxy is left to a future ADR.

#### § Decision 12 v2: an empty account store no longer refuses to boot — seeding a passwordless admin + restricted mode

**Revision motivation** (an implementation-period record): the original § Decision 12 rule "multi_user + an empty account store → fail-fast" pushed the responsibility of "create the first admin" onto the `[multi_user].bootstrap_admin` toml section. A new user runs `build_macos.sh --start --remote` (bind 0.0.0.0 → auto-inferred multi_user) → no toml → the gateway refuses to start → **the user never sees stderr** (the script does `> /dev/null`) → the Desktop cannot connect → a black hole. That is a terrible first-use experience, equivalent to "the product cannot be used by anyone".

**The new contract** (v2, **implemented**; see the passwordless seed path of `Gateway::new` in §6.4 + the `http/restricted_mode.rs` middleware):

1. **An empty account store + no toml `bootstrap_admin`**: seed a passwordless account with `username=admin` / `role=Admin` / `password_hash=DISABLED_PASSWORD_HASH` and enter **restricted mode** (`is_restricted()` returns `true`).
2. **HTTP behaviour in restricted mode**:
   - `/health` → 200
   - `/api/status` → 200 with the new field `requires_setup: true`
   - every other `/api/*` → **403 `{error: "setup_required"}`** (not 401)
3. **Lifting restricted mode**: the operator completes the first-time setup on the Gateway host, restricted mode is lifted, and normal service resumes. **No HTTP endpoint accepts a first-time password** — the password only travels via stdin / file / TTY / toml, **never over the network**.
4. **An empty account store + a toml `bootstrap_admin` already configured**: the behaviour is unchanged (the admin is created directly with the toml password, skipping restricted mode), retained as the zero-interaction startup path for non-interactive / container deployments.
5. **A non-empty account store**: the behaviour is unchanged (the `bootstrap_admin` toml is ignored + a warn).

**The three first-time setup entry points** (ordered by frequency of use):

| Entry point | Applicable scenario | Implementation |
|---|---|---|
| **TTY prompt** (automatic when no subcommand is invoked) | Local developers, ssh remote | `cli.rs`: after `Gateway::new`, detect restricted + `stdin`/`stdout` **both** being a TTY → `rpassword::prompt_password` twice for confirmation → write accounts.json. A non-TTY does not block startup (v3, see § Decision 12 v3) |
| **The `admin-setup` CLI subcommand** | systemd / Docker / non-TTY | `acowork-gateway admin-setup [--password-file PATH] [--password-stdin]`: read the password from a file / stdin / rprompt → `AuthService::set_admin_password` → **does not start the Gateway**, exits 0 |
| **The `[multi_user].bootstrap_admin` toml section** | Pure configuration-driven (k8s ConfigMap / image packaging) | Restart the daemon → `ensure_bootstrap_admin` takes path A → restricted mode is never entered in the first place |

**The security invariants** (v2 breaks none of the original security guarantees):

- **The first-time password never travels over the network**. There is no HTTP endpoint that accepts it. Even if a LAN attacker connects to the Gateway first, the open `/health` and `/api/status` do not accept passwords. SSH / physical access = already an admin.
- **Restricted mode + a LAN attacker + a remote Desktop**: the Desktop receives `requires_setup=true` → shows a "go set the password on the Gateway host" page and **does not** attempt to log in. The attacker cannot set the password on the operator's behalf.
- **The race window**: from "the daemon finishes writing `set_admin_password` to disk" to "the HTTP middleware next sees `is_restricted()==false`" is on the order of microseconds (an atomic accounts.json write within the same process + the middleware reading the disk directly).
- **Policy consistency**: `set_admin_password` goes through `PasswordPolicy::validate`, the same code path as an ordinary `change-password`.

**The relationship with the original § Decision 12**:

- The original "fail-fast" was v1's "security first" extreme choice, assuming the operator would definitely read the toml. **This assumption is disproven** (build_macos.sh swallows stderr). v2 keeps the security invariant "the operator must set a password" while **lowering "how to set it" from "must read the docs and edit the toml" to "one TTY prompt when the gateway starts" or "one CLI line"**.
- The `bootstrap_admin` toml section is **retained** — it is the compliance exit for the "pure configuration-driven" scenario, used by CI / k8s / CI image packaging.
- v2 is a **downgrade** of v1, not a replacement: the bind inference rules, all of local mode's no-ops, the existence of UserAccount in local mode, the rollback / downgrade paths, the mainstream references and the ponytail ceilings are all inherited.

**The implementation mapping**:

| File | Change |
|---|---|
| `auth/service.rs::ensure_bootstrap_admin` | Split into Path A (toml bootstrap_admin → an admin with a password) and Path B (an empty store → seeding a passwordless admin); the "non-empty store → bootstrap_admin is ignored" branch is retained |
| `auth/service.rs::set_admin_password` + `is_restricted` | New methods; set validates the policy and emits a one-shot fail; is_restricted is the sole criterion for the middleware / status field |
| `gateway/mod.rs::is_first_boot_restricted` + `set_admin_password` | Public façade for the daemon arm |
| `cli.rs::Commands::AdminSetup` | A new subcommand with three password sources (file / stdin / rprompt), **does not start the Gateway** |
| The `cli.rs` daemon arm | `is_first_boot_restricted()` → only prompts when `stdin`+`stdout` are both a TTY (exits if the disk was written without `--daemon`); **a non-TTY or a prompt failure only warns (stderr + the log file) and does not interrupt startup**, and the daemon starts HTTP as usual in restricted mode. See § Decision 12 v3 |
| `http/restricted_mode.rs` | A new middleware (implemented), hung outside `auth_middleware`, returning 403 `setup_required` |
| `http/routes.rs::SystemStatusResponse` | The new field `requires_setup: bool`, automatically carried by the `/api/status` serialization |
| The Desktop `authStore` + `SetupRequiredView` | Detects `requires_setup=true` → switches to the setup_required state + polls `/api/status` every 5 s → automatically continues with `init()` after the flip |

**The ceilings / what is not done** (deliberately left blank):

- **A remote Desktop cannot do it by itself** — the first password cannot be set from the Desktop UI. Either SSH in, or use the `admin-setup` subcommand, or use the toml. This is by design, not a bug (see safety invariant #1 in § Decision 12 v2).
- **The 5 s polling interval is hardcoded**. After setup is complete the user perceives at most a 5 s delay. Switch to WebSocket / SSE when sub-second latency is needed.
- **Restricted mode does not restrict MQTT**: it relies on the existing broker CONNECT authentication (`mqtt.auth_enabled`). If operations need "restricted mode = the broker also rejects every user:*", add a broker ACL — **not done currently** — the reason being that the MQTT credential already reuses the same secret as the HTTP bearer, a passwordless admin cannot obtain a token and therefore cannot connect to the broker, so no extra restriction is needed.

#### § Decision 12 v3: decoupling the first-time setup from daemon startup — the daemon always starts HTTP first

**Revision motivation** (an implementation-period record; the v3 comments are already referenced in `cli.rs`): v2 wrote the implementation of the first-time setup as "the `cli.rs` **daemon arm**: TTY detection + prompt + write to disk; a non-TTY **exits 1**". Measurement showed that this implementation **did not fix the very scenario that motivated it** — restricted mode is simply unreachable on the first-startup path:

1. `build_macos.sh --start` starts the process with `"$GATEWAY_EXE" ... &`. In a non-interactive shell a background job's stdin is assigned `/dev/null` (POSIX behaviour, confirmed by measurement under a pty), so `stdin.is_terminal() == false` → it takes the "non-TTY" branch → **`return Err` → exit 1, and HTTP never listens**. What the Desktop gets is still a connection refused, equivalent to the v1 black hole; only this time accounts.json has gained a seed that nobody can see.
2. Even when stdin is a TTY (an interactive foreground start), the prompt is ordered **before** `if self.daemon { async_main(...) }` and blocks: by the time the password is written `is_restricted()` is already `false`, and only then does HTTP start serving. In other words, **restricted mode has never actually served externally** — the `requires_setup` / `SetupRequiredView` / 5 s polling machinery would only ever appear during runtime when `reset_password` happens to hit it (which is a different failure, see the definition of `is_restricted()` in § Decision 12 v2).

**The v3 contract**: the first-time setup is **best-effort** and must never interrupt startup.

1. When `is_first_boot_restricted()` is true, the prompt only appears if `stdin` and `stdout` are **both** a TTY. `rpassword` reads and writes `/dev/tty` directly (it does not look at fd 0/1), so "both ends of stdio are terminals" is a conservative proxy for "somebody is sitting in front of it"; a redirected startup (a build script / systemd / a Tauri subprocess) never prompts.
2. If the prompt succeeds → write to disk; if `--daemon` was not passed, exit and let the operator decide when to start serving (the v2 semantic is retained, see the v3 comments in `cli.rs`).
3. **If the prompt is unavailable (non-TTY) or fails (the two entries do not match / the policy is not satisfied) → only warn, do not interrupt**: `eprintln!` + a `tracing::warn!` naming the three solutions, and then continue as usual. `--daemon` will start HTTP, and restricted mode begins **serving externally**.
4. Restricted mode is therefore genuinely reachable: `/health` and `/api/status` (carrying `requires_setup: true`) return 200, and every other `/api/*` returns 403 `setup_required`; the Desktop renders the prompt page and polls every 5 s, and after the operator runs `admin-setup` on the host and writes to disk, restricted mode is lifted on the middleware's next disk read, **with no restart required**.

**Why the solutions must be written into the log file**: the motivating scenario's stderr is thrown into `/dev/null` by the script, so `warn_first_boot_restricted()` simultaneously emits a `tracing::warn!`, which lands in `data_dir/logs/*.log` — that is the real answer to "the user sees absolutely nothing".

**Measured** (`--home <tmp> --auth-mode multi_user --daemon --addr 127.0.0.1:21999`, with all of stdio redirected):

| Request | Result |
|---|---|
| The process | survives (under v2 it would be exit 1 here) |
| `GET /health` | 200 |
| `GET /api/status` | 200 + `requires_setup: true` |
| `GET /api/users` (no token) | **403 `setup_required`** (not 401, see the bug fixed below) |
| `POST /api/auth/login` | 403 `setup_required` |
| After `admin-setup --password-stdin` | `requires_setup: false`, login obtains a valid token pair, `/api/users` returns 401 again |

> **The bug fixed in this round**: the `restricted_mode` middleware was originally hung with `.layer()` **before** `auth_middleware`, and axum layers work as "the later one is the outer one, and requests travel from the inside out" — so it was actually **inside** auth, and a tokenless request in restricted mode was answered by auth with a 401, the opposite of the v2 contract "403, not 401". It is now hung outside auth, and a regression case going through the real `build_router` was added (`real_router_answers_403_not_401_without_a_token`).

---

## 5. Consequences

### 5.1 Positive

1. **A minimally invasive account system**: it reuses the existing Vault master key, the partition paradigm and the SessionMeta persistence paradigm, introducing no new cryptography, no new storage layer, and no new collapse component.
2. **Session isolation is a single-field change**: on the Runtime side only one `SessionMeta.user_id` field + one `?user_id=` query parameter are added; all other code paths stay untouched.
3. **admin does not break the permission model**: admin is a `role` field inside the token, not identity impersonation; `as_user` is strictly read-only, and write operations are validated against the token's real identity.
4. **User chat is data the Gateway owns**: it is not coupled to the Node agent topology; adding group chat / reactions / read receipts later is a schema upgrade that does not involve the Runtime.
5. **Token rotation** (the refresh token family rotation): leaking one refresh token does not let an attacker live forever — the next refresh invalidates all the old families.

### 5.2 Negative / costs

1. **The login-state middleware is a full-stack retrofit**: all existing handlers must pass through auth_middleware, and handler function signatures need to add `Extension(auth): Extension<AuthContext>`. Roughly 30+ handlers have to be touched.
   > **Implementation revision (Phase C-2)**: after the middleware was made a **global layer**, handlers **do not** have to change their signatures one by one — `auth_middleware` already blocks every request without a token, and only the handlers that **need the identity** (proxy injection, `/me`, `/change-password`) add `Extension<AuthContext>` on demand. The actual retrofit cost drops from "30+ handlers" to "a few handlers + one layer", which is the direct benefit of hanging a global layer rather than a per-route `route_layer`.
2. **Session isolation is spread across multiple Runtime handlers**: the filter and the owner check must cover list / get / messages / latest / open / close / delete / visibility, every entry point; missing one is a data leak. This has been converged onto two shared predicates (`is_readable_by` / `is_writable_by`) + two shared helpers (`authorize_read` / `authorize_write`), so a new handler only needs to call the helper — but a grep ceiling lint is still needed as a backstop (see §6.6).
3. **The two Desktop stores coexist during the transition**: `userProfileStore` (old) + `authStore` (new) coexist for a while, and during the migration their data may be inconsistent; a clear deprecation path is required.
4. **The storage cost of token revocation**: the refresh token family must be persisted to support the "revoke the whole family" semantic, either in a separate file or in Redis; this round chooses a file (`data_dir/auth/revoked_families.txt`), which is acceptable at small scale.
5. **The first-startup threshold (in multi_user mode)**: ~~the first admin must be created via the `bootstrap_admin` configuration; a missing configuration leaves the system idling with nobody able to log in — fail-fast refuses to start~~ (decision 12 v2 revised it to seeding a passwordless admin + restricted mode, see § Decision 12 v2 / v3), rather than only warning. There is no such threshold in local mode (the first user is created automatically by `bootstrap/orchestrator.rs`, see "The existence of UserAccount in local mode" in decision 12).

### 5.3 Boundaries / exceptions

**The extension clause of ADR-009 §5.4** (appended during the ADR-009 v3 revision):

> **Exception — user account and chat data** (ADR-076 § Decision 8): account credentials (`data_dir/vault/accounts/`), account metadata (`data_dir/user_profiles.json`), and user-to-user chat records (`data_dir/users/*/chats/`) belong to the Gateway process and are read and written by the Gateway directly via the filesystem. This data has no corresponding agent instance and is therefore not "Runtime private data". Runtime session data is still accessed only through the Runtime HTTP reverse proxy; this exception is limited to the three categories above.

### 5.4 Rollback

**Mode-level rollback** (the first class, controlled by the `AUTH_MODE` of § Decision 12):
- multi_user → local: set `AUTH_MODE=local` or `--bind 127.0.0.1`; the frontend skips LoginView, token verification falls back to bearer, and the admin routes are not registered; the account files are retained but cannot be used to log in (the password hash is still there but no login UI triggers verification); `SessionMeta.user_id` is written but not filtered; the user chat routes do not respond; the PM/Doc REST proxy falls back to injecting the `X-Actor: human` constant
- local → multi_user: reuse the `invite_token` flow of decision 6 — existing users activate through "set a password on first login"; the first user created by an admin goes through the same invite flow

**Field-level rollback** (the second class, fine-grained degradation within a mode):
- `UserProfile` → `UserAccount` is an in-place upgrade (the migration script converts fields within the same table); rolling back = deleting the `user_id` / `password_hash` fields
- The `session.user_id` field is optional; deleting it disables the filtering (reverting to everyone sharing) — **multi_user mode only**, since local mode never filtered in the first place
- The auth middleware is optional: retain the `HttpAuth` bearer token as a fallback path
- Chat data lives in an independent directory, so deleting `data_dir/users/*/chats/` amounts to "user chat not enabled"
- The PM/Doc proxy identity (decision 10): under multi_user `build_trusted_headers` injects `auth.effective_user_id`; under local it injects the constant `human` (the decision 12 coupling)
- The PM member model (decision 11): `ProjectMember.kind` carries `#[serde(default)]`, so rolling back merely discards the kind field and the data file shape stays compatible; the `"human"` compatibility parse is retained until the migration completes before removal (the multi_user mode path; the local mode retains the `"human"` constant path)

### 5.5 Known technical debt

> This section mixes **change records** (marked settled / implemented) with **unsettled ceilings**. To see only "what is still not done" jump straight to [§10 The remaining list](#10-remaining-list-the-full-set-of-what-has-not-been-done) — that section is an **index** of this section plus §7.2 / §9, ordered by "trigger condition", and it also separately lists the **rejected** approaches.

- **ponytail: access token validation is stateless** (signature + `exp` only, no `accounts.json` read), so the window in which "a token still works after the account was disabled" = `ACCESS_TTL_SECS` (15 minutes). This is a **deliberate upper bound**, not an oversight: the cost is one disk parse per request, and the benefit is only 15 minutes. The true strong-consistency point is `refresh` (which re-reads the store every time). To take effect immediately → add an in-memory `user_id → revoked_at` set checked in `verify_access` (no disk I/O). Move to RS256 + a revocation list when the user count exceeds 100 or when instant kick-out is needed.
- **✅ Closed: refresh token single-use + reuse detection could hurt retries** (former `ponytail:` item). The defect: a client sends a refresh, the server processes it successfully, but the response is lost on the network; the retry with the same token is judged "reuse" → every family of that user is revoked. Worse, `{user_id}.*` was unbounded and `login()` never clears the register, so this **locked into a loop**: login → 401 on refresh ~13 minutes later → login screen → login again → 401 again. The observed trigger was two refreshes 3 ms apart inside the same webview, not multiple devices — see §Decision 3 "single owner of the Desktop tokens". The implementation is the original upgrade path: a timestamp on the `r:` entry + a 10-second grace determination, with the wildcard entry changed to an `iat <= cutoff` time basis (a new login is naturally immune; logging in is itself the recovery path). See the §Decision 3 implementation record and the module docs in `core/acowork-user/src/auth/revoked.rs`.
- **ponytail: `revoked_families.txt` is a flat file with no GC**, appending a line on every refresh. The target scale (fewer than 100 users) is fine; beyond that a SQLite table + an expiry column is needed.
- **ponytail: the user chat list is an fs scan**, O(n) over `data_dir/users/`. Acceptable when n < 1000; beyond that a SQLite index is needed.
- **ponytail: the `as_user` query is a string pass-through on the proxy chain**, so a future proto upgrade would need a structured field.
- **Not implemented**: a multi-device concurrent session limit, forced password expiry (this round only records `password_expires_at` without enforcing it), account lockout (5 failures → a 15-minute lock), **login endpoint rate limiting** (`POST /api/auth/login` has neither rate limiting nor lockout, and §7.4 and §10.2 item 14 record this as a "trust boundary gap" — the only mitigation is the default bind of `127.0.0.1`) — the first three belong to a future ADR, and the fourth must be added **before exposing the Gateway to an untrusted network**.
- **Settled: a single write authority for the display fields (language / timezone / avatar …) under multi_user**. Previously two write paths each did their own thing: `PUT /api/users/{id}` only accepted `display_name` + `role` (the Desktop's remaining preference fields were silently ignored by serde, a no-op that reported no error), while `/api/user/avatar-*` directly modified the shared "active user" in `user_profiles.json` — and `account_api::sync_profiles` **rebuilds** that view from `accounts.json` in full on every account change, so an avatar change was wiped out by the next account change, and under multi_user the "active user" is itself meaningless. It is now converged onto a single authority, `accounts.json`: `AuthService::update_account` now takes a `ProfilePatch` (all the display fields; `None` = unchanged, an empty avatar string = clear, `display_name` keeps the trim + blank-ignoring contract), and `UpdateAccountRequest` passes it through with `#[serde(flatten)]`; under the multi_user branch the avatar route writes through `accounts.json` by `AuthContext.user_id` and then goes through the same `sync_profiles` (the local mode keeps the active-user path untouched). Regression tests: `update_account_applies_the_display_patch` (service layer: the patch semantics + persistence), `update_account_persists_display_fields` (HTTP: PUT the display fields → the authoritative read agrees), `avatar_config_writes_through_accounts_under_multi_user` (set the avatar first → then change display_name to trigger the rebuild → the avatar is still there; the old implementation would necessarily fail). **Asset ownership (settled)**: avatar files are namespaced per account under `assets/avatars/{user_id}/` — upload / list only see your own namespace, and delete does an ownership guard **before** unlink (a path prefix outside your own namespace → 403; admin has no bypass: dereference via `PUT /api/users/{id}` first and then delete; the guard is before the unlink because "reject after deleting" means the file is already gone), while GET stays readable pool-wide (avatars are meant to be shown to others, read isolation is not a requirement); local mode retains the `assets/` root (a single user has no cross-user vector). The user_id's character set is validated at the boundary (alnum / `-` / `_`) before entering a path, and the shape of a token claim is not trusted. No migration of existing assets is done — multi_user has no published data yet (the ADR is unreviewed and the Desktop authStore is not wired), legacy `assets/avatar-XX` paths are still readable on GET, and re-uploading migrates them. Regression tests: `avatar_file_deletes_are_confined_to_the_owner_namespace` (another user / admin deleting → 403 and the file survives; the owner deleting → 200 + the field cleared via the authority), `avatar_target_dir_is_per_user_under_multi_user_and_shared_under_local` (directory resolution + rejecting an out-of-bounds user_id). **Still open: the upload quota**. Phase 6 evaluated it: when the attachment upload landed, the quota was **not** added in passing, and this round goes further and **rejects the per-user quota framework** — disk is a shared resource, and a per-account limit only makes sense in a multi-tenant deployment where "the quota is itself a fairness contract" (otherwise one user's 100 MiB limit × 50 users = 5 GiB protects nothing), and "the size of a single file" and "the total per account" are not the same thing either. The only writer this round, `store_attachment`, already tiers the single-file size by mime. **If it is ever done, switch to a global water mark**: before `store_attachment`, total up `data_dir/users/**/files/` and reject once it exceeds a water-mark threshold of `data_dir` (one number, one check, protecting the resource that is actually being contended for). Trigger condition: real disk pressure appears, or mutually untrusting multi-tenancy is introduced.
- **ponytail: the pagination count for public sessions is "the number of visible rows" rather than "the total number of rows"**: the filter happens before pagination, so `total_count` / `total_pages` only count the sessions the caller can see. This is deliberate — paginating by the total would leak "others have N more sessions" — but the cost is that different users see different page boundaries for the same data, so the frontend **must not** cache cross-user pagination results.
- **Settled: the MQTT commands of the session control plane are all deleted** (the proto fields + the Runtime variants + the Gateway/Tauri mapping tables), and the field numbers have been renumbered as a whole to be contiguous. The protocol documents (`mqtt.md` / `http.md` / ADR-034 §11.2.B) have been synced to "deleted + migrated to HTTP". **Still unsettled**: the read-side confidentiality of the MQTT event plane (there is no per-user topic ACL, and `mqtt.md` §10 already marks it as deferred / a known gap).
- **Settled: the full scan of the `meta/` directory has been replaced with an in-process in-memory index**. Previously every listing was `scan_sessions_from_meta` = `read_dir` + per-file `read + serde_json`. Measured (2000 metas / 1.5 MB, a warm local APFS cache): **23 ms** in release, **42 ms** in debug, while `read_dir().count()` takes only **0.9 ms** — release being only 1.8× faster than debug shows the bottleneck is the **2000 `open`/`read`/`close` system calls (~11 µs per file), not JSON parsing**. The real scale (1–17 sessions per agent on this machine) is ~0.2 ms per call, so at the time this was judged a **shape** problem (growing linearly with history) rather than a current problem. It has now landed exactly along the upgrade path written down at the time: `META_INDEX` (an in-process `HashMap<session directory, MetaIndex>`; a Runtime process handles one agent, so production has exactly 1 entry) holds the sorted `Vec<(String, SessionMeta)>`, and `scan_sessions_async` / `find_latest_session` / `prune_excess_sessions` read the cache; the **only write path** is `write_session_meta` (upserting as it writes, so there is no "remember to invalidate" step), and the **only delete path** is `remove_session_meta` (both `delete_session` and prune converge there). Validity is probed with `read_dir().count()` (rebuild when it disagrees, self-healing). Two additions for correctness: the sort gained a `session_id` tie-break (otherwise the cached index and a rescan would disagree on the page boundary for sessions tied at the millisecond level), and `dev/ci.sh`'s `run_meta_layout_redline` confines the meta path construction to `conversation.rs` (everything else is only a test fixture, a fixed upper bound, or only-decreasing). **It was deliberately not persisted as an index file** — that would be a second source of truth, already rejected in § Decision 2 of this ADR. Regression tests: `session_index_reflects_funnel_writes_without_rescanning` (when the same meta is rewritten the entry count does not change, and only a write-side upsert lets the listing see the new value), `session_index_self_heals_on_out_of_band_meta_change`, `remove_session_meta_drops_the_session_from_the_listing`.
- **Settled: the session count cap is bucketed by owner** (`prune_excess_sessions`): previously the trimming was global, ordered by `last_active_at`, so under multi_user one account creating a new session would archive **another** account's oldest session (the `.jsonl` → `.jsonl.archive` + delete meta → it disappears from that account's listing with no recovery entry). Now it is bucketed by `SessionMeta::user_id` and each bucket applies `max_sessions` **independently**; `user_id = None` (pre-ADR-076 data / local mode) forms its own group and keeps the old semantics. Regression test `prune_excess_sessions_is_per_owner_not_per_agent`: alice 2 / bob 3 / unowned 3, cap 2 → only the two over-cap buckets each lose 1; **alice's timestamps are deliberately the oldest**, so the old implementation would necessarily fail.
- **Settled: the `visibility` switch already has UI**. The per-session switch is in the input box (composer) tool row: the 🌐 / 🔒 icon calls `PUT /api/agents/{id}/sessions/{sid}/visibility`, clickable only by the owner (`can_write === true`), and rendered as disabled for a non-owner (rather than hidden, so a bystander knows what they are looking at); with optimistic updates and rollback on failure. **The default for new sessions is also settled** (owned → `Private`, unowned → staying public; the full semantics are in § Decision 4 "The two meanings of the default value", and the regression tests are in §7.1's `owned_sessions_start_private_and_ownerless_stay_public` and `only_an_admin_may_re_share_an_unowned_session`). The sentence that originally read "per-agent default visibility is deliberately left blank" was the **second layer of the same hole** and is now decided: what is left blank is "whether admin can force a given agent's sessions to be visible to all accounts" — that is an admin-level policy ("this agent's conversations are a team-shared log"), which is not the same layer as "my new sessions default to private"; if it is ever needed, add an admin field to the configuration / `accounts.json` and open a separate decision then.
- **Settled: "read-only open" of a public session**. The approach is **the viewer does not activate** (rather than granting viewers `open` permission): when `can_write === false` the Desktop's input box is disabled with a "read-only session" placeholder, and `openSession` / `closeTab` skip `POST /open` / `POST /close` respectively; history is still loaded via `GET /messages` (read authorization), and the event stream is delivered by the wildcard MQTT subscription when the session is genuinely Active. **Why not grant `open` read authorization** (it was implemented once and then withdrawn): `Active` / `Closed` is per-session **global** state, and a viewer activating it would produce a resident session that "the viewer has no right to close (close is a write authorization, deliberately not letting a bystander dismantle the session), and the owner does not know who is holding it" — correctly reclaiming it would require observer reference counting, and the Runtime currently happens to have **no** per-session GC (see the previous item). Regression protection: the Desktop `src/stores/sessionSharing.test.ts` (the optimistic visibility flip + rollback; zero requests for open / close when `can_write === false`).

- **ponytail: the MQTT event plane has no per-user subscription isolation** (a read-side confidentiality gap). `session isolation` only covers the **write side** and the **HTTP read side**: in theory any client that can connect to the broker can SUBSCRIBE to any `agents/{id}/sessions/{sid}/messages/#` and see another user's session event stream. This is a ceiling — `rumqttd` 0.20 has no per-topic ACL capability, so **it cannot be fixed on the existing broker** (the user's decision: MQTT user authentication is deferred for now). Mitigation: the broker binds `127.0.0.1` by default (an attacker must first be able to reach the local loopback). The upgrade path = switch to mosquitto (a Phase 5b evaluation) or add a tokenized subscription proxy to the event plane. It is already marked as "deferred / a known gap" in [mqtt.md §10](../../protocols/zh/mqtt.md).
- **Settled + implemented: an ordinary user "initiating" a conversation** (user chat, § Decision 8). The original gap: resolving the recipient requires a user directory, but `GET /api/users` is admin-only, and the "send a message" entry point was only hooked onto the right-click menu of the admin-only sidebar `UserList` (an ordinary user only sees their own row) — so an ordinary user could only **reply**, never initiate. **The decision (the user delegated: "you decide")**: add `GET /api/users/directory`, readable by **any authenticated account**, returning the three fields `user_id` / `username` / `display_name`, **excluding disabled accounts** and **excluding the caller**. The basis for the trade-off: ① these display fields are already a public view of the deployment — `user_profiles.json` is rebuilt from `accounts.json` and consumed by the Runtime as `last_user_profile` (§ Decision 1), so names are not a secret in this context; ② a chat feature where the recipient cannot be named is unusable **for everyone except admin** (i.e. for everyone it is meant to serve), and a "privacy deadlock" is worse than "enumerable usernames". On the Desktop side the `MessagesView` left column header gains a "new conversation" selector (the contacts come from that endpoint), and the admin's `UserList` right-click "send a message" degrades to a shortcut rather than the only entry point. Regression test: `user_directory_is_readable_by_any_account_but_bounded` (an ordinary user can read it + disabled accounts do not appear + admin can read it).
  - **ponytail: the directory is fully enumerable by any authenticated account** (a residual ceiling). The endpoint exposes no email / timezone / custom fields and no admin-only fields, but the complete set of `username`s is visible to every account. For a personal / small-team deployment (this ADR's target scale) that is acceptable; converging it would require "first query by exact username / only return existing conversation counterparts / an invite-based address book" — all of which need a new product decision and cannot be solved by just adding a filter.
- **ponytail: attachments have four known ceilings** (all marked in place in the code, § Decision 9). ① **No blob reclamation**: the blob is written before the metadata, and a crash between the two writes leaves a file nobody can reference; the upload itself requires authentication and the failure window is extremely narrow, so no background sweeper thread is added for it — to reclaim, scan `files/` for "no sidecar and older than N days". ② **The whole file is read into memory on download**: `load_attachment` returns `Vec<u8>`, with a single cap of 100 MiB (acceptable for the desktop scenario over a local loopback); to make it streaming, switch to `tokio::fs::File` + `ReaderStream`, at the cost of one more dependency. ③ **The upload size limit is tiered by the client-declared mime**: declaring a 100 MiB document as `image/png` is only **stricter** (25 MiB), and there is no leak in the reverse direction — what actually determines the handling is the mime the storage side normalized itself, so the limit only affects the accepted volume. ④ **The upload route's body limit is an estimate of "the document cap + 1 MiB of envelope headroom"** (`chat_api::UPLOAD_BODY_LIMIT`): the multipart envelope also contains the boundary + the filename + the headers, so **a file right at the 100 MB boundary can still be rejected by the outer limit** (which shows up as a 413 rather than a business error). Exact computation is not used because it can only be known by "parsing the multipart", and by then the request body has already been read and the limit has lost its meaning. To be exact, read `Content-Length` before the route and decide in two stages based on "the file size + a fixed envelope estimate".

---

## 6. Change list (by crate / file)

> **Implementation status (through Phase 6 Desktop chat UI)**: the items in this section marked **(implemented)** have been merged and pass tests — core `src/account.rs`; gateway `src/account/store.rs` / `password.rs` / `auth/{mode,token,revoked,service}.rs` / `http/{auth_middleware,auth_api,account_api,restricted_mode}.rs` / `chat.rs` / `http/chat_api.rs` / config `auth_mode` + `[multi_user]` + `effective_auth_mode()` / cli `--auth-mode` / the **passwordless seed + restricted mode** of `Gateway::new` (§ Decision 12 v2, superseding the original bootstrap_admin fail-fast) / the 14 session control routes of `proxy.rs` (7 lifecycle + 7 session actions); runtime `src/http/session_control.rs` + the scope / visibility of `conversation.rs` + the `create_frontend_session` / `resume_session` of `agent/session/session_manager.rs`; Desktop `authStore` / `authFetch` / `account/*` / `user-list/*` / `lib/user-chat-api.ts` / `stores/userChatStore.ts` / `views/MessagesView.tsx` / `components/account/SetupRequiredView.tsx`. The **not implemented** items = follow-up PRs (`chat/attachments`), which are in the planned scope, not an omission from this section. The original draft's `protocol.rs AccountPublicView` was **not added** — `acowork_core::account::AccountView` already serves as the redacted API return type, and adding a second of the same kind would be duplication.
### 6.1 core/acowork-core

- New `src/account.rs` **(implemented)**: `UserAccount`, `Role`, `AccountListFile`, `DISABLED_PASSWORD_HASH`; `UserAccount::to_public_profile()` produces the `UserProfile` public view
- Modified `src/protocol.rs`: `UserProfile` is retained (for display) — **`AccountPublicView` is not added**: `acowork_core::account::AccountView` is already the redacted API return type (shared by `/api/auth/me` + `account_api`), and the original draft's `AccountPublicView` duplicates it
- Modified the `Cargo.toml` dependencies (e.g. the jsonwebtoken crate if needed) — **not introduced**: the HS256 signature is a minimal in-house implementation (a fixed header + constant-time verification), with no dependency on jsonwebtoken, see § Decision 3

### 6.2 core/acowork-vault

- **Zero changes**: it reuses the `Vault::store` / `Vault::retrieve` interfaces, only adding callers (the account module)

### 6.3 core/acowork-runtime

> **Phase D status**: everything is **landed** (see the Phase D implementation record in § Decision 4).

- ✅ Modified `src/conversation.rs`: `SessionMeta` gains `user_id` + `visibility` (both `serde(default)` + `skip_serializing_if`, backward compatible); the `SessionScope` enum + `from_header_value`; the `is_readable_by` / `is_writable_by` predicates; `ConversationSession::set_user_id` (write-once) / `set_visibility` / `is_private`; `scan_sessions_async` gains a `scope` parameter and filters **before pagination**; `SessionInfo` gains `visibility`
- ✅ Modified `src/usecases/session_metadata.rs` + `session_metadata_impl.rs`: `list_sessions` gains a `scope: &SessionScope` parameter and passes it through (the original draft's `user_id: Option<&str>` + the `"__admin__"` sentinel are replaced by `SessionScope` — a sentinel is a magic string value, and one more variant cannot express "local does not filter")
- ✅ Modified `src/http/server.rs`: `/sessions` / `/sessions/{sid}` / `.../messages` / `.../latest` parse the scope from the `x-user-id` header and validate (not a `?user_id=` query — identity is carried by the Gateway-injected header and cannot be forged by a client); registers **14 control-plane routes** (7 lifecycle + 7 session actions); reverses the "the control plane is not on HTTP" comment of ADR-034 §11.2
- ✅ New `src/http/session_control.rs`: **14 handlers** (the first batch of 7 lifecycle: create / open / close / delete / visibility / workspace / config; the second batch of 7 session actions: messages / stop / continue / approval / answer / cancel-tool / compress) + the shared `dispatch_session_action` helper + `authorize_read` / `authorize_write` / `scope_from_headers` (shared by the read path of `server.rs`)
- ✅ Modified `src/agent/session/session_manager.rs`: `create_frontend_session` gains `user_id` / `visibility` parameters; a new `resume_session` (encapsulating the ADR-038 activation state machine, returning `Option<SessionOpenOutcome>`, where `None` = not found) — the original draft's `create_session_with_id_and_conversation` name was a guess, and the real entry point is `create_frontend_session`
- ✅ Modified `src/startup/gateway_loop.rs`: the MQTT `open_session` command now calls `resume_session` (the same state machine, no duplicate implementation)
- ✅ Modified `src/startup/session_init.rs` + `tests/conversation_session_tokens.rs`: the `SessionMeta` construction sites add `user_id: None, visibility: None`
- ✅ Modified `src/http/server.rs` (completing the read / write guards): `GET`/`PUT /sessions/{sid}/config` and `POST /sessions/{sid}/files` previously had **no scope check at all** (anyone could read and modify someone else's session model / workspace / title, and upload attachments to someone else's session), and now call `authorize_read` / `authorize_write`

### 6.4 core/acowork-gateway

**New**:
- `src/http/auth_middleware.rs` **(implemented)**: token validation + injecting `AuthContext { user_id, role, as_user }` + `effective_user_id()`; a local whitelist + `OPTIONS` allowed; a non-admin `as_user` → 403
- `src/http/auth_api.rs` **(implemented)**: `/api/auth/login` `/refresh` `/logout` `/change-password` `/first-login` `/me` (`/first-login` consumes the `invite_token`, landing with Phase E); all Argon2 calls go through `spawn_blocking`
- `src/auth/service.rs` **(implemented)**: `AuthService` — login / refresh (rotation + reuse detection) / logout / change password / `first_login` / account CRUD (`create_account` / `update_account` / `set_role` / `disable_account` / `reset_password`) / `verify_access` / `ensure_bootstrap_admin`; the definitions of `PasswordPolicy`, `BootstrapAdmin`, `AuthError` (including `Conflict`), `TokenPair`, `AuthPrincipal`, `ProfilePatch` (a display field patch, see the "settled" item in §5.5)
- `src/http/account_api.rs` **(implemented)**: account CRUD (`GET/POST /api/users`, `GET/PUT/DELETE /api/users/{id}`, `/disable`, `/reset-password`), which under multi_user supersedes the `/api/users` path of `users_api.rs` and re-projects `accounts.json` into `user_profiles.json`; `PUT /api/users/{id}` receives all the display fields through `UpdateAccountRequest` (`#[serde(flatten)] ProfilePatch`); plus `GET /api/users/directory` (readable by **any authenticated account** — the contacts directory, the only account list that is not admin-only, see §5.5)
- `src/http/chat_api.rs` **(implemented)**: the user-to-user chat API — `GET /api/users/{user_id}/chats` (ordered by `last_active_at` descending, including `peer_user_id` / `peer_display_name` / `unread_count` / the preview), `GET|POST .../chats/{chat_id}/messages` (`offset` counts from the tail, default 50 / max 200; the body must be non-empty after trim and ≤ 8000 characters), `POST .../chats/{chat_id}/read`. Reads = self-or-admin (the admin "view as" is a read-only scope); writes = **self-only** (even an admin cannot send on someone else's behalf, and `from` is forced to the token identity); a non-canonical `chat_id`, or a caller who is not a participant → 404 (not leaking existence). `peer_display_name` is resolved server-side (`display_name` → `username` → id) by reading the account table once for the whole page — a non-admin cannot read `/api/users`, so otherwise there would be no way to label the counterpart
- `src/account/store.rs` (implemented): reading and writing the authoritative account table `accounts.json` (atomic write: temp + rename); `src/account/password.rs` (implemented): Argon2id PHC hashing / verification
- `src/auth/token.rs` (implemented): HS256 token issuance / verification / refresh family management; the signing key `data_dir/auth/secret` (generated on first startup, `0600`)
- `src/auth/revoked.rs` (implemented): `revoked_families.txt` file management (`r:{family}` rotation / `x:{family}` explicit revocation / `{user_id}.*` wildcard, see the decision 3 implementation record)
- `src/chat.rs` (**implemented**): reading and writing `conversation.json` (`participants` / `last_active_at` / `last_message_preview` truncated to 80 characters / `unread` / `version`) + the append-only `messages.jsonl`. **A single-file module** — the original draft's `src/chat/{persistence,attachments}.rs` split was not adopted: the attachments ultimately land in this same file (sectioned by `// ── Attachments ──` without splitting the file), and splitting the read/write logic of one paired directory into two files is just directory noise. The paired directory `users/{min(a,b)}/chats/{max(a,b)}/` (the wire form `chat_id = min__max`; since an id is a UUIDv4, `__` never appears inside an id); `conversation.json` is written atomically (temp + rename); the JSONL is parsed line by line and **a single corrupt line only warns rather than invalidating**; unread is counted by `user_id` (not by the a/b positional order, to avoid count drift when the pairing order is recomputed). Known ceilings (already marked `ponytail:`): the list does a full directory scan, and tail pagination reads the whole JSONL
- The attachment submodule of `src/chat.rs` (**implemented**; the original draft's separate file `src/chat/attachments.rs` was not adopted): attachments land at `users/{min}/chats/{max}/files/{id}` (the blob, `{id}` = UUIDv4) + `{id}.json` in the same directory (metadata: filename / mime / size / kind), which is naturally isolated per conversation because it lives under the same directory tree as the conversation; the storage filename is sanitized by `safe_filename` (stripping path separators / control characters / quotes — `file_name` ends up in `Content-Disposition`) and truncated to 200 characters; the routes are `POST .../chats/{chat_id}/files` (multipart, **self-only write**) and `GET .../chats/{chat_id}/files/{attachment_id}` (self-or-admin read); the limit is tiered by the **client-declared mime at upload time** (`image/*` 25 MiB, otherwise 100 MiB, 413 when exceeded) — this is a **cap and not an allow-list**, and declaring an image is only stricter; the download `Content-Type` comes from the metadata written at storage time (not trusting the client-declared value), and `Content-Disposition` supports RFC 5987 UTF-8 (CJK filenames); the `attachments` field of `messages.jsonl` (an array of ids) is written by the 6th parameter of `append_message`
- `src/auth/mode.rs` **(implemented)**: `AUTH_MODE` inference + bind address parsing + CLI flag parsing (decision 12); `pub enum AuthMode { Local, MultiUser }`; `pub fn resolve_auth_mode(cli: Option<AuthMode>, toml: Option<AuthMode>, bind_host: &str) -> AuthMode`; a pub `is_loopback_host(host: &str) -> bool` helper (the loopback determination covers `127.0.0.0/8`, `::1`, `localhost`; `0.0.0.0` / `fe80::/10` / a LAN IP / a domain name → MultiUser on the safe side)

**Modified**:
- `src/http/routes.rs` **(implemented)**: `AppState` gains `auth_mode: AuthMode` + `auth_service: Option<Arc<AuthService>>` (**`Some` only under multi_user**); `/api/auth/*` is `merge`d only when `auth_service.is_some()` (under local mode it is **not registered**, returning 404 rather than 403); `auth_middleware` is mounted as a global layer inside CORS and outside the routes, and is a no-op when `auth_service == None`
- `src/http/server.rs` **(implemented)**: `start_http_server` gains the `auth_mode` + `auth_service` parameters and writes them into `AppState`
- `src/gateway/mod.rs` **(implemented)**: `Gateway::new` resolves `effective_auth_mode()`; under multi_user it constructs an `AuthService` and calls `ensure_bootstrap_admin()` (**fail-fast at constructor time**)
- `src/http/proxy.rs` **(implemented)**: adds 5 session control-plane proxy routes (`POST /sessions`, `POST .../{sid}/open`, `POST .../{sid}/close`, `DELETE .../{sid}`, `PUT .../{sid}/visibility`, with body / method pass-through). **Note: the proxy layer does no owner check and does not inject user_id** — `x-user-id` is uniformly stripped / injected by `auth_middleware` (a global layer), and the owner determination is done by the Runtime (see the division of duties in § Decision 4). The original draft's "the proxy layer adds `Extension(auth)`, injects a query and re-verifies the owner" is therefore **not adopted**: that would create a second source of truth in the Gateway and would also have to handle the race where "the meta was just deleted"
- `src/http/auth_middleware.rs` **(implemented)**: `as_user` is allowed only for an admin and **only for read-only methods** (GET / HEAD) — a write request carrying `as_user` is rejected with 403 (the enforcement point of §9 open question 5)
- `src/http/pm_proxy.rs` / `src/http/doc_proxy.rs` (decision 10, **implemented**): the `build_trusted_headers` signature gains an `actor: &str` parameter; the multi_user mode REST branch changes from injecting the constant `"human"` to `auth.effective_user_id` (resolved via the `trusted_rest_actor` helper from `Option<Extension<AuthContext>>`); the **local mode retains the `X-Actor: human` constant injection** (the decision 12 coupling); the MCP branch's `X-MCP-Actor` validation logic is unchanged (identical in both modes). Tests: `rest_path_injects_token_identity_under_multi_user` (multi_user + a token → injects `u-alice`, a forged `X-Actor` is dropped); the existing `rest_path_injects_human_when_absent` / `rest_path_overrides_forged_x_actor` (the local fallback) are unchanged
- `src/http/users_api.rs`: **retained as the local mode implementation**; the original draft's "deprecate + alias to `account_api`" was **not adopted** — `routes.rs` uses a `match &state.auth_service` to choose between `account_api::account_routes()` and `users_api::users_routes()` (the same set of paths cannot be registered twice, axum panics), which is one less indirection than an alias. Phase E already fixed its multi_user defects: ① the `/api/user/avatar-*` routes are registered mode-independently; previously they directly modified the shared active user in `user_profiles.json` (which `sync_profiles` would wipe out), and now under the multi_user branch they write through `accounts.json` by `AuthContext.user_id` (reusing the `blocking` / `sync_profiles` / `account_err` of `account_api`), leaving the local path untouched; ② the avatar files switch to the per-user namespace `assets/avatars/{user_id}/` (upload / list narrowed to the caller, the delete ownership guard before the unlink, GET readable pool-wide), while local retains the `assets/` root (see "Asset ownership" in §5.5)
- `src/resource_cache.rs`: `UserProfileListFile` is renamed or retained — `user_profiles.json` is retained as the public view; the existing `UserProfile` write path is maintained in local mode (no `password_hash` sentinel is introduced)
- `src/account/store.rs` (the companion to decision 2): **`accounts.json` is not created in local mode**, and `user_profiles.json` is not touched (zero changes)
- `src/bootstrap/orchestrator.rs`: **not adopted** — the startup check instead lands in `Gateway::new` (only a constructor-period `Result` can genuinely refuse to start; the orchestrator is a runtime subsystem, where an error would only log)
- `src/config.rs` **(implemented)**: a top-level `auth_mode: Option<AuthMode>` (`None` = auto-inferred from bind) + `effective_auth_mode()`; a new `[multi_user]` section: `bootstrap_admin: Option<BootstrapAdmin>`, `password_policy: PasswordPolicy`, `registration_open: bool` (**wired**, in Phase E: a non-admin can create an ordinary account, and never an admin)
- `src/cli.rs` (CliArgs): `--auth-mode <local|multi_user>` **(implemented)**, including the env `ACOWORK_GATEWAY_AUTH_MODE`; the "an explicit `--auth-mode` conflicts with `--bind` → warn" coupling — **implemented**: an explicit mode always wins, and `Gateway::new` emits a `warn` when `config.auth_mode.is_some()` disagrees with the bind inference (decision 12 allows that override, it only warns)

### 6.5 apps/acowork-desktop

**New**:
- `src/stores/authStore.ts`: token management + the account switching reset flow
- `src/components/account-switcher/`: the top-bar account menu + a login modal + a change-password modal + a registration modal (visible to admin)
- `src/components/user-list/UserList.tsx` + `partitionAccounts.ts`: the sidebar User collapsible group
- `src/components/chat/`: the user-to-user chat UI (the list / a conversation / attachment upload)
- `src/lib/api/auth.ts`: an HTTP client injecting the Authorization header

**Modified**:
- `src/components/agent-list/AgentList.tsx`: inserts `<UserList />` below the agent groups
- `src/stores/userProfileStore.ts`: **retained** (for display), with a new `authStore` as a higher layer (the auth state determines the current userProfile; the userProfile is a redacted copy)
- `src/App.tsx`: login-state determination; not logged in → render LoginView; after logging in → the existing main UI
- `src/lib/types.ts`: new `UserAccount`, `AuthState`, `Role` types
- `src/i18n/`: new `account.*` / `userList.*` / `chat.*` string keys

**Implemented (the Phase D wrap-up, the Desktop side of session isolation)**:
- New `src/lib/session-control.ts`: an HTTP wrapper for the session control plane (replacing `invoke("mqtt_publish_control")`)
- New `src/components/chat/SessionVisibilityToggle.tsx`: the per-session visibility switch in the input box tool row (🌐 / 🔒, clickable only by the owner)
- Modified `src/stores/agentStore.ts`: `setSessionVisibility` (an optimistic flip + PUT + rollback on failure)
- Modified `src/stores/chatStore.ts`: 8 control call sites MQTT → HTTP; `closeTab` skips `POST /close` when `can_write === false`
- Modified `src/components/chat/ChatPanel.tsx`: the input box is disabled when `can_write === false` + a read-only placeholder; the visibility switch is mounted
- Modified `src/lib/types.ts`: `SessionInfo` gains `visibility` / `can_write`

**Implemented (the Phase 2 remainder + Phase 5, this round)**:
- New `src/lib/auth-api.ts`: a thin HTTP wrapper for `/api/auth/*` + `/api/users` (login / refresh / logout / change password / me / the account list / soft delete), with errors unified as `AuthApiError(status, message)`
- New `src/lib/session/tokens.ts`: **the single owner of the token pair** (a follow-up refactor, fixing "kicked to the login screen ~13 minutes after every login"). The original implementation had the token lifecycle **fragmented** across four call sites — the `authStore` proactive timer, the 401 branch in `authStore.init()`, the 401 branch of the `authFetch` interceptor, and the `AUTH_REQUIRED_EVENT` branch of `gatewayAuthBridge` — each of them a token *consumer* that also performed a *management* action (calling `refreshTokens()` directly). Consequence: two refreshes 3 ms apart inside one webview were inevitable, the second was judged a replay by the server → cascading revocation → login screen → loop. The single-flight field (`_refreshPromise`) covered **only** the `refreshTokens()` entry point; concurrency across entry points was not serialized at all — the invariant was written into a data structure but not enforced by the architecture.
  - **Boundary**: the module owns persistence, `exp` bookkeeping, the proactive scheduled rotation, single-flight, and the verdict that a session is dead. A consumer has exactly two actions — `acquire()` (give me a token that is still usable) and `reportRejected(token)` (report the *fact* that this one was refused, get a usable one back). A consumer **never commands a rotation** and never receives a refresh token (`refreshToken` was removed from `authStore`).
  - **High cohesion / low coupling**: the module has zero store dependencies; the rotation transport is injected via `configure({ refresh, logout })` (wired at `authStore` module load). Importing `lib/config` directly would close a cycle (config → settingsStore → authStore → this module); injection breaks that edge and also gives the only testable seam — the unit tests need no store at all.
  - **Open/closed**: a new token consumer (mobile, a future interceptor) only calls `acquire()` and adds no rotation logic; a new rotation **policy** (grace length, lead time, backoff) changes module internals and no consumer.
  - **The proactive timer stays, and must**: in relay mode the MQTT CONNECT password *is* the access token, and a bad credential is dropped by `rumqttd` without a CONNACK, so the lazy 401 path is structurally unavailable to that consumer — it can only be served by a token that is fresh before it expires.
  - `authStore` loses ~150 lines; the four refresh entry points → **zero** (the only entry point is inside the module). Test migration: `authStore.proactive.test.ts` was deleted and its coverage (lead time, re-arming, the stall ceiling, unreadable `exp`) — plus the new single-flight collapse, `reportRejected` dedup, and the session-end signal — moved to `src/lib/session/tokens.test.ts` (22 cases, module-only, no store).
- New `src/lib/authFetch.ts`: a **global `fetch` interceptor** (replacing the original draft's per-call-site `src/lib/api/auth.ts` wrapper) — it injects `Authorization` only for the Gateway origin, skips `/api/auth/*`, and on 401 calls `reportRejected` → replaying once; under `AUTH_MODE=local` it passes through purely. The reason for the trade-off: Desktop has ~170 bare `fetch(` call sites, and wrapping them one by one makes it extremely easy to miss one (a miss = a silent 401 under multi_user), while the interceptor guarantees zero omissions
- New `src/stores/authStore.ts`: mode resolution (`auth_mode` from `/api/status`) + the login-gate state + account UI state; account switching / logging out / deregistering / changing a password all **clear the token + `window.location.reload()`** (the "logged out but not logged in" intermediate state of §9 question 6 lands on LoginView after the reload, reusing the repository's existing `location.reload` recovery pattern and replacing the original draft's per-store reset). **Tokens are not managed here** — the original `localStorage` read/write and the single-flight `refreshTokens` moved out to `lib/session/tokens.ts` (see the entry above); `accessToken` is a **read-only mirror** of the module's token, because a dozen-plus components bind to it and must re-render on rotation
- New `src/components/account/`: `LoginView` (the App gate) + `AccountMenu` (reusing the `common/ContextMenu` popover: switch account / change password / deregister / user preferences / log out) + `ChangePasswordModal`
- Modified `src/components/layout/NavBar.tsx`: the avatar entry uses `AccountMenu` (falling back to the old "edit profile" behaviour under local / when not logged in)
- Modified `src/App.tsx`: `installAuthFetchInterceptor` is mounted during startup in `main.tsx`; `logged_out` → LoginView, `unknown` (while the mode is being resolved) → an empty surface, anything else → AppLayout
- Modified `src/lib/types.ts`: `UserAccount` / `AccountListResponse` / `TokenPair` / `AuthMode` / `AuthState` / `Role`; `SystemStatusResponse.auth_mode`
- Modified `src/stores/agentStore.ts`: `fetchSessions` appends `as_user` according to `viewAsUserId`
- Modified `src/i18n/locales/*`: the `account.*` / `userList.*` keys (en / ja / ko / zh-CN / zh-TW)
- The Gateway's `src/http/routes.rs`: `GET /api/status` gains the `auth_mode` field (the mode probe entry point of decision 12; `/api/status` was already in the middleware whitelist)
- Not done (left for later): none — the three items listed in this block (the Desktop entry point for `registration_open`, the user-to-user chat UI, the attachment upload API + UI) are all settled in the Phase 6 / non-admin entry point blocks below

**Implemented (this round: the non-admin account creation entry point is settled, § Decision 6 / §9 question 9)**:
- The Gateway's `src/http/routes.rs`: `GET /api/status` gains `registration_open: bool`; the value = `auth_service.is_some() && config.multi_user.registration_open` — it is constantly `false` when the account system is not running, so the frontend can never render a button that is bound to return 403
- Modified `src/lib/auth-api.ts`: `fetchAuthMode()` → `fetchAuthPolicy()`, where a single `/api/status` probe returns both `{ authMode, registrationOpen }` (the original function had only one call site, so renaming is cheaper than probing one more endpoint)
- Modified `src/stores/authStore.ts`: the new `registrationOpen` (persisted in `init()` together with the mode)
- Modified `src/components/user-list/UserList.tsx`: `canInvite = isAdmin || registrationOpen` decides the "+" at the top of the group; a non-admin's `onCreated` does **not** `reload()` (`GET /api/users` is admin-only, so a non-admin would only get a 403 and light up a "load failed" banner; their group only lists themselves anyway, so there is nothing to refresh)
- Tests: a new `src/components/user-list/UserList.registration.test.tsx` (3 cases: a non-admin + open → there is a button; a non-admin + closed → no button; admin → always). `cargo test -p acowork-gateway --lib test_system_status` → 2 passed (the local mode asserts `!registration_open`); the whole Desktop suite is 803 passed (the only two failures are the pre-existing `formatTime` timezone / `DocRichEditor` tiptap module cases)

**Implemented (Phase 6, the Desktop chat UI, this round)**:
- New `src/views/MessagesView.tsx`: the user chat view (a conversation list on the left + a conversation thread on the right + an input box); `Enter` sends / `Shift+Enter` inserts a newline; a send failure puts the draft **back** into the input box (never swallowing user input); the conversation title prefers `peer_display_name` from `GET /chats`, a newly created conversation (with no list row yet) falls back to the label passed in from `UserList`, and a bare id is the last resort
- New `src/stores/userChatStore.ts`: `chats` / `activePeerId` / `messages` + `send` (**using the Gateway's echo**, never minting `ts` / `from` locally) / `openChat` (loads the thread only, and **no longer** posts the read receipt) / `markThreadRead` (called by **whichever component renders the thread**; de-duplicated on "newest message ts", so a 5 s poll does not become a POST every 5 s) / `reset`; the poll is a **single interval owned by the store itself**, driven by an `authStore` subscription (sign-in starts it, sign-out stops it) and **mounted on no component**
- **Amendment (poll host, 2026-10)**: the bullet above originally read `startPolling` (reference-counted, invoked by "whichever inbox view is mounted"). When the full-screen `MessagesView` became the conditional `InboxPanel` pane inside the chat layout, the `useEffect` moved along with the JSX and the host's lifetime silently changed from "while signed in" to "while `currentView === "chat"` **and** `selectedAgentId === null`" — selecting any agent session stopped polling entirely, so a message from another device never arrived (and `unread` therefore stayed set on the server). Reference counting is blind to **zero** hosts, so nothing surfaced the regression. **Conclusion: the lifecycle of a data-sync loop must not live in the render tree**, because the render tree is exactly what refactors move; the store now subscribes to session state itself. The read receipt deliberately **stays** with the renderer (only it knows "the user saw this"). Splitting the two is intentional: if that signal is ever orphaned again, the cost is a lagging badge, never silent data loss. Tests: `src/stores/userChatStore.test.ts` ("polls with zero components mounted and dies with the session" / "opens a thread and loads it WITHOUT acking")
- New `src/lib/user-chat-api.ts`: a thin wrapper over the 4 routes. **It deliberately does not pass a token** — these paths are not under `/api/auth/*`, the global interceptor injects it and refreshes on 401, and copying the token flow here again would only be weaker; `auth-api.ts`'s `readError` is changed to be exported for reuse (the `{error}` / `{detail}` decoding lives in only one place)
- New `src/components/common/MessagesIcon.tsx`: the outline / filled variants of the nav icon. **It deliberately does not reuse `ChatIcon`**: the two nav targets share the same 40px vertical bar, and at 24px they must be distinguishable
- A new nav unread red dot, driven by the unread total of `userChatStore`. The dot lives on the permanent `NavBar` (a `badge` prop on `NavButton`; on a 40 px tile it shows a dot, not the number); the poll is driven by session state rather than mounted by `NavBar` (see the amendment above) — so a message arriving while the user is looking at an agent conversation, or sitting on any other view, is still visible. Under `local` the dot simply never appears (`chats` stays empty; the route does not exist, avoiding "show it and then 404")
- Modified `src/stores/layoutStore.ts` + `src/components/layout/AppLayout.tsx`: a new `requestNavView(view)` seq contract (following the consume-once paradigm of `workspaceSearchFocusSeq`). `currentView` is `AppLayout`'s private state, so the only channel for the sidebar to "jump to the inbox" is this one
- Modified `src/components/user-list/UserList.tsx`: the right-click menu gains "send a message to this user" (not shown for yourself or for disabled accounts), which opens the thread and switches to the `users` view
- Modified `src/lib/types.ts`: `NavView` gains `"users"`; new `UserChatSummary` / `UserChatMessage` / `UserChatMessagesPage` / `UserDirectoryEntry` (aligned with `ChatSummary` / `ChatMessage`)
- A new "new conversation" selector: the `MessagesView` left column header renders a contact dropdown from `GET /api/users/directory` (when the endpoint fails or there is nobody else, nothing is rendered — the inbox degrades but **replying still works**); `user-chat-api.ts` gains `listUserDirectory` / `contactLabel`. The admin's `UserList` right-click "send a message" is retained as a shortcut and is no longer the only entry point
- Modified `src/i18n/locales/*`: `navBar.users` + `messages.*` (11 keys × 5 locales, verified key by key with a script)
- The attachment UI: a paperclip in the composer + a pending attachment strip (individually removable, and a **failed upload is not enqueued**) + image thumbnails / file bars inside bubbles (click to download) + an `attachmentObjectUrl` blob cache. **Images go through `fetch` + `createObjectURL` rather than `<img src="/api/...">`** — the download route requires a Bearer token, the global interceptor only intercepts `fetch`, and stuffing the token into the URL would be even worse
- Not done (the Phase 6 remainder): none; letting an ordinary user initiate a conversation is closed out (see §5.5; the residual ceiling = the directory is fully enumerable)

**Implemented (Phase 5 admin account management UI + the Phase 6 backend, this round)**:
- New `src/components/account/CreateAccountModal.tsx`: admin account creation (username / display_name / an optional password; leaving it blank → returns an `invite_token` to activate via first login)
- New `src/components/account/InviteTokenModal.tsx`: the `invite_token` display + copy (24 h expiry, burned on use). Account creation and password reset share this modal
- Modified `src/components/user-list/UserList.tsx`: the admin right-click menu is wired up (disable → a confirmation modal / reset password → `InviteTokenModal` / view as that user → `setViewAsUser`); the "+" at the top of the group opens `CreateAccountModal`
- Modified `src/lib/auth-api.ts` / `src/stores/authStore.ts`: `createAccount` / `disableAccount` / `resetPassword` / `deleteAccount` + an `accounts` cache
- Modified `src/i18n/locales/*`: the new `account.*` / `userList.*` keys of this round are complete in all 5 locales (verified key by key with `jq`)
- The Gateway: a new `src/chat.rs` (including attachment storage: `store_attachment` / `load_attachment` / `content_disposition` / the entry normalization of mime and filename) + `src/http/chat_api.rs` + `AuthService::data_dir()`; `routes.rs` adds `ApiError::payload_too_large` (413); `routes.rs` merges `chat_api::chat_routes()` inside the same `auth_service.is_some()` branch (the auth-mode redline scan of `dev/ci.sh` passes)
- The Gateway: a new `GET /api/users/directory` in `src/http/account_api.rs` (readable by **any authenticated account** — the contacts directory, §5.5) + `UserDirectoryResponse` / `DirectoryEntry`; `routes.rs` registers it **before** `/api/users/{user_id}` (axum 0.8 prefers a literal, and writing it first spares the next reader from having to know that rule)
- Tests: the Gateway adds **24**. `chat::tests` 16 = pairing normalization / unread semantics / both-sides visibility / tail pagination / a bad line being skipped / preview truncation / a self-conversation being rejected / a non-participant being denied reads + 8 attachment cases (participant narrowing / **borrowing across conversations is refused** / the metadata being authoritative / an injected mime falling back / the path being untraversable / the limit being tiered by mime / a CJK filename landing in `filename*` / a message with no body falling back to an attachment-name preview); `http::chat_api::tests` 7 = send/receive + unread + the admin read-only view / a third party and admin being unable to overstep / an empty body and a missing token / **the full attachment upload→send→download chain** / **the attachment read/write permission boundary** / **the limit and the empty-attachment 413·422** / **the upload route breaking through the 64 MiB global body limit**; `http::account_api::tests` 1 = the directory is readable by an ordinary user and is bounded. `cargo clippy --all-targets -D warnings` is clean, `cargo test -p acowork-gateway --lib` is **596 passed**; the Desktop's `tsc --noEmit` is error-free on the changed files, and `authStore` / `authFetch` / `partitionAccounts` / `userChatStore` are **48 passed**

### 6.6 The new ceiling lint in dev/ci.sh (**implemented**)

```bash
# ADR-076 § Decision 4: there is exactly one trusted writer of the identity (auth_middleware).
# The USER_SCOPE_HEADER constant and the "x-user-id" literal should only appear at their
# definition site + in tests (both of which are in auth_middleware.rs).
run_gateway_auth_scope_redline() { ... }   # greps (USER_SCOPE_HEADER|"x-user-id"), excluding auth_middleware.rs

# ADR-076 § Decision 12: the account APIs are only registered under multi_user — they must be
# inside the auth_mode branch.
run_gateway_auth_mode_redline() { ... }    # awk: the auth_routes()/account_api registration lines
                                           # must have auth_service/auth_mode within the preceding 4 lines
```

Both are hooked into the `check` and `all` modes of `dev/ci.sh`. The first prevents the identity injection from being bypassed (the proxy layer injecting `x-user-id` on its own); the second prevents a later PR from turning the multi_user routes into unconditional registrations.

> **Why there is no lint for "does the proxy inject user_id"**: that original lint was based on the "the Gateway injects at proxy time + verifies the owner" design, whereas the implementation places the owner determination in the Runtime (the division of duties in § Decision 4). The Gateway proxy layer **should not** see `user_id` at all — the lint should instead assert that it **does not** appear there.

### 6.7 core/acowork-pm (the coupling of decisions 10 + 11)

> **Implementation status (this implementation, decision 11 landed)**:
> - `src/types.rs`: the new `MemberKind { Agent, User }` (`#[serde(default)] = Agent`) and `ProjectMember.kind`; `MemberKind::from_actor()` infers from the actor value (`human`/`unknown` → `User`, otherwise → `Agent`) — **implemented**
> - `src/store/tree.rs`: `PmStore` gains `create_project_as` / `create_task_as` (an explicit `kind`; a bare UUID cannot be distinguished by value as a user_id vs. an instance_id, so the entry point must declare it explicitly); `create_project` / `create_task` are retained as convenience wrappers (internally inferring via `from_actor`, for tests and legacy paths); the self-bootstrapping of `create_project_as` changes so that both User and Agent creators **do** join members carrying `kind`; the `review_status` determination of `create_task_as` changes from `== "human"` to by `kind` — **implemented**
> - `src/api/projects.rs` / `src/api/tasks.rs`: the REST side (the human operation side) explicitly passes `MemberKind::User` — **implemented**
> - `src/mcp/tools.rs`: the MCP side (the agent tool side) explicitly passes `MemberKind::Agent`; the member projection of `pm_get_project` gains a `kind` field, and User members do not query the agent directory; `creator_is_user()` makes `created_by_meta` of a User creator short-circuit to `null` — **implemented**
> - `src/mcp/agent_dir.rs`: member validation continues to use the `agent_exists` fallback; User members get no MCP validation — **unchanged (unnecessary)**
> - `apps/acowork-desktop/src/lib/pm-types.ts`: `PmProjectMember` gains `kind?: "agent" | "user"` — **implemented**
> - Tests: `creator_is_auto_member_for_agent_and_human` in `tree.rs` (the original "a human-created project has empty members" assertion becomes "a human creator joins members with kind=User"), the new `explicit_kind_drives_review_status_and_member_kind` (a user_id that looks like a UUID is not misjudged as an Agent); the counts of `member_add_remove_roundtrip` / `remove_member_with_open_tasks_conflicts` are adjusted for "the creator joins members" — **implemented**

---

## 7. Test Strategy

### 7.1 Unit tests

- `core/acowork-gateway/src/account/`: account creation / password change / deregistration round-trip; the metadata is visible while the Vault is locked
- `core/acowork-gateway/src/auth/token.rs`: HS256 issuance / verification; refresh family rotation; a tampered token is rejected
- `core/acowork-gateway/src/auth/revoked.rs`: after revoking a family the old refresh_token is invalid; `r:` rotation and `x:` explicit revocation are distinguishable
- `core/acowork-gateway/src/auth/service.rs` (**implemented, 16 items**): a successful login / case-insensitivity / a wrong password / an unknown user / `$disabled$` / `disabled_at` all 401 and are indistinguishable; access and refresh are not interchangeable; access expiry; a tampered signature; refresh rotation + reuse detection taking down the family; logout killing only this device without collateral damage; a password change requiring the old password and killing all families; the signing key reused across restarts; bootstrap requires an empty table / is ignored after being configured once / password policy validation; the display field patch (`update_account_applies_the_display_patch`: `None` = unchanged / an empty avatar clears / a blank display_name is ignored / persistence agrees)
- `core/acowork-gateway/src/http/auth_api.rs` (**implemented, 4 items, going through the real `build_router`**): in local mode `/api/auth/login` → 404 (not registered); in multi_user accessing `/api/agents` without a token → 401 while `/health` is allowed; login → `/me` is redacted (no `password_hash`) → a non-admin `as_user` → 403 → refresh rotation → after a password change the old refresh is invalid; a bootstrap admin can log in
- ✅ `core/acowork-runtime/src/conversation.rs`: `SessionMeta` serialization with / without `user_id` is compatible (an old jsonl without the field still loads); `set_user_id` is write-once
- ✅ `core/acowork-gateway/src/http/auth_middleware.rs` (**implemented, 1 item, going through the real `build_router` layers**): a client-forged `x-user-id` is stripped; no token → 401; an admin gets `*`; `as_user` narrows the scope; an ill-formed `as_user` → 403
- ✅ `core/acowork-runtime/src/conversation.rs` (**implemented**): the three states of `SessionScope::from_header_value` (`*` → `Unfiltered`, a concrete id → `User`, empty/absent → `Unfiltered`); the determination of `is_readable_by` / `is_writable_by` over the combination matrix of `(user_id ownership × the three visibility states × scope)`; `visibility` defaulting to public + backward compatibility
- ✅ `core/acowork-runtime/src/http/session_control.rs` (**implemented**): `only_an_admin_may_re_share_an_unowned_session` — the truth table of `may_change_visibility` (the owner can / an ordinary account cannot on an unowned session / admin can), corresponding to the 403 that `PUT .../visibility` returns for an unowned session
- ✅ `apps/acowork-desktop/src/lib/agent-start.test.ts` (**implemented**): `opens the caller's newest readable session instead of retrying` (`/latest-session` 404 while they do have sessions → opens their own newest one, **retrying not once** and not creating a new one) + `creates a session when the account genuinely has none` (both sources empty → creates one of their own)
- ✅ `apps/acowork-desktop/src/components/user-list/UserList.registration.test.tsx` (**implemented, 3 items**): a non-admin + `registration_open` → the "+" at the top of the group is visible; a non-admin + closed → not visible; admin → always visible
- ✅ `core/acowork-runtime/src/agent/session/session_manager.rs` (**implemented**): `owned_sessions_start_private_and_ownerless_stay_public` — an owned session is written to disk as `Private` at creation (asserting the **on-disk meta**, because the listing / authorization read the file, so a value that exists only in memory is a bug), another account cannot read it while the owner and admin can; an unowned session still writes `None` (local mode and pre-upgrade data are unaffected)
- ✅ `core/acowork-runtime/src/conversation.rs` (**implemented**): `visibility_and_ownership_gate_read_and_write` (a private session's non-owner is refused both reads and writes; a public session's non-owner is readable but not writable; **unowned + Private → an ordinary account is refused both reads and writes**, unowned + `None`/`Public` → readable and writable), `session_scope_from_header_value` (the three states of `*` / a concrete id / absent), `scan_filters_by_scope_before_paginating` (the filter precedes the pagination, and `total_count` reflects the visible row count), `session_meta_visibility_is_absent_by_default_and_means_public`, `set_visibility_persists_and_clears_back_to_public`
- ✅ Anything unreadable / unwritable is **404 rather than 403** (the one shared mapping `session_control::not_found`): the handler wiring is covered by the existing HTTP server tests — `test_session_config_unknown_session_and_visibility_gate`, `test_http_upload_file_docx_lands_with_real_extension` (uploading requires the session to exist first)
- ✅ `core/acowork-runtime/src/usecases/session_metadata_impl.rs`: `list_sessions(page, size, scope)` only returns the sessions visible to the scope, **filtering before pagination** (`total_count` reflects the visible row count)

### 7.2 Integration tests (e2e)

> The **deployment side** of item 1 (refusing to start / the mode / what lands on disk) has been verified at the **process level** in §7.5, while the **data side** (session filtering) remains a cross-process e2e remainder. The **unit tests** of items 2 and 3 (password change / deregistration flows) are covered by `account_api::tests` (the invite lifecycle / non-admin self-containment / conflict rejection / the `registration_open` gate / display field persistence and avatar write-through — `update_account_persists_display_fields`, `avatar_config_writes_through_accounts_under_multi_user`) — but the process-level verification of account CRUD and password change is still to be done (§7.5 only covers the deployment mode). Item 4 (user chat) **has landed and is covered**.

- ◐ The full flow: admin creates → alice is created → alice creates a session → bob cannot see alice's session → an admin sees it with `?as_user=alice` (the scope resolution / filtering / owner check already have unit tests, and the "account creation → login → unauthorized access refused" stretch also already has a router-level integration test `auth_api::tests`; **only the "cross-process" stretch remains** — it needs Gateway + Node + Runtime all running)
- The password change flow: alice changes her password → the old refresh_token is invalid → a fresh login is required
- The deregistration flow: alice DELETE self → alice can no longer log in → her historical sessions remain readable by admin
- ✅ User chat: A → B sends text + an image + a document → B receives it → the unread_count increases → it zeroes after B reads it (**implemented**: `chat_api::tests::send_read_and_list_across_two_accounts` + `attachment_round_trips_from_upload_to_download`, going through the real `build_router`)
- The Vault-locked flow: after the Vault is locked alice can still log in (the `password_hash` is in the plaintext `accounts.json`), but an admin gets an error trying to read `vault/accounts/*.enc`

### 7.3 Protocol compatibility

- `user_profiles.json` retains compatibility: the existing `UserProfile` fields are untouched, and new fields (such as `username` / `role`) are optional

### 7.4 Security tests (a manual checklist)

- [x] An ordinary user accessing with `?as_user=<admin>` → 403 (unit tested)
- [x] An admin using `?as_user` to call a **write** method (POST / DELETE) → 403 (unit tested; the enforcement point is `auth_middleware`, see §6.4)
- [x] An ordinary user changing the `X-User-Id` header → **the Gateway middleware strips it outright** and injects the value derived from the token (under local mode it only strips and does not inject), so the client value can never reach the Runtime (unit tested)
- [x] A non-owner reading a private session → 404 (not 403, not leaking existence); a non-owner writing a public session → 404 (public ≠ modifiable) (unit tested)
- [x] The `visibility` switch has the same effect on **config** (going through the same `authorize_read`): `test_session_config_unknown_session_and_visibility_gate` asserts that when private a non-owner reading config → 404, that after the same owner flips it to public a non-owner reads → 200, and that public does not open up writes (PUT → 404)
- [x] An old refresh_token after deregistering the account → `disable_account` / `reset_password` call `revoke_user` → rejected (the account_api unit test asserts that login fails after deregistration; the `AuthService` unit tests cover a `revoke_user` hit)
- [x] An admin using `as_user` to call POST (a write operation) → 403 (the `auth_middleware` enforcement point, unit tested)
- [x] Cross-user chat paths: `POST /api/users/{A}/chats/{B}/messages` sends as A, and taking the `from` field from the token → from=B is rejected (**implemented**: `chat_api::tests::third_party_and_admin_cannot_write_as_someone_else` — neither a third party nor admin can write on someone else's behalf; the self-only write of `upload_attachment` / the self-or-admin read of `download_attachment` are covered by `attachment_writes_are_self_only_and_reads_are_scoped`)
- [ ] **Not covered (a known gap, see §10.2 item 14: brute-force protection for the login endpoint)** — `POST /api/auth/login` has no rate limiting and no account lockout (consecutive failures do not lock). The only mitigation today is the **deployment side**: `multi_user` is inferred from the bind address, and the default deployment is the `127.0.0.1` loopback; password strength (`password_policy`) is the only remaining online defence. **This must be added before exposing the Gateway to an untrusted network** — it is a "defence on the trust boundary", not a ceiling that can be left blank.

### 7.5 Deployment-mode tests (decision 12)

**Unit tests** (`core/acowork-gateway/src/auth/mode.rs`):

- The `resolve_auth_mode` truth table:
  - `bind = 127.0.0.1:19876`, no explicit flag → `Local`
  - `bind = 0.0.0.0:19876`, no explicit flag → `MultiUser`
  - `bind = 192.168.1.20:19876` → `MultiUser`
  - `bind = [::1]:19876` → `Local`
  - `bind = [fe80::1]:19876` → `MultiUser` (link-local defaults to the safe side)
  - an explicit `--auth-mode local` + `bind = 0.0.0.0` → `Local` (an explicit override of bind)
  - an explicit `--auth-mode multi_user` + `bind = 127.0.0.1` → `MultiUser` (an explicit override of bind, warn only)
- The priority chain: CLI > TOML > bind inference > default

**Integration tests** (`core/acowork-gateway/tests/auth_mode_e2e.rs`, **implemented, 4 cases**):

> They really launch the binary (`target/debug/acowork-gateway --daemon --home <tmp> …`, on a private port with a private `--home`), and only assert the things that are "only visible at the process level": the exit code, stderr, whether the port is connectable, and what `--home` puts on disk. **No HTTP client is introduced** — route registration itself is already covered by the in-process tests of `build_router`, and adding a client would just verify the same thing twice.

- ✅ **multi_user refuses to start**: an empty account table with no `bootstrap_admin` → a non-zero exit code, stderr containing `bootstrap_admin` and `multi_user`, and **no** `accounts.json` left behind (`multi_user_without_bootstrap_admin_refuses_to_start`)
- ✅ **multi_user starts normally**: reading `auth_mode` + `[multi_user.bootstrap_admin]` from the **TOML** (rather than a CLI flag) → the port is connectable; `accounts.json` lands on disk, contains `root`, and its plaintext does **not** contain the bootstrap password (`multi_user_from_config_bootstraps_an_admin_and_serves`)
- ✅ **local mode starts**: bind `127.0.0.1` with no flag → it starts normally; `accounts.json` **does not exist**; `data_dir/auth/` **does not exist** (`local_mode_leaves_no_account_state_behind`); `user_profiles.json` keeps the current state (no shape upgrade, no sentinel written)
- ✅ **an explicit local overrides a public bind**: `--addr 0.0.0.0:<port> --auth-mode local` → it is still local and creates no account state at all (`an_explicit_local_mode_overrides_a_public_bind`). This is the only combination that leaves the service running bare, and it must be nailed down at the process level
- ◐ **Session filtering for local / multi_user**: the criteria (`SessionScope`'s three states / `is_readable_by` / filtering before pagination) already have unit tests (§7.1); the **cross-process** e2e requires really starting a Runtime + Node and is still not done (see "still not covered" below)

**The alternative coverage that did land** (Phase C-2 + Phase E, going through the real `build_router`, see the `tests` of `core/acowork-gateway/src/http/auth_api.rs` and `account_api.rs`):
in local mode `/api/auth/login` → 404, `/api/users` goes through the display routes (`/reset-password` → 404); in multi_user `/api/agents` without a token → 401 while `/health` is allowed; login → `/me` is redacted → a non-admin `as_user` → 403 → refresh rotation → after a password change the old refresh is invalid; a bootstrap admin can log in; **Phase E**: the invite lifecycle is single-use (creating a passwordless account → first-login activation → a replay fails → the new password can log in), the invite expires after 24 h and an illegal timestamp fails closed, `reset-password` mints a new invite + clears the password + kills all refresh families, a non-admin can only read / delete themselves (list / disable / reset → 403), the `registration_open` switch (closed → a non-admin creating an account is 403; open → an ordinary account can be created but is **never** an admin), a duplicate name 409, and the last admin cannot be disabled (409). **Added this round**: `Gateway::new` refusing to start / starting fine / local leaving no trace / an explicit local overriding a public bind — the **real process-level** verification of all four is above (`tests/auth_mode_e2e.rs`). **Still not covered**: the cross-process e2e of session filtering — it needs Gateway + Node + Runtime simultaneously (only then does `?as_user=` reaching the Runtime's `authorize_read` mean anything), and the cost is a multi-process harness; the criteria logic itself is already exhaustively unit tested in §7.1 (the three states × ownership × the visibility matrix).

**Regression protection** (a new ceiling lint in `dev/ci.sh`, **implemented**): what actually landed is **three** redline functions — this round adds `run_gateway_chat_path_redline` (§ Decision 8: `.join("users"|"chats"|"files"|"conversation.json"|"messages.jsonl")` is only allowed in `src/chat.rs`, preventing a second place from deriving the `min__max` paired path on its own — the first consequence of getting the ordering wrong is reading someone else's messages); the other two are `run_gateway_auth_scope_redline` (`x-user-id` can only appear in `auth_middleware.rs`, preventing the proxy layer from injecting the identity on its own) and `run_gateway_auth_mode_redline` (the registration of `auth_api::auth_routes` / `account_api::` must be guarded by `auth_mode` / `auth_service`), all registered in the `check` / `all` modes with their negative cases verified. The original draft's `accounts.json` split lint / session route inventory lint were not landed separately — the former is covered by `account_api` being `merge`d only under multi_user (`routes.rs`'s `match &state.auth_service`), and the latter by the Runtime-side handlers carrying `authorize_read` themselves.

```bash
# Decision 12: under local mode the auth/admin routes must not be registered — preventing a later PR
# from accidentally turning the multi_user routes into unconditional registrations
grep -rn "auth_middleware\|account_api::router" core/acowork-gateway/src/http/routes.rs \
  | grep -v "auth_mode\|AuthMode" && echo "FAIL: route registration is not split by auth_mode" && exit 1

# Decision 12: under local mode `accounts.json` must not be created
grep -rn "save_accounts\|account_list_path" core/acowork-gateway/src/ | grep -v "auth_mode\|auth::mode\|AuthMode" \
  && echo "FAIL: accounts.json writes are not split by auth_mode" && exit 1

# Decision 4: the session routes must be listed one by one — adding a new route forces a manual
# confirmation of the validation path.
# Expected: 8 lines, and each line's handler is either inside session_control:: (with the check built in)
#        or calls authorize_read itself (get_session / get_messages / /latest)
grep -nE '"/sessions' core/acowork-runtime/src/http/server.rs
```

### 7.6 The verification results after the Phase D data side landed

- `cargo test`: core **216** / gateway **531** / runtime **1486** all pass (including scope / visibility / `session_control` / the `as_user` read-only guard / `can_write` delivery / the MQTT write-path refusal boundary tests)
- `cargo clippy -p acowork-core -p acowork-gateway -p acowork-runtime --all-targets -- -D warnings`: the three crates are clean
- Desktop: `chatStore.test.ts` **60 passed**; the full `vitest` suite is 695 passed / 1 failed; `tsc --noEmit` is error-free on the files I changed
- The 4 pre-existing failures are unrelated to this change: `git_api_e2e` 2 cases (git environment differences), the timezone skeleton assertion in `formatTime.test.ts`, and `DocRichEditor.test.tsx` (a missing `@tiptap/react` module); `doc_supervisor_integration` (missing the `acowork-doc` binary) and `acowork-embed` (missing the ONNX runtime) are out of the scope of this run

---

## 8. Implementation Milestones (proposed)

> **Progress (this implementation)**: the **backend of Phase 1-4 is complete and tested** — the `UserAccount` model + Argon2id + `accounts.json` + `/api/auth/*` (including `/first-login`) + the token middleware + bootstrap_admin fail-fast (Phase 1-2); `SessionMeta.user_id` + the `visibility` switch + Runtime scope filtering + owner validation + the **full migration of the session write path from MQTT to HTTP** (Phase 3, see the § Decision 4 Phase D implementation record) — the proto fields / Runtime variants / command-name mapping tables of **all user-operation commands on the MQTT side** (two batches, 16 in total: 8 lifecycle + 8 session actions) have been **deleted** and renumbered consecutively, and `can_write` is pushed down to the frontend to disable the write controls; **Phase 4 account CRUD** (`account_api.rs` + the `registration_open` wiring + the invite/first-login lifecycle) is complete. Phases 5-7 were untouched at the time — **this paragraph is a historical record, and everything in it has since landed, see the "Supplement" below; the sole authority for what remains in this ADR today is [§10 Remaining List](#10-remaining-list-the-full-set-of-what-has-not-been-done)** (the body no longer scatters ⬜ markers, to avoid two mutually contradictory status claims).
>
> **Supplement (this round)**: the leftovers of Phase 2 (the Desktop `authStore` + the global fetch interceptor + the `LoginView` gate + the top-bar account menu) and Phase 5 (the Sidebar User collapsible group + the admin account management UI + `?as_user=` filtering) have landed — see §6.5 "Implemented (the Phase 2 leftovers + Phase 5, this round)" and "Implemented (the Phase 5 admin account management UI + the Phase 6 backend, this round)". The **backend of Phase 6** (`src/chat.rs` persistence + `src/http/chat_api.rs`, see §6.4) and the **Desktop chat UI** (`MessagesView` + `userChatStore` + the nav unread dot + `requestNavView`, see §6.5 "Implemented (the Phase 6 Desktop chat UI, this round)") have both landed and are tested; the `GET /api/users/directory` that an **ordinary user needs to start a conversation** (this round's decision, §5.5) and the "new conversation" selector of `MessagesView` were completed together — § Decision 8 is closed apart from attachments. **Attachment upload/download** (§ Decision 9) has also landed; § Decisions 8 + 9 are thereby fully closed. **Phase 7 wrapped up this round**: the process-level e2e (`tests/auth_mode_e2e.rs`, 4 cases, see §7.5), the third ceiling lint (`run_gateway_chat_path_redline`, see the regression protection in §7.5), the user manual ([`docs/runbooks/multi-user-accounts.md`](../../runbooks/multi-user-accounts.md)), and the protocol documentation (`http.md` §4.14 / §4.15). **The sole remainder**: the cross-process e2e of session filtering (it needs a Gateway + Node + Runtime three-process harness; the criteria logic is already exhaustively unit tested in §7.1). **One more supplement (this round)**: the last "the backend is wired up but the frontend has no entry point" hole has been plugged — the Desktop entry point for `registration_open` (a non-admin sees "+" while the switch is open, see § Decision 6 / §9 question 9) — while two product decisions were settled as decisions (the directory enumeration stays as it is, §5.5; the upload quota rejects the per-user framework and is instead recorded as a global watermark, §5.5 / §9 question 10). **Closing**: everything still not done has been gathered into [§10 Remaining List](#10-remaining-list-the-full-set-of-what-has-not-been-done) (13 deliberately-left-blank items + 8 unimplemented + 2 test gaps + 7 rejected), each with its trigger condition and escalation path — so that nobody has to re-read 1000 lines to know what is left.

| Phase | Content | Estimate | Status |
|---|---|---|---|
| 1 | The `UserAccount` data model + the Argon2id password_hash + the Vault-encrypted extension fields | 1 week | ✅ model / Argon2id / store complete; the Vault-encrypted extension fields are not done (login with the Vault locked already works, the extension fields are non-blocking) |
| 2 | `/api/auth/*` + the token middleware + `authStore` + the top-bar account menu (login / change password / deregister) | 1 week | ✅ the backend is complete (`AuthService` + 5 routes + the middleware + bootstrap_admin); the Desktop `authStore` / account menu **is complete (this round)**: `authStore` + the global `fetch` interceptor (`authFetch`) + the `LoginView` gate + the top-bar `AccountMenu` + the change-password modal |
| 3 | `SessionMeta.user_id` + `visibility` + Runtime scope filtering + owner validation + HTTP-ising the control plane | 1.5 weeks (including the grep ceiling lint) | ✅ the schema (`user_id` write-once + `visibility`) + the Gateway header hygiene (stripping / injecting `x-user-id`) + the Runtime scope filtering (before pagination) + the read/write owner validation + the **14 HTTP control routes** (7 lifecycle including `PUT .../workspace`, 7 session actions) + `can_write` pushdown + the **deletion of all MQTT user-operation commands** (two batches, 16 in total; the proto fields are removed and renumbered consecutively) + the Desktop `session-control.ts` + disabling the write controls in the frontend. The grep ceiling lint still belongs to Phase 7 |
| 4 | The admin role + `as_user` + bootstrap_admin + creating the first admin | 0.5 week | ✅ the `as_user` validation + the read-only guard + the data plane; bootstrap_admin / the first admin; the account CRUD (`account_api.rs`) + `/api/auth/first-login` + the `registration_open` wiring are all complete (**this round adds the Desktop entry point**: `/api/status` exposes `registration_open`, and a non-admin also sees "+" while the switch is open, see § Decision 6 / §9 question 9) |
| 5 | The Sidebar User collapsible group (`partitionAccounts` + UserList.tsx) | 0.5 week | ✅ landed: `partitionAccounts` + `UserList.tsx` + the `AgentList` wiring + the admin `?as_user=` filtering (`viewAsUserId`) + the admin account creation (`CreateAccountModal`) / invite token (`InviteTokenModal`) / disable / password reset UI |
| 6 | User-to-user chat (persistence + API + the Desktop UI + attachment upload) | 2 weeks | ✅ **complete**: `src/chat.rs` (`users/{min}/chats/{max}/conversation.json` + `messages.jsonl`, atomic writes, tail pagination, a single bad line being skipped, unread counted by user_id) + `src/http/chat_api.rs` (4 routes; read = self-or-admin, write = self-only with `from` forced from the token; `peer_display_name` resolved server-side) + **attachment upload/download** (`files/{id}` + a sidecar + `Content-Disposition`/`nosniff` + the per-route body limit raised; see § Decision 9) + `MessagesView` / `userChatStore` (reference-counted polling + a pending-attachment queue) / `user-chat-api` / the nav unread dot / `requestNavView` + the **start-a-conversation closure** (`GET /api/users/directory` of `account_api` + the "new conversation" selector of `MessagesView`) |
| 7 | The ceiling lint + the integration tests + the documentation (README / user manual) | 1 week | ✅ the unit tests + the router integration tests are in place; the **process-level e2e** (`tests/auth_mode_e2e.rs`, 4 cases) landed — see §7.5; the **ceiling lint** landed the third one, `run_gateway_chat_path_redline` (the regression protection in §7.5); the **user manual**: [`docs/runbooks/multi-user-accounts.md`](../../runbooks/multi-user-accounts.md) (configuration / account creation / Desktop usage / on-disk backups / status codes / troubleshooting); the **protocol documentation**: `http.md` adds §4.14 Authentication and Accounts + §4.15 User-to-User Chat, `mqtt.md`'s command tree and ADR-034 §11.2.B were updated earlier to "the MQTT control plane is emptied + everything moves to HTTP + the field numbers are renumbered", and the `node_proto_golden.rs` golden was recomputed. **Remainder**: the cross-process e2e of session filtering (it needs a Gateway + Node + Runtime three-process harness, see §7.5) |

Total ~7.5 weeks. Suggested as three merged PRs: **PR1 = Phase 1-4** (accounts + isolation + admin), **PR2 = Phase 5-6** (the UI + chat), **PR3 = Phase 7** (the lint + the documentation).

---

## 9. Open Questions (the reviewers should focus on these)

> **Status update**: 1-7 have been decided along the mainstream technical lines (industry standard + the "if you're doing it, do the best version" principle), and are taken as design input into the decisions; question 8 has been decided in § Decision 11.
>
> **Mode-splitting supplement (§ Decision 12)**: under a **local** (bind `127.0.0.1`) deployment, the whole of the §1-§11 decision set **degenerates into a no-op** — question 1 (the first admin = the physical OS user), question 2 (the deregistration policy = OS account removal), questions 3-7 (isolation / chat / atomicity / concurrency = they do not trigger in the single-user scenario) are resolved by the `AUTH_MODE=local` split and need no separate decision. Only under multi_user mode (bind `0.0.0.0`) do questions 1-7 enter the implementation path.

1. **✅ Decided — the first-admin creation flow**: chose **`bootstrap_admin` configuration-driven** (unattended first). Reason: cluster systems such as Kubernetes / Consul / etcd / Docker all use a configuration file / environment variable; "the Gateway is a keep-alive process and should not have stdin" is an existing architectural principle (stated explicitly in `AGENTS.md`). The interactive scenario is provided by an **independent CLI subcommand** `acowork-gateway admin create` (a separate process, which does not break the keep-alive boundary), corresponding to the planned `apps/cli/` increment. When the configuration is missing: ~~an empty `accounts.json` → refuse to start (fail-fast, not a warning)~~ (**v2 revision**: an empty library now seeds a passwordless admin + a restricted mode, see § Decision 12 v2 / v3; it was originally "refuse to start"); non-empty → ignore (see the Decision 5 implementation revision).
2. **✅ Decided — the deregistration policy**: this phase implements **soft delete only** (`disabled_at`, consistent with Decision 6), and does not offer a hard delete. Reason: Slack / Discord / Teams all keep history by default with a soft delete; a hard delete is only needed in legally mandated scenarios such as GDPR (it would break the referential integrity of sessions / chat), and YAGNI applies at this phase; if a hard delete is needed later, raise an independent ADR (a grace period + an async cleanup job + a reference remapping policy).
3. **✅ Decided — group chat**: the `participants` field type of `conversation.json` stays `Vec<String>`, and this phase asserts `len() == 2` at runtime; a future group chat extends via "len() > 2 + group metadata (name / avatar / owner)" with **zero migration** (the schema is already compatible). Reason: Slack / Discord / WeChat / Telegram all adopt a unified DM / Group schema; this phase only exposes the 2-person path in the frontend while leaving a door in the schema.
4. **✅ Decided — the chat attachment size limit**: **images 25 MB / documents 100 MB / no virus scan introduced**. ponytail marker: the trade-off is known for the personal / small-team scenario; beyond that scale ClamAV (a separate process) + splitting out object storage (an independent ADR) is needed. Reason: Discord 25 MB (images / video) and Telegram 100 MB (any file) are the recognised sweet spot; the ROI of a virus scan is negative under 100 users — it is over-engineering.
5. **✅ Decided — does `as_user` need write operations**: this phase is a **read-only view** (consistent with Decision 4), and write operations keep the token's actual identity. Reason: it avoids the XSS / CSRF attack chain (Decision 4 already analysed this: identity impersonation = the entry point for horizontal privilege escalation). If the operational scenario needs "an admin sending a message to an agent on behalf of a user", extend it following the **OAuth 2.0 Token Exchange (RFC 8693)** pattern — a new `X-On-Behalf-Of` header + an `act` claim + a separate ACL policy, designed in an **independent ADR** so as not to pollute this phase's schema.
6. **✅ Decided — the atomicity of Desktop account switching**: keep the original ADR recommendation — **keep the "logged out but not logged in" intermediate state on failure**, and have the UI guide the user to log in again (the `LoginView` renders directly). Reason: VS Code / Google account switching both adopt this model; a transactional account switch is over-engineering, and rolling back on failure instead introduces new inconsistency risks (the old token is already revoked / the new token was not obtained / the MQTT state is half-connected — the three of them entangle).
7. **✅ Decided — multi-device concurrency**: this phase **allows multi-device concurrency** (an independent `token_family` per device) and does not introduce a `device_id` or a concurrency cap. Reason: Slack / Discord / Google / Microsoft all support multi-device concurrency by default; OAuth 2.0 RFC 6749 itself does not restrict it. If per-`device_id` + `max_sessions_per_user` + a "the new login kicks the old one out" policy is needed later, raise an independent ADR.
8. **~~The evolution of the PM `"human"` special case~~ ✅ Decided (option B)**: human operators become members — `ProjectMember` gains `kind` (`Agent`/`User`), humans and agent members are symmetric, and `assignee ∈ ∅ ∪ members` has no special case. The full design is in **§ Decision 11**. ~~Option A (relaxed to "any logged-in user may be assigned")~~ is rejected: semantically ambiguous (a task assigned by A can be claimed by B), and misaligned with the ADR-073 three-layer identity paradigm.
9. **✅ Decided — the Desktop entry point for non-admin account creation (settled this round)**: the semantics of `registration_open = true` are fixed as "**any logged-in account may invite a new account**", and on the Desktop side the new `registration_open` field of `/api/status` decides whether to render "+" in the `Users (N)` group. **Anonymous registration (`allow_public_signup`) is explicitly not done** — it would require moving `/api/users` out of the authentication middleware, which is a net expansion of the unauthenticated attack surface, and the target deployment of this ADR has no corresponding scenario. See § Decision 6.
10. **✅ Decided — the upload quota is not per-user** (settled this round): disk is a shared resource and a per-user cap cannot protect it; if one is truly wanted, put a **global watermark** on `data_dir`, with the trigger being real disk pressure or the introduction of mutually untrusted multi-tenancy. See §5.5.

---

## 10. Remaining List (the full set of what has not been done)

> **This section is an index, not a second description**: each entry only records "type + trigger condition + see-where", and the details stay in the section being pointed at — writing it in two places will eventually drift into two different stories.
>
> The boundary (the state covered as of this ADR's "closing this round"): the entries marked **settled** / **implemented** in §5.5 are **not** listed here (those are change records, not debt). The purpose of this section is so that the next person (or the next me) does not have to re-read 1000 lines to know "which holes are still there, and when do they need plugging".
>
> **The criterion**: a given row is only worth working on when its "trigger condition" holds. Working on it before that is writing code for a scenario that has no consumer.

### 10.1 Deliberately left blank (with a clear escalation path, not an oversight)

| # | Item | Trigger condition (when it really has to be done) | See |
|---|---|---|---|
| 1 | **No per-user subscription ACL on the MQTT event plane** — in theory a client that can connect to the broker can SUBSCRIBE to any `agents/{id}/sessions/{sid}/messages/#` | The deployment changes from "this machine's loopback" to "cross-machine / multi-tenant"; or an ACL capability is found in `rumqttd` | §5.5 + [`mqtt.md §10`](../../protocols/en/mqtt.md) (already marked "deferred / known gap") |
| 2 | **Access token validation is stateless** — after an account is disabled the old token is still usable, and the window = `ACCESS_TTL_SECS` (15 minutes) | More than 100 users, or a need to "kick someone offline immediately" | §5.5 (escalation path: an in-memory `user_id → revoked_at` set) |
| 3 | **✅ Closed (was: refresh has no grace window)** — a retry after a lost response was judged "reuse", revoking every family of that user; and `{user_id}.*` was unbounded while `login()` never clears the register → **locked into a loop** (login → 401 ~13 min later → login screen → login again). The trigger condition did arrive, but not as predicted: not multiple clients, rather two refreshes 3 ms apart inside one webview (fragmented token management colliding with itself) | — | §Decision 3 implementation record (grace window + `iat <= cutoff` time-bounded revocation) + §5.5 |
| 4 | **`revoked_families.txt` is a flat file with no GC** | Tens of thousands of refreshes over the lifetime | §5.5 (escalation path: a SQLite table + an expiry column) |
| 5 | **The chat list is a full filesystem scan (O(pairs))** | The number of conversation pairs approaches 1000 | §6.4 (a known ceiling of `chat.rs`) + the in-code `ponytail:` |
| 6 | **Pagination has to read the whole of `messages.jsonl`** | A single conversation has tens of thousands of messages | §6.4 (same) |
| 7 | **Attachment blobs have no reclamation** — a crash leaves orphaned files that nobody references | Real disk pressure appears | §5.5 (escalation path: scanning for "no sidecar and older than N days") |
| 8 | **A download reads the whole file into memory** (`Vec<u8>`) | Attachments greater than 100 MiB are needed | §5.5 (escalation path: `ReaderStream`, at the cost of one more dependency) |
| 9 | **The body cap of the upload route is an estimate of "100 MB + 1 MiB envelope"** — a file hugging the cap may be refused by the outer layer (a 413 rather than a business error) | Users start uploading 99-100 MB files | §5.5 (④) |
| 10 | **Upload quotas are not done** (the framework has been changed from per-user to a **global `data_dir` watermark**) | Real disk pressure appears, or mutually untrusted multi-tenancy is introduced | §5.5 + §9 question 10 |
| 11 | **`user_profiles.json` is still a global derived view** (`is_active` takes "the most recent login / the first admin"), and it only feeds the legacy `last_user_profile` theme | The Runtime needs per-owner profile push | § Decision 2 "the residual ceiling of the derived view" |
| 12 | **The user directory is fully enumerable to any authenticated account** (the complete set of `username`s) | A scenario of "strangers on the same instance" appears | §5.5 (escalation path: an exact-username query / returning only existing counterparts / an invite-only directory) |
| 13 | **Per-agent default visibility** (whether an admin can force a given agent's sessions to be visible to all accounts) | A real need for "this agent's conversation is a team-shared log" appears | § Decision 4 + §5.5 |

### 10.2 Unimplemented (explicitly left to later work / an independent ADR)

| # | Item | Trigger condition | See |
|---|---|---|---|
| 14 | **The login endpoint has no rate limiting / no account lockout** — online brute force can only be blocked by password strength (the deployment-side mitigation: only bind `127.0.0.1`) | It **must** be added before exposing to an untrusted network | §7.4 (already listed as a known gap) + §5.5 |
| 15 | **A multi-device concurrency cap / `device_id`** — this phase allows multi-device concurrency by "an independent family per device" | A need for "the new login kicks the old one out" | §9 question 7 |
| 16 | **Forced password expiry** (only `password_expires_at` is recorded, not enforced) | A compliance requirement | §5.5 |
| 17 | **Vault-encrypted extension fields** (`vault/accounts/{user_id}.enc`: `api_secrets` / `recovery_codes`) | A need for "each account having its own API key" appears | § Decision 2 + the Phase 1 row of §8 |
| 18 | **The `acowork-gateway admin create` CLI subcommand** (an interactive first admin; the unattended scenario already has `bootstrap_admin`) | Someone wants to create an account interactively in a terminal | §9 question 1 + the planned `apps/cli/` |
| 19 | **An admin writing on behalf of a user** (the `X-On-Behalf-Of` of OAuth 2.0 Token Exchange / RFC 8693) | A real flow of "operations sends a message on behalf of a user" appears | §9 question 5 |
| 20 | **Group chat** (`participants` already reserves `Vec<String>`, and the runtime asserts `len() == 2`) | A need for conversations of 3+ people (zero migration of the schema) | §9 question 3 + § Decision 8 |
| 21 | **Virus scanning** (attachments do not go through ClamAV) | Attachment sources are untrusted + the scale goes up | §9 question 4 |

### 10.3 Test gaps

| # | Item | Current state | See |
|---|---|---|---|
| 22 | **The cross-process e2e of session filtering** (Gateway + Node + Runtime, three processes) | The criteria logic is already exhaustively unit tested by the `(ownership × visibility × scope)` matrix; end to end there is only the single-process Gateway deployment-side e2e | §7.2 + §7.5 |
| 23 | **The process-level e2e of account CRUD / password change** | Already covered by unit tests (the invite lifecycle / a non-admin's self-containment / conflict rejection / the `registration_open` gate / display field persistence) | §7.2 |

### 10.4 Rejected (do not re-discuss during review unless the trigger conditions change)

| Option | Reason for rejection | See |
|---|---|---|
| **Anonymous registration** (`allow_public_signup`) | It requires moving `/api/users` out of the authentication middleware = a net expansion of the unauthenticated attack surface; the target deployment has no corresponding scenario | § Decision 6 |
| **A per-user upload quota** | Disk is a shared resource and a per-account cap cannot protect it (100 MiB × 50 people is only 5 GiB) | §5.5 + §9 question 10 |
| **Hard-deleting accounts** | It breaks the referential integrity of sessions / chat; mainstream implementations are all soft deletes | §9 question 2 |
| **Upgrading `user_profiles.json` into the account schema** | It would mix credentials into the semantics of "non-sensitive display metadata" (ADR-059 §7.3) | § Decision 1/2 |
| **A second source-of-truth index file** (persisting the session index) | A cached index only lives in the process's memory, and the authority is always the meta file | § Decision 2 + §5.5 |
| **`as_user` write operations** | Identity impersonation = the entry point for horizontal privilege escalation | §9 question 5 |
| **Opening up `open` read authorisation for public sessions** (activating spectators) | `Active` is a per-session global state, which would create sessions with "no owner in charge"; it is changed to "spectators do not activate" | §5.5 |
