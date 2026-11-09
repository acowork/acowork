# User Identity Management (Simplified Design)

> Version: v1.0 | Created: 2026-05-28 | Status: Design Phase

---

## 1. Design Background

The original design (`07-system-agent.md`, deleted) delegated user identity management to the system Agent (`com.acowork.system`), implementing identity management through the full chain `identity_deps` → `identity_delivery` → `identity_store`/`identity_query` tools. That approach introduced excessive complexity: the system Agent had to run, required a ContentProvider mechanism, needed dedicated built-in tools, and required cross-Agent Intent communication channels.

This design follows the **public resource management model** (used for model/MCP/search: version-driven diff sync + AgentHelloResult injection), treating user identity as a public resource managed by Gateway. Gateway centralizes management and persistence, pushes to Runtime at handshake, and hot-pushes on changes.

### Design Principles

1. **Single source of truth** — Gateway is the sole holder and distributor of user identity data
2. **Reference existing patterns** — Reuse `ResourceCache` + `AgentHelloResult` + HTTP API patterns to reduce cognitive load
3. **Multi-user ready** — Data model naturally supports multiple user identities; Gateway manages complete profiles for all historical users; currently only the active user is pushed to Runtime
4. **Simplified toolchain** — No longer need `identity_store`/`identity_query`/`identity_observe` tools; Runtime obtains identity context directly from the system prompt

---

## 2. Data Model

### 2.1 UserProfile

```rust
/// A single user's identity profile.
///
/// Persisted in `user_profiles.json` in Gateway's data directory.
/// Each profile is keyed by a UUID `user_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    /// Unique user identifier (UUID v4)
    pub user_id: String,

    /// Display name — what the user wants to be called
    pub display_name: String,

    /// Preferred language (BCP 47, e.g. "zh-CN", "en-US")
    pub language: String,

    /// Timezone (IANA, e.g. "Asia/Shanghai", "UTC")
    pub timezone: String,

    /// City (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,

    /// Country (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,

    /// Occupation / domain (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupation: Option<String>,

    /// Communication style preference (optional)
    /// e.g. "concise", "detailed", "casual"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub communication_style: Option<String>,

    /// Free-form extension fields (optional)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom: HashMap<String, String>,

    /// When this profile was created (ISO 8601)
    pub created_at: String,

    /// When this profile was last updated (ISO 8601)
    pub updated_at: String,

    /// Whether this user is currently the active / online user.
    /// Only persisted as true for the latest user; Runtime only receives
    /// active=true profiles.  In multi-user scenarios the Gateway may
    /// select a different active user via HTTP API.
    #[serde(default)]
    pub is_active: bool,
}
```

### 2.2 Versioned User List (UserListFile)

```rust
/// Versioned user profile list persisted to disk.
///
/// Follows the same pattern as ProviderListFile, McpListFile, SearchListFile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfileListFile {
    /// Monotonic version counter — bumped on every create/update/delete
    pub version: u64,
    /// All known user profiles (historical + current)
    pub users: Vec<UserProfile>,
}
```

**Persistence location:** `{data_dir}/user_profiles.json`

**Field Semantics:**

| Field | Source | Required | Description |
|-------|--------|----------|-------------|
| `display_name` | Onboarding / Settings | Yes | Address name; Agent learns via system prompt |
| `language` | Onboarding / Settings | Yes | Language preference, affects LLM system prompt language |
| `timezone` | Onboarding / Settings | Yes | Timezone, affects time-related replies |
| `city` | Onboarding / Settings | No | City |
| `country` | Onboarding / Settings | No | Country |
| `occupation` | Onboarding / Settings | No | Occupation/domain |
| `communication_style` | Settings | No | Communication preference |
| `custom` | Settings | No | Extension fields |

**Future Multi-user Extensions:**
- `user_profiles.json` stores complete profiles for all historical users
- `is_active` flags the current online user
- Gateway HTTP API supports user list viewing and switching
- Runtime only receives profiles where `is_active=true`

---

## 3. Resource Management Pattern (Reference: model/MCP)

### 3.1 Data Flow Overview

```
┌──────────────┐   HTTP API     ┌─────────────────┐   AgentHello     ┌─────────────┐
│ Desktop App  │ ──────────────→ │    Gateway      │ ───────────────→ │   Runtime   │
│              │ ←────────────── │                 │ ←─────────────── │             │
│ Onboarding   │  GET/PUT/POST   │ ResourceCache   │  AgentHelloResult │ Context     │
│ Settings     │  /api/users     │ user_list.json  │  user_identity    │ Builder     │
└──────────────┘                 └─────────────────┘                   └─────────────┘
        │                                │                                  │
        │                                │    Hot Push (after update)       │
        │                                │ ──────────────────────────────→ │
        │                                │    UserProfileUpdate             │
```

### 3.2 Resource Loading and Caching

At startup, Gateway loads the user list from `{data_dir}/user_profiles.json` into the in-memory `ResourceCache`:

```rust
impl ResourceCache {
    /// NEW: User profile list (versioned)
    pub user_profile_list: UserProfileListFile,
}
```

### 3.3 AgentHello Delivery

In `handle_agent_hello()`, follow the version comparison logic of provider_list/mcp_list:

```rust
// Only deliver active user profiles when Runtime's cached version is stale
let (user_identity, gw_user_version) = if user_profile_version < gw.resource_cache.user_profile_list.version {
    let active_user = gw.resource_cache.user_profile_list.users
        .iter()
        .find(|u| u.is_active)
        .cloned();
    (active_user, gw.resource_cache.user_profile_list.version)
} else {
    (None, gw.resource_cache.user_profile_list.version)
};
```

`AgentHelloResult` new fields:

```rust
AgentHelloResult {
    // ... existing fields ...

    /// Active user profile. Only included when user_profile_version in AgentHello
    /// is stale.  None when no user is active (pre-onboarding).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_identity: Option<UserProfile>,

    /// Gateway's current user profile list version
    #[serde(default)]
    user_profile_version: u64,
}
```

### 3.4 Hot Push

When the Desktop App updates a user profile or switches the active user via HTTP API, Gateway rebuilds `user_profiles.json` (version+1) and pushes updates to **all connected Runtimes** via IPC:

```rust
// New GatewayResponse variant
GatewayResponse::UserProfileUpdate {
    /// Updated active user profile
    user_identity: Option<UserProfile>,
    /// New version
    version: u64,
}
```

After receiving, Runtime calls `SessionManager::update_user_identity()` to rebuild identity context for all active sessions.

### 3.5 Version Sync

```
AgentHello.request.user_profile_version: 0 (never synced)
AgentHelloResult.user_profile_version: 7  (Gateway current version)
AgentHelloResult.user_identity: Some({...}) (diff push)

─── Subsequent Hot Push ───

UserProfileUpdate.user_identity: Some({...})
UserProfileUpdate.version: 8
```

---

## 4. Gateway Components

### 4.1 New/Modified Files

| File | Change |
|------|--------|
| `core/acowork-core/src/protocol.rs` | Add `UserProfile`, `UserProfileListFile` structs; `AgentHelloResult` adds `user_identity`/`user_profile_version` fields; `GatewayResponse` adds `UserProfileUpdate` variant; `GatewayRequest` adds `user_profile_version` field |
| `core/acowork-core/proto/gateway_ipc.proto` | Add `UserProfile`, `UserProfileUpdate` messages; `AgentHelloResult` adds `user_identity` field |
| `core/acowork-core/src/proto_bridge.rs` | Add UserProfile ↔ proto conversion |
| `core/acowork-core/src/identity.rs` | **Simplify** — Remove `IdentityCategory`/`PrivacyLevel`/`IdentityStore trait`/`IDENTITY_FIELDS` etc.; keep basic data structures if needed or mark deprecated |
| `core/acowork-gateway/src/resource_cache.rs` | `ResourceCache` adds `user_profile_list: UserProfileListFile`; add `load_user_profile_list()`, `save_user_profile_list()`, `rebuild_and_save_user_profile_cache()` |
| `core/acowork-gateway/src/ipc/server.rs` | `handle_agent_hello()` adds user identity version comparison and delivery logic; add `handle_user_send_identity()` broadcast handling |
| `core/acowork-gateway/src/http/users_api.rs` | **New** — `GET /api/users`, `PUT /api/users/{id}`, `POST /api/users` HTTP API |
| `core/acowork-gateway/src/http/routes.rs` | Add `users_routes()` merge |
| `core/acowork-gateway/src/lifecycle/manager.rs` | **Remove** `build_identity_delivery()`, `get_identity_deps()`, `load_user_display_name()`; per-agent identity construction no longer needed |
| `core/acowork-runtime/src/grpc/client.rs` | Parse `AgentHelloResult.user_identity` → `GatewayResponse::AgentHelloResult`; handle `UserProfileUpdate` push |
| `core/acowork-runtime/src/agent/session/session_manager.rs` | Add `update_user_identity()` method; `identity_context` formatted from `UserProfile` |
| `core/acowork-runtime/src/agent/session/session_task.rs` | `identity_context` accepts `UserProfile` instead of `IdentityEntry` |
| `core/acowork-runtime/src/agent/context.rs` | Simplify `identity_context` formatting logic |

### 4.2 Components to Remove

| Component | Description |
|-----------|-------------|
| `identity_store` tool | The 14th built-in tool in `12-tool-system.md` — no longer implemented |
| `identity_query` tool | The 15th built-in tool — no longer implemented |
| `identity_observe` tool | The 16th built-in tool — no longer implemented |
| `GatewayRequest::IdentityQuery` | `protocol.rs` — no longer used (kept for backward compatibility, returns empty) |
| `GatewayResponse::IdentityDelivery` | `protocol.rs` — no longer used |
| `identity_deps` (manifest) | `manifest.rs` — keep field but mark deprecated, no longer consumed |
| `identity_entries` (RunningAgentInfo) | `gateway/state.rs` — remove |
| System Agent `identity:query`/`identity:observe` capabilities | Remove from system Agent manifest |

---

## 5. HTTP API

### 5.1 `GET /api/users`

List all known users.

**Response:**
```json
{
  "users": [
    {
      "user_id": "uuid-1234",
      "display_name": "Alice",
      "language": "en-US",
      "timezone": "America/Los_Angeles",
      "city": "San Francisco",
      "occupation": "Software Engineer",
      "communication_style": "concise",
      "is_active": true,
      "created_at": "2026-05-28T00:00:00Z",
      "updated_at": "2026-05-28T10:30:00Z"
    }
  ]
}
```

### 5.2 `POST /api/users`

Create user profile (called on first Onboarding).

**Request:**
```json
{
  "display_name": "Alice",
  "language": "en-US",
  "timezone": "America/Los_Angeles",
  "city": "San Francisco",
  "occupation": "Software Engineer",
  "communication_style": "concise"
}
```

**Behavior:**
1. Gateway generates `user_id` (UUID v4)
2. Add new user to `user_profiles.json`
3. Set `is_active = true`, mark previously active user as `is_active = false`
4. Bump version
5. Push `UserProfileUpdate` to all connected Runtimes

### 5.3 `PUT /api/users/{user_id}`

Update user profile.

**Behavior:**
1. Find user by `user_id`
2. Merge update fields
3. Update `updated_at`
4. Bump version
5. Push `UserProfileUpdate` to all connected Runtimes

### 5.4 `POST /api/users/{user_id}/activate`

Switch active user (multi-user scenario).

**Behavior:**
1. Set all users `is_active = false`
2. Set specified user `is_active = true`
3. Bump version
4. Push `UserProfileUpdate` to all connected Runtimes

---

## 6. Runtime Consumption

### 6.1 Identity Context Formatting

After Runtime receives `UserProfile`, ContextBuilder formats it as part of the system prompt:

```
## User Identity
- Name: Alice
- Language: en-US
- Timezone: America/Los_Angeles
- City: San Francisco
- Occupation: Software Engineer
```

Or compact format (token-saving):

```
## User Identity
Name: Alice | Language: en-US | Timezone: America/Los_Angeles
```

### 6.2 AgentCore.user_display_name

`user_display_name` field remains in `AgentCore`, populated from `UserProfile.display_name`. Used in stop messages and other scenarios:

```rust
// loop_.rs
format!("Agent stopped by {}", self.core.user_display_name.as_deref().unwrap_or("user"))
```

### 6.3 Lifecycle

```
Gateway startup
  └─ load_resource_cache() → load user_profiles.json
       │
       ├─ Has active user  → Normal
       └─ No active user   → user_identity = None (Agent degraded)
                              ↓
Desktop App completes Onboarding
  └─ POST /api/users → create first user
       └─ Hot Push UserProfileUpdate → Runtime
            ↓
        Agent rebuilds identity context, identity perceived on next LLM call

Desktop App updates user profile
  └─ PUT /api/users/{id} → update fields
       └─ Hot Push UserProfileUpdate → Runtime
```

### 6.4 Degradation Handling

When `user_identity` is `None` (user has not completed Onboarding or no user is active):

- `identity_context` is `None`
- `user_display_name` is `None`
- Stop message falls back to `"Agent stopped by user"`
- LLM receives no user identity information and works normally

---

## 7. Proto Changes

### 7.1 New Messages

```protobuf
message UserProfile {
    string user_id = 1;
    string display_name = 2;
    string language = 3;
    string timezone = 4;
    optional string city = 5;
    optional string country = 6;
    optional string occupation = 7;
    optional string communication_style = 8;
    map<string, string> custom = 9;
    string created_at = 10;
    string updated_at = 11;
    bool is_active = 12;
}

message UserProfileUpdate {
    UserProfile user_identity = 1;     // active user profile (None = no active user)
    uint64 version = 2;
}
```

### 7.2 AgentHello Changes

```protobuf
message AgentHelloRequest {
    // ... existing fields ...
    uint64 user_profile_version = 7;   // NEW: Runtime's cached version (0 = never synced)
}

message AgentHelloResult {
    // ... existing fields ...
    UserProfile user_identity = 31;    // NEW: active user profile (only when version differs)
    uint64 user_profile_version = 32;  // NEW: Gateway's current version
}
```

### 7.3 ServerMessage Changes

```protobuf
message ServerMessage {
    // ... existing fields ...
    UserProfileUpdate user_profile_update = 38;  // NEW
}
```

---

## 8. Existing Code Migration

### 8.1 Deletion List

| File | Delete Content |
|------|----------------|
| `acowork-core/src/identity.rs` | Delete `IdentityCategory`, `PrivacyLevel`, `IdentitySubscription`, `IdentityStore trait`. Keep `IdentityEntry` marked deprecated, or remove entirely. |
| `acowork-core/src/protocol.rs` | Delete `IdentityDelivery`, `IdentityQuery` (request/response). Keep `AgentHelloResult.identity_entries` marked deprecated. |
| `acowork-core/proto/gateway_ipc.proto` | Delete `IdentityDelivery`, `IdentityQueryRequest`, `IdentityQueryResult` messages. Keep `AgentHelloResult.identity_entries_json` marked reserved. |
| `acowork-gateway/src/lifecycle/manager.rs` | Delete `build_identity_delivery()`, `get_identity_deps()`, `load_user_display_name()` and related tests |
| `acowork-gateway/src/gateway/state.rs` | `RunningAgentInfo.identity_entries` removed |
| `acowork-gateway/src/ipc/server.rs` | Delete `handle_identity_query()`. Remove `identity_entries` assembly in `AgentHelloResult` |
| `acowork-runtime/src/grpc/client.rs` | Delete `IdentityDelivery` parsing. Remove `identity_entries: vec![]` hardcoding in `AgentHelloResult` parsing |
| `acowork-runtime/src/agent/agent_core.rs` | No changes (`user_display_name` retained) |
| Project files | Remove identity tool-related references |

### 8.2 Not Removed but Deprecated

| Item | Handling |
|------|----------|
| `agents/system-agent/manifest.toml` | Remove `identity:query`/`identity:observe` capabilities |
| `agents/system-agent/prompts/system.md` | Remove identity management-related responsibilities |
| `docs/design/07-system-agent.md` | Deleted (this document replaces it) |
| `docs/design/12-tool-system.md` | `identity_store`/`identity_query`/`identity_observe` marked **deprecated**, pointing to this document |
| `docs/design/06-communication.md` §1.2 | `identity_delivery` marked deprecated, replaced by `user_identity` |

---

## 9. Multi-user Extension Reserved

Current design (v1.0) focuses on single-user scenarios, but the data model and API are already prepared for multi-user:

| Layer | Single-user (v1.0) | Multi-user (v2.0) |
|-------|-------------------|-------------------|
| `user_profiles.json` | Stores N historical users, only 1 with `is_active=true` | Stores N users, only 1 with `is_active=true` |
| `AgentHelloResult` | Only pushes active user | Only pushes active user (security consideration: do not leak other user data to Runtime) |
| `GET /api/users` | Returns all user list | Returns all user list |
| `POST /api/users/{id}/activate` | Supported | Supported |
| Runtime awareness | Only aware of current user | Only aware of current user |
| User switching | Desktop App calls activate endpoint | Desktop App calls activate endpoint, Gateway hot-pushes |

**Extension Highlights:**
- `user_profiles.json` always maintains complete data for all historical users
- Runtime only ever receives data for the current active user (security boundary)
- User switching through Gateway HTTP API → version bump → hot push
- Multi-user switching can be supported without modifying proto or Runtime code

---

## 10. Alignment Matrix with model/MCP Pattern

| Pattern Element | model (provider_list.json) | MCP (mcp_list.json) | identity (user_profiles.json) |
|----------------|---------------------------|---------------------|-------------------------------|
| Persistence file | `provider_list.json` | `mcp_list.json` | `user_profiles.json` |
| Cache structure | `ResourceCache.provider_list` | `ResourceCache.mcp_list` | `ResourceCache.user_profile_list` |
| Version number | `version: u64` | `version: u64` | `version: u64` |
| Startup load | `load_provider_list()` | `load_mcp_list()` | `load_user_profile_list()` |
| Save | `save_provider_list()` | `save_mcp_list()` | `save_user_profile_list()` |
| AgentHello push | Push on demand after version comparison | Push on demand after version comparison | Push on demand after version comparison |
| Hot Push | RuntimeConfigUpdate/LlmConfigDelivery | RuntimeConfigUpdate | UserProfileUpdate |
| HTTP API | `GET/POST /api/vault/providers` | MCP catalog API | `GET/PUT/POST /api/users` |
| Data key separation | List vs keys separate | List vs keys separate | N/A (user identity has no keys) |