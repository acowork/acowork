# acowork-core — Shared Types and Protocols

> Part of [Module Design Overview](00-overview.md)

---

## Directory Structure

```
crates/acowork-core/
├── Cargo.toml
└── src/
    ├── lib.rs                 # crate entry + re-exports
    ├── manifest.rs            # manifest.toml data structures
    ├── protocol.rs            # Gateway Service API message definitions
    ├── intent.rs              # Intent message structures
    ├── permission.rs          # permission declaration and verification types
    ├── identity.rs            # user identity data structures
    ├── budget.rs              # budget/usage types
    ├── tools/
    │   ├── mod.rs
    │   ├── traits.rs          # Tool trait + ToolSpec + ToolResult
    │   └── schema.rs          # tool JSON Schema sanitization
    ├── providers/
    │   ├── mod.rs
    │   └── traits.rs          # Provider trait + ChatMessage + ChatResponse + StreamEvent
    ├── memory/
    │   ├── mod.rs
    │   └── traits.rs          # Memory trait (storage-agnostic abstraction layer)
    └── error.rs               # unified error type
```

---

## Key Type Designs

### manifest.rs

```rust
/// .agent package's manifest.toml complete data structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub agent_id: String,
    pub version: String,
    pub name: String,
    pub description: String,
    pub author: String,
    pub runtime_version: String,
    pub permissions: Vec<Permission>,
    pub triggers: Vec<Trigger>,
    pub llm: LlmConfig,
    pub memory: MemoryConfig,
    pub identity_deps: Vec<String>,
    pub tools: Vec<ToolDeclaration>,
    pub capabilities: HashMap<String, CapabilityDef>,
    pub resources: ResourceLimits,
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub system: bool,
    #[serde(default)]
    pub dev: bool,
}
```

### protocol.rs

```rust
/// Gateway Service API request (contract layer, transport-agnostic)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GatewayRequest {
    KeyRelease { provider: String },
    IntentSend { target: String, action: String, params: Value, async_: bool },
    BudgetQuery { provider: String },
    UsageReport(UsageReport),
    RateAcquire { provider: String },
    PermissionRequest { permission: String, reason: String },
}

/// Gateway Service API response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GatewayResponse {
    KeyReleaseResult { api_key: String },
    IntentDelivered { message_id: String },
    IntentReceived { from: String, action: String, params: Value },
    BudgetInfo { remaining_tokens: u64, remaining_cost_usd: f64 },
    UsageReportAck {},
    RateToken { granted: bool, retry_after_ms: Option<u64> },
    PermissionResult { granted: bool, reason: Option<String> },
}

/// Transport layer frame format
pub struct Frame {
    pub body_len: u32,         // 4 bytes big-endian
    pub msg_type: u8,          // 0=request, 1=response, 2=stream_chunk, 3=error
    pub body: Vec<u8>,         // JSON payload
}
```

### permission.rs

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Permission {
    Network(String),               // "network:https://api.weather.com"
    FilesystemRead(String),        // "filesystem:read:~/Documents"
    FilesystemWrite(String),       // "filesystem:write:~/Documents"
    // MemoryRead / MemoryWrite were removed by ADR-101 (their binary tags 3/4 are
    // retired too, not renumbered): memory is served by the Gateway-hosted
    // acowork-memory service now and is no longer a permission domain.
    IntentSend(String),            // "intent:send:com.example.calendar"
    IntentReceive(String),         // "intent:receive:com.example.weather"
    Shell,                         // "shell"
}
```

---

## Dependencies

Only `serde`, `serde_json`, `async-trait`, `thiserror`, `chrono`, `uuid`

## Design Decisions

- Provider trait placed in core rather than runtime: Gateway's Budget Tracker needs to know Provider name for statistics, doesn't depend on specific implementation
- Tool trait placed in core: Gateway needs to parse tool declarations in manifest for permission verification
- Zero heavy dependencies: doesn't depend on tokio (async methods in traits via `async-trait`, returning `Pin<Box<dyn Future>>`)
