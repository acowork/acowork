# Agent Packaging Format (.agent)

> Version: v3.3 | Last Updated: 2026-04-17

---

## 1. Package Structure

The `.agent` file is essentially a ZIP archive. Agent packages **contain no executable code** — only configuration, Prompts, and data. The Agent Runtime binary loads and executes them.

```
<agent_id>.agent
├── manifest.toml          # Required: metadata + LLM config + permissions + tool declarations
├── prompts/               # System prompt templates
│   ├── system.md          # Main system prompt
│   ├── tools.md           # Tool usage instructions
│   └── constraints.md     # Constraints and safety rules
├── config/                # Default config files (user-overridable)
│   └── settings.toml
├── data/                  # Initial data (e.g. empty SQLite memory layer snapshot)
├── skills/                # Skill definitions (compatible with Agent Skills open standard)
│   └── weather-query/
│       ├── SKILL.md       # YAML frontmatter (---) + Markdown body
│       └── references/    # Optional: supplementary docs, template data
├── tools/                 # Custom tools (WASM, optional)
│   └── image_filter.wasm
└── resources/             # Icons, localization, etc.
```

**Constraints:**

- Package size limit **50 MB** (validated at install time; oversized packages are rejected).
- `skills/*/references/` only allows non-executable data files (JSON templates, Markdown docs, etc.). Dynamic execution logic must be implemented via WASM tools under `tools/`.

## 2. Package Signing Mechanism

.agent packages must be signed before Gateway will install and execute them, similar to Android APK signatures. The signing mechanism protects three core security properties: **integrity** (package has not been tampered with), **source authentication** (verifying developer identity), and **update protection** (preventing malicious packages from replacing installed Agents).

### 2.1 Signature Structure — Signing Block

Following the approach of APK Signature Scheme v2, a Signing Block is inserted before the ZIP Central Directory. The signature covers all ZIP contents (Local Files + Central Directory + End of Directory), rather than placing signature files inside the ZIP's `META-INF/` directory (which can be bypassed via strip attacks):

```
.agent ZIP Structure:
┌──────────────────────┐
│   ZIP Local Files    │  ← covered by signature
│   (manifest.toml,    │
│    prompts/, skills/)│
├──────────────────────┤
│   Signing Block      │  ← signature data (before Central Dir)
│   ┌────────────────┐ │
│   │  Signer        │ │
│   │  - certificates│ │     X.509 certificate chain
│   │  - digest list │ │     SHA-256 digests per section
│   │  - signature   │ │     signature over digest list
│   └────────────────┘ │
├──────────────────────┤
│   ZIP Central Dir    │  ← covered by signature
├──────────────────────┤
│   ZIP End of Dir     │  ← covered by signature
└──────────────────────┘
```

### 2.2 Signature Data Structure

```rust
struct SigningBlock {
    signers: Vec<Signer>,
}

struct Signer {
    certificates: Vec<Certificate>,     // X.509 certificate chain
    digest_algorithm: DigestAlgorithm,  // SHA-256
    digests: Vec<SectionDigest>,        // Per-section digests
    signature: Vec<u8>,                 // Signature over digests
    signed_attrs: SignedAttributes,     // Signature timestamp, etc.
}

struct SectionDigest {
    section: Section,       // LocalFiles / CentralDir / EoCD
    hash: [u8; 32],         // SHA-256 digest
}
```

### 2.3 Two Signing Identities

Phase 1 implements two signing identities. Future phases will introduce Distribution Key (store distribution key); see Section 5.

| Identity Type | Use Case | Description | Android Analogy |
|--------------|----------|-------------|-----------------|
| Developer | Regular Agents | Self-signed certificates, most common | Standard app debug/release signatures |
| Platform | System Agents | Certificates signed by the ACowork platform; Gateway has the platform root public key built-in | Platform signature (system apps) |

### 2.4 Signature Verification Flow

> **Phase 1 Note**: Signature verification is optional in the current phase — unsigned .agent packages are allowed to install but generate a warning log. Strict enforcement will be implemented in Phase 2.

```
User installs .agent package
       │
       ▼
1. Parse ZIP, extract Signing Block
       │
       ▼
2. Verify signature with certificate's public key
   (proves "the package was signed by the holder of this private key")
   └─ Failure → Reject install: "Invalid signature"
       │
       ▼
3. Recompute per-section SHA-256 digests, compare against signed digests
   (proves "package contents have not been tampered with")
   └─ Mismatch → Reject install: "Package has been tampered with"
       │
       ▼
4. Verify certificate trust (distinguish signing identity):
   │
   ├─ Was the certificate issued by the Gateway's built-in platform root key?
   │   └─ Yes → Identify as Platform signature
   │
   └─ No → Identify as Developer self-signed
            (self-signed certificate chains have no security value — anyone can self-sign)
       │
       ▼
5. Check identity matches manifest declaration:
   ├─ manifest declares platform.system = true → must be Platform signature
   │   └─ Not Platform signature → Reject: "System Agents must be signed by the platform"
   │
   └─ Old version of same agent_id already installed → compare certificate fingerprints
       └─ Fingerprint mismatch → Reject update: "Signer differs from installed version"
       │
       ▼
6. All checks pass → Allow install
```

**Trust Model Differences Between Two Identities:**

| Verification Dimension | Developer Self-Signed | Platform Key |
|------------------------|----------------------|--------------|
| Integrity | SHA-256 digest comparison | Same as left |
| Source authentication | Certificate fingerprint consistency (match on update) | Certificate chain verification (must chain to platform root CA) |
| First install | Any self-signed certificate is accepted | Must match platform root public key |
| Update install | New package fingerprint = installed fingerprint | Same (platform root CA is stable) |

Developer self-signed security relies on **fingerprint lock-in**: the certificate fingerprint is recorded on first install, and subsequent updates must match. This aligns with what Android did early on (v1 signature) for regular apps — you may not know who the developer is, but you can guarantee updates come from the same developer.

### 2.5 Signature-to-Permission Mapping

Gateway configuration can define trust rules based on signers; Agents with specific signatures can automatically receive additional permissions:

```toml
# ~/.config/agent-gateway/config.toml

[trust]
# Permissions automatically granted to platform-signed Agents
platform_signer_permissions = [
    "identity:admin",       # Can manage user identity
    "agent:install",        # Can trigger installation of other Agents
    "sandbox:bypass",       # Can bypass sandbox (system Agents)
]

# Trust rules for specific certificate fingerprints
[[trusted_signers]]
fingerprint = "sha256:AB:CD:EF:..."
permissions = ["network:*"]
label = "Trusted Weather Developer"
```

### 2.6 Signing Toolchain

```bash
# Generate developer key pair
acowork-keygen --alias my-key --output ./keys/

# Sign .agent package
acowork-sign \
    --key ./keys/my-key.pem \
    --cert ./keys/my-key.crt \
    --input ./build/com.example.weather.unsigned.agent \
    --output ./build/com.example.weather.agent

# Verify signature
acowork-verify ./build/com.example.weather.agent

# View signature information
acowork-verify --verbose ./build/com.example.weather.agent
# Output: Signer: CN=Zhang San, O=Example Corp
#         Digest: SHA-256
#         Valid from: 2026-01-01 to 2027-01-01
```

### 2.7 Debug Signature

Reference Android Debug Keystore mechanism to provide convenient signing for local development and testing.

A debug key is auto-generated on first run of the `acowork` CLI:

```
~/.config/acowork/debug.key
~/.config/acowork/debug.crt

- Algorithm: Ed25519
- Validity: 1 year
- Local development only, not for production distribution
```

```bash
# Use --debug to auto-select debug key
acowork-sign --debug \
    --input ./build/com.example.weather.unsigned.agent \
    --output ./build/com.example.weather.debug.agent
```

Gateway behavior when installing debug packages:

1. Verify signature integrity (same as production packages)
2. Detect debug certificate fingerprint → emit warning: "Debug signature, for local development only"
3. `platform.system = true` debug packages are still rejected (debug key is not a Platform Key)
4. Production environments can disable debug package installation in Gateway config:

```toml
# ~/.config/agent-gateway/config.toml
[debug]
allow_debug_packages = true   # Set to false in production
```

### 2.8 System Agent Local Debug

Agents with `platform.system = true` must be signed by the Platform Key, but local developers don't have one. Phase 1 addresses this with **local trust configuration** — the developer explicitly trusts specific fingerprints in the Gateway config to run with system permissions:

```toml
# ~/.config/agent-gateway/config.toml
[debug]
# Allow packages with specified fingerprints to run with system permission (dev only)
[[debug_platform_overrides]]
fingerprint = "sha256:12:34:56:..."
agent_id = "com.acowork.system"
note = "For local development and debugging"
```

This is purely local and does not depend on any online platform service. A more complete Debug Platform Key mechanism (online request, remote revocation, etc.) is left for Phase 5.

## 3. manifest.toml Format

```toml
agent_id = "com.example.weather"
version = "1.0.0"
name = "Weather Agent"
display_name = "Weather Assistant"
role = "Weather Specialist"
description = "Query real-time weather and suggest clothing"
author = "example@domain.com"
runtime_version = "0.1.0"
system = false
dev = false

# Avatar: prefer bundled image (assets/avatar.png), else use built-in icon ("icon-05" / "5")
# If neither is set, a random built-in icon is assigned on first install
# avatar = "assets/avatar.png"
# builtin_avatar = "icon-05"

# Permissions use array-of-tables syntax
[[permissions]]
type = "Network"
value = "https://api.weather.com"

[[permissions]]
type = "FilesystemRead"

# Memory is no longer a permission domain: ADR-100 split it out into a Gateway-hosted
# standalone service, so `memory_store` / `memory_recall` are now tools of that service
# (MCP server id = `memory`). Per-tool opt-in lives in `agent_mcp_tools.json` under the
# prefixed names `mcp_memory__*` (ADR-069), and the deleted `MemoryRead` / `MemoryWrite`
# types are rejected as unknown.

[[permissions]]
type = "IntentSend"
value = "com.example.calendar"

# Triggers
triggers = []

[llm]
temperature = 0.7

[llm.providers.openai]
model = "gpt-4o"
api_key_ref = "vault:openai_key"
base_url = "https://api.openai.com/v1"

[llm.providers.claude]
model = "claude-sonnet-4-20250514"
api_key_ref = "vault:anthropic_key"

[llm.routing]
strategy = "quality_priority"
fallback_order = ["openai", "claude"]

[llm.budget]
max_output_tokens = 8192
exceeded_action = "warn"

[memory]
enabled = true
retention_days = 90

identity_deps = ["display_name", "language", "timezone"]

[[tools]]
name = "http_request"

[[tools]]
name = "memory_store"

[[tools]]
name = "memory_recall"

# Enterprise RAG tool (optional)
[[tools]]
type = "rag"
name = "enterprise_knowledge"
[tools.rag]
endpoint = "https://rag.internal.company.com/api/query"
collection = "product_docs"
auth_ref = "vault:company_rag_token"
auth_type = "bearer"
max_results = 5
score_threshold = 0.7

# Capabilities use map syntax
[capabilities.query_weather]
description = "Query weather information"

[capabilities.query_weather.input_schema]
type = "object"
properties.city = { type = "string" }
properties.date = { type = "string" }

[resources]
max_memory_mb = 512
idle_timeout_secs = 300

[sandbox]
enabled = false

[skills]
progressive = false
```

**Key Field Descriptions:**

- `runtime_version`: Declares compatible Agent Runtime version. Currently `"0.1.0"`.
- `dev`: Whether this is a development mode Agent. Used for local development and testing.
- `display_name` / `role`: Short name and role title for UI display. `display_name` defaults to `name`.
- `avatar`: Optional, in-package avatar image path (e.g. `"assets/avatar.png"`). Highest priority.
- `builtin_avatar`: Optional, built-in avatar index (e.g. `"icon-05"` or bare number `"5"`). Used to assign a deterministic default icon to first-time installs when no `avatar` is set. Clients normalize this alphabetically/numerically to `"icon-XX"` and validate against their bundled icon set; fall back to random if unrecognized.

#### Avatar Resolution Priority (Client Contract)

Agent clients rendering Agent avatars MUST follow this priority (aligned with [Avatar resolution priority policy](../development_practice/agent-avatar-priority.md)):

1. **`manifest.avatar` in-package avatar** (highest priority). Desktop App fetches it via Gateway's `GET /api/agents/:id/avatar` endpoint with `?v=<manifest.version>` appended as the HTTP cache buster. When the package is reinstalled (same `agent_id` but different `version`), the URL changes automatically and the browser/WebView re-downloads.
2. **`avatarIconId` in the profile store** (local persistence). Overrides both: the user-selected icon via `AgentSetupTab`, and the default icon assigned by `ensureBuiltinAvatars` on first install. The profile store self-heals after every `fetchAgents`: if the manifest newly adds `avatar`, any residual `avatarIconId` is cleared (so the in-package avatar takes effect).
3. **Deterministic random built-in icon**. A hash-based selection from `BUILTIN_ICONS[id]` by `agentId`, consistent with what `ensureBuiltinAvatars` is about to assign, avoiding first-render flicker.

This priority must also be preserved in remote Gateway scenarios (see [04-gateway.md § 9.6](./04-gateway.md#96-security-design)).

- `permissions`: Uses TOML array-of-tables syntax; each entry contains `type` and optional `value`. Supports `Network`, `FilesystemRead`, `FilesystemWrite`, `IntentSend`, etc. (`MemoryRead` / `MemoryWrite` were removed by ADR-100 — memory is served by a hosted service now, so it is no longer a permission domain.)
- `triggers`: Activation trigger array. Supports `cron`, `event`, `manual` types. `cron` uses standard 5-field expressions (UTC); second-level precision and special macros are not supported.
- `llm.providers`: Supports multiple LLM Providers, each referencing a Vault key.
- `llm.routing.strategy`: LLM routing strategy (`cost_priority` / `quality_priority` / `latency_priority`).
- `llm.budget`: Token and cost budget; action on overrun (`stop` / `fallback_to_local` / `warn`).
- `memory`: Memory system configuration. `enabled` toggles; `retention_days` is the retention period.
- `identity_deps`: Declares required user identity fields at startup (e.g. `display_name`, `language`, `timezone`); Gateway injects via UserProfile during handshake.
- `tools`: Tool declaration array. Supports `builtin` (default) and `rag` types. RAG tools must configure endpoint, auth, etc. in `[tools.xxx.rag]`.
- `capabilities`: Declares capabilities this Agent can expose to other Agents via Intent. Uses map syntax (action name → description + schema).
- `skills`: Skill system configuration. `progressive = true` enables progressive skill injection (only summaries injected into system prompt; full instructions loaded on demand).

### 3.1 identity_deps Injection Details

`identity_deps` is a string array declaring required user identity fields at Agent startup. Gateway injects UserProfile into Runtime during handshake (AgentHello → AgentHelloResult), bypassing the system Agent. See [18-user-identity-simplified.md](./18-user-identity-simplified.md).

**Field Naming Convention**:

| Field Name | Meaning | Source | Phase 1 Default (when Gateway has no data) |
|-----------|---------|--------|--------------------------------------|
| `display_name` | How the user wants to be addressed | Onboarding required | `""` (empty string, Agent should gracefully degrade in prompt) |
| `language` | User language preference (BCP 47) | Onboarding required | `"en-US"` (safe fallback to English) |
| `timezone` | User timezone (IANA) | Onboarding required | `"UTC"` (safe fallback) |
| `city` | Current city | Onboarding optional | `null` (unknown, do not guess) |
| `occupation` | Occupation/domain | Conversation derived | `null` |
| `communication_style` | Communication preference | Conversation derived | `null` |
| `custom:*` | Open extension fields | Various | `null` |

**Required vs Optional Semantics**:

All fields in the `identity_deps` array are treated as **optional** — even if the Agent declares `identity_deps = ["display_name", "city"]`, if Gateway doesn't have these fields (user has not provided them), Runtime starts normally; the injected UserProfile simply has `null` for those fields.

This design is based on the following considerations:
- Agents should not refuse to work just because they're missing some identity (e.g. a weather Agent that doesn't know the user's city should ask)
- Required vs optional control belongs to the Onboarding side (which fields are mandatory), not the Agent declaration side
- If truly required semantics are needed in the future, append `!` to the field name (e.g. `"city!"`), but Phase 1 does not implement this

**Handling Missing Fields in identity_delivery**:

```json
// Agent declares identity_deps = ["display_name", "city", "occupation"]
// Gateway only knows display_name

{
    "type": "user_identity",
    "fields": {
        "display_name": "Alice",
        "city": null,
        "occupation": null
    }
}
```

- Known field: actual value returned
- Unknown field: value is `null`
- Field name Gateway doesn't recognize: still returns `null` (no error)

### 3.2 Permission Matching Semantics

Each permission string in the permissions array follows a unified pattern syntax. Gateway and Runtime use this to determine authorization status of tool calls.

**Permission Format**:

```
<domain>:<resource>[:<qualifier>]
```

| Component | Description | Example |
|-----------|-------------|---------|
| `domain` | Permission domain (major category) | `network`, `filesystem`, `memory`, `intent` |
| `resource` | Specific resource or operation | `https://api.weather.com`, `read`, `write`, `send` |
| `qualifier` | Optional qualifier | `~/Documents`, `com.example.calendar` |

**Wildcard Rules**:

| Pattern | Meaning | Match Example | No Match |
|---------|---------|---------------|----------|
| `network:https://api.weather.com` | Exact match | `https://api.weather.com` | `https://api.other.com` |
| `network:https://*.weather.com` | Subdomain wildcard | `https://api.weather.com`, `https://v2.weather.com` | `https://weather.com` (bare domain does not match) |
| `network:*` | Entire network domain | Any HTTPS URL | — |
| `filesystem:read:*` | Entire filesystem:read domain | `~/Documents`, `/tmp` | — |
| `filesystem:*:*` | Entire filesystem domain | read/write any path | — |
| `intent:send:*` | Can send Intent to any Agent | `com.example.calendar`, `com.acowork.system` | — |

> Earlier revisions listed a `memory:read` row here; since ADR-100 memory is no longer a permission domain, so the wildcard rules do not apply to it.

**Matching Algorithm**:

```rust
fn matches_permission(declared: &str, requested: &str) -> bool {
    let decl_parts: Vec<&str> = declared.splitn(3, ':').collect();
    let req_parts: Vec<&str> = requested.splitn(3, ':').collect();

    // 1. Domain must match exactly
    if decl_parts[0] != req_parts[0] { return false; }

    // 2. Resource matching (supports * wildcard and subdomain *.example.com)
    if decl_parts[1] == "*" { return true; }  // whole domain wildcard
    if !wildcard_match(decl_parts[1], req_parts[1]) { return false; }

    // 3. Qualifier matching (if declared)
    if decl_parts.len() == 3 && req_parts.len() == 3 {
        wildcard_match(decl_parts[2], req_parts[2])
    } else if decl_parts.len() == 3 && req_parts.len() == 2 {
        false  // qualifier declared but request has none — no match
    } else {
        true   // neither has qualifier — match
    }
}
```

**Permission Check Flow**:

1. Agent initiates tool call (e.g. `http_request({"method":"GET","url":"https://api.weather.com/..."})`)
2. Runtime constructs request permission string (e.g. `network:https://api.weather.com`)
3. Iterate manifest.permissions, call `matches_permission`
4. Any declared permission matches → allow execution
5. No match → reject, return PermissionDenied error

**Phase 1 Actual Permission List**:

| Domain | Permission Example | Corresponding Tool |
|--------|-------------------|-------------------|
| network | `network:https://api.example.com` | http_request |
| filesystem | `filesystem:read:~/Documents`, `filesystem:write:~/Documents` | file_read, file_write |
| filesystem | `filesystem:read:/tmp` | file_read |
| intent | `intent:send:com.example.calendar` | intent_send |
| shell | (shell tool requires no permission declaration; controlled by Approval Gate) | shell |
| identity | (identity_store available to system Agents only; authorized via `platform.system` declaration) | identity_store |

## 4. Design Decision Records

| Decision | Choice | Reason |
|----------|--------|--------|
| Signature scheme | v2 style (single signer) | Phase 1 minimum implementation; complex features like key rotation deferred to Phase 5 |
| Signature metadata location | Signing Block metadata | Signature verification data is not part of the developer declaration layer; not placed in manifest.toml |
| `system` field location | Top-level manifest field | Security-sensitive attributes declared independently |
| Package size limit | 50 MB | Prevents oversized SQLite memory layer snapshots or WASM tools causing installation issues |
| capabilities syntax | Map (not array) | Action names are naturally unique; maps are more intuitive than arrays |
| Platform compatibility model | Android uses-feature (required/optional) | shell is unavailable on mobile, file ops are restricted; declarative degradation is needed |
| target_platforms | Not implemented yet, left for Phase 5+ | Mobile compatibility declarations |
| RAG tool declaration | Independent type="rag" + rag_config section | Enterprise RAG is an external service requiring endpoint/auth/collection independent configuration |

## 5. Future Extensions (Phase 5+)

The following features will be implemented during the cloud and ecosystem phases. Current record is design direction only — not implemented in Phase 1.

### 5.1 Dual Key Model (Play App Signing Analogy)

Introduce separation of Upload Key (developer-held) and Distribution Key (store-held). Developers sign submissions with Upload Key; the store re-signs with Distribution Key before distribution. Benefit: if developer loses Upload Key, they can reset without affecting installed users' updates.

### 5.2 Key Rotation (Proof-of-Rotation)

Reference APK Signature Scheme v3 to support signing key rotation. The Signing Block may contain multiple Signers; trust chain from old key to new key is established via `proof_of_rotation` field. Older Runtime versions can still verify via historical Signers.

### 5.3 Distribution Key (Store Distribution Key)

Add a third signing identity. The store/platform holds the Distribution Key, used to re-sign developer submissions. At install time, Gateway verifies based on the store's root CA for Distribution Key.

### 5.4 Certificate Revocation and CRL

The store maintains a Certificate Revocation List (CRL); Gateway queries online during install. Developers can request revocation and reset Upload Key; the store issues new certificates with proof_of_rotation.

### 5.5 Debug Platform Key (Online Application)

Developers apply for a Debug Platform Key (30-day validity, remotely revocable) via platform account authentication, used for local debugging of system=true Agents. Replaces the current Phase 1 local trust configuration scheme.