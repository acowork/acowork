# acowork-core — 共享类型与协议

> 属于 [模块设计总览](00-overview.md) 的一部分

---

## 目录结构

```
crates/acowork-core/
├── Cargo.toml
└── src/
    ├── lib.rs                 # crate 入口 + re-exports
    ├── manifest.rs            # manifest.toml 数据结构
    ├── protocol.rs            # Gateway Service API 消息定义
    ├── intent.rs              # Intent 消息结构
    ├── permission.rs          # 权限声明与校验类型
    ├── identity.rs            # 用户身份数据结构
    ├── budget.rs              # 预算/用量类型
    ├── tools/
    │   ├── mod.rs
    │   ├── traits.rs          # Tool trait + ToolSpec + ToolResult
    │   └── schema.rs          # 工具 JSON Schema 清洗
    ├── providers/
    │   ├── mod.rs
    │   └── traits.rs          # Provider trait + ChatMessage + ChatResponse + StreamEvent
    ├── memory/
    │   ├── mod.rs
    │   └── traits.rs          # Memory trait（SQLite 记忆层 抽象层）
    └── error.rs               # 统一错误类型
```

---

## 关键类型设计

### manifest.rs

```rust
/// .agent 包的 manifest.toml 完整数据结构
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
/// Gateway Service API 请求（合同层，与传输层无关）
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

/// Gateway Service API 响应
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

/// 传输层帧格式
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
    // MemoryRead / MemoryWrite 已随 ADR-101 删除（其二进制 tag 3/4 一并废弃、不重编号）：
    // 记忆改由 Gateway 托管的 acowork-memory 服务承载，不再是权限域。
    IntentSend(String),            // "intent:send:com.example.calendar"
    IntentReceive(String),         // "intent:receive:com.example.weather"
    Shell,                         // "shell"
}
```

---

## 依赖

仅 `serde`, `serde_json`, `async-trait`, `thiserror`, `chrono`, `uuid`

## 设计决策

- Provider trait 放在 core 而非 runtime：Gateway 的 Budget Tracker 需要知道 Provider 名称做统计，不依赖具体实现
- Tool trait 放在 core：Gateway 需要解析 manifest 中的 tool 声明做权限校验
- 零重型依赖：不依赖 tokio（trait 中的 async 方法通过 `async-trait` 实现，返回 `Pin<Box<dyn Future>>`）

