# 用户身份管理（简化设计）

> 版本：v1.0 | 创建日期：2026-05-28 | 状态：设计阶段

---

## 1. 设计背景

原有设计（07-system-agent.md，已删除）将用户身份管理委托给系统 Agent（`com.acowork.system`），通过 `identity_deps` → `identity_delivery` → `identity_store`/`identity_query` 工具的全链路实现身份管理。该方案引入了过多复杂性：系统 Agent 需要运行、需要 ContentProvider 机制、需要专用的内置工具、需要跨 Agent Intent 通信链路。

本方案参照 **model/MCP/search 等公共资源的管理模式**（version-driven diff sync + AgentHelloResult 注入），将用户身份也视为 Gateway 管理的公共资源，由 Gateway 集中管理、持久化，在 Agent 握手时推送给 Runtime，并在变更时热推送。

### 设计原则

1. **单一权威源** — Gateway 是用户身份数据的唯一持有者和分发者
2. **参照现有模式** — 沿用 `ResourceCache` + `AgentHelloResult` + HTTP API 的模式，降低认知负担
3. **多用户预留** — 数据模型天然支持多个用户身份，Gateway 管理所有历史用户的完整档案，当前仅推送在线用户给 Runtime
4. **简化工具链** — 不再需要 `identity_store`/`identity_query`/`identity_observe` 工具，Runtime 从 system prompt 直接获得身份上下文

---

## 2. 数据模型

### 2.1 UserProfile（用户档案）

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

### 2.2 版本化用户列表（UserListFile）

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

**持久化位置：** `{data_dir}/user_profiles.json`

**字段语义：**

| 字段 | 来源 | 必填 | 说明 |
|------|------|------|------|
| `display_name` | Onboarding / Settings | 是 | 称谓，Agent 通过 system prompt 获知 |
| `language` | Onboarding / Settings | 是 | 语言偏好，影响 LLM system prompt 语言 |
| `timezone` | Onboarding / Settings | 是 | 时区，影响时间相关回复 |
| `city` | Onboarding / Settings | 否 | 城市 |
| `country` | Onboarding / Settings | 否 | 国家 |
| `occupation` | Onboarding / Settings | 否 | 职业/领域 |
| `communication_style` | Settings | 否 | 沟通偏好 |
| `custom` | Settings | 否 | 扩展字段 |

**未来多用户扩展：**
- `user_profiles.json` 存储所有历史用户的完整档案
- `is_active` 标记当前在线用户
- Gateway HTTP API 支持用户列表查看、切换
- Runtime 仅接收 `is_active=true` 的用户档案

---

## 3. 资源管理模式（参照 model/MCP）

### 3.1 数据流全景

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

### 3.2 资源加载与缓存

Gateway 启动时从 `{data_dir}/user_profiles.json` 加载用户列表到内存 `ResourceCache`：

```rust
impl ResourceCache {
    /// NEW: User profile list (versioned)
    pub user_profile_list: UserProfileListFile,
}
```

### 3.3 AgentHello 交付

`handle_agent_hello()` 中，参照 provider_list/mcp_list 的版本比较逻辑：

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

`AgentHelloResult` 新增字段：

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

### 3.4 热推送

当 Desktop App 通过 HTTP API 更新用户档案或切换活跃用户后，Gateway 重建 `user_profiles.json`（version+1），并通过 IPC 向**所有已连接的 Runtime** 推送更新：

```rust
// New GatewayResponse variant
GatewayResponse::UserProfileUpdate {
    /// Updated active user profile
    user_identity: Option<UserProfile>,
    /// New version
    version: u64,
}
```

Runtime 收到后调用 `SessionManager::update_user_identity()` 重建所有活跃 session 的 identity context。

### 3.5 版本同步

```
AgentHello.request.user_profile_version: 0 (从未同步)
AgentHelloResult.user_profile_version: 7  (Gateway 当前版本)
AgentHelloResult.user_identity: Some({...}) (diff 推送)

─── 后续热推送 ───

UserProfileUpdate.user_identity: Some({...})
UserProfileUpdate.version: 8
```

---

## 4. Gateway 组件

### 4.1 新增/修改文件

| 文件 | 变更 |
|------|------|
| `core/acowork-core/src/protocol.rs` | 新增 `UserProfile`、`UserProfileListFile` 结构体；`AgentHelloResult` 新增 `user_identity`/`user_profile_version` 字段；`GatewayResponse` 新增 `UserProfileUpdate` 变体；`GatewayRequest` 新增 `user_profile_version` 字段 |
| `core/acowork-core/proto/gateway_ipc.proto` | 新增 `UserProfile`、`UserProfileUpdate` 消息；`AgentHelloResult` 增加 `user_identity` 字段 |
| `core/acowork-core/src/proto_bridge.rs` | 新增 UserProfile ↔ proto 互转 |
| `core/acowork-core/src/identity.rs` | **简化** — 移除 `IdentityCategory`/`PrivacyLevel`/`IdentityStore trait`/`IDENTITY_FIELDS` 等复杂概念，保留基本数据结构（如有必要）或标记 deprecated |
| `core/acowork-gateway/src/resource_cache.rs` | `ResourceCache` 新增 `user_profile_list: UserProfileListFile`；新增 `load_user_profile_list()`、`save_user_profile_list()`、`rebuild_and_save_user_profile_cache()` |
| `core/acowork-gateway/src/ipc/server.rs` | `handle_agent_hello()` 新增 user identity 版本比较和交付逻辑；新增 `handle_user_send_identity()` 广播处理 |
| `core/acowork-gateway/src/http/users_api.rs` | **新建** — `GET /api/users`、`PUT /api/users/{id}`、`POST /api/users` HTTP API |
| `core/acowork-gateway/src/http/routes.rs` | 新增 `users_routes()` 合并 |
| `core/acowork-gateway/src/lifecycle/manager.rs` | **移除** `build_identity_delivery()`、`get_identity_deps()`、`load_user_display_name()`；不再需要 per-agent identity 构建 |
| `core/acowork-runtime/src/grpc/client.rs` | 解析 `AgentHelloResult.user_identity` → `GatewayResponse::AgentHelloResult`；处理 `UserProfileUpdate` 推送 |
| `core/acowork-runtime/src/agent/session/session_manager.rs` | 新增 `update_user_identity()` 方法；`identity_context` 从 `UserProfile` 格式化 |
| `core/acowork-runtime/src/agent/session/session_task.rs` | `identity_context` 接收类型从 `IdentityEntry` 改为 `UserProfile` |
| `core/acowork-runtime/src/agent/context.rs` | `identity_context` 格式化逻辑简化 |

### 4.2 需要移除的组件

| 组件 | 说明 |
|------|------|
| `identity_store` 工具 | 12-tool-system.md 中的第 14 个内置工具 — 不再实现 |
| `identity_query` 工具 | 第 15 个内置工具 — 不再实现 |
| `identity_observe` 工具 | 第 16 个内置工具 — 不再实现 |
| `GatewayRequest::IdentityQuery` | protocol.rs — 不再使用（但可保留以兼容旧版，返回空） |
| `GatewayResponse::IdentityDelivery` | protocol.rs — 不再使用 |
| `identity_deps` (manifest) | manifest.rs — 保留字段但标记 deprecated，不再消费 |
| `identity_entries` (RunningAgentInfo) | gateway/state.rs — 移除 |
| System Agent `identity:query`/`identity:observe` capabilities | 系统 Agent manifest 中移除 |

---

## 5. HTTP API

### 5.1 `GET /api/users`

列出所有已知用户。

**响应：**
```json
{
  "users": [
    {
      "user_id": "uuid-1234",
      "display_name": "张三",
      "language": "zh-CN",
      "timezone": "Asia/Shanghai",
      "city": "Shanghai",
      "occupation": "软件工程师",
      "communication_style": "concise",
      "is_active": true,
      "created_at": "2026-05-28T00:00:00Z",
      "updated_at": "2026-05-28T10:30:00Z"
    }
  ]
}
```

### 5.2 `POST /api/users`

创建用户档案（首次 Onboarding 时调用）。

**请求：**
```json
{
  "display_name": "张三",
  "language": "zh-CN",
  "timezone": "Asia/Shanghai",
  "city": "Shanghai",
  "occupation": "软件工程师",
  "communication_style": "concise"
}
```

**行为：**
1. Gateway 生成 `user_id`（UUID v4）
2. 将新用户加入 `user_profiles.json`
3. 设置 `is_active = true`，将之前活跃用户标记为 `is_active = false`
4. Bump version
5. 向所有已连接 Runtime 推送 `UserProfileUpdate`

### 5.3 `PUT /api/users/{user_id}`

更新用户档案。

**行为：**
1. 查找 `user_id` 对应用户
2. 合并更新字段
3. 更新 `updated_at`
4. Bump version
5. 向所有已连接 Runtime 推送 `UserProfileUpdate`

### 5.4 `POST /api/users/{user_id}/activate`

切换活跃用户（多用户场景）。

**行为：**
1. 将所有用户 `is_active` 设为 `false`
2. 将指定用户 `is_active` 设为 `true`
3. Bump version
4. 向所有已连接 Runtime 推送 `UserProfileUpdate`

---

## 6. Runtime 消费

### 6.1 Identity Context 格式化

Runtime 收到 `UserProfile` 后，ContextBuilder 将其格式化为 system prompt 的一部分：

```
## User Identity
- Name: 张三
- Language: zh-CN
- Timezone: Asia/Shanghai
- City: Shanghai
- Occupation: 软件工程师
```

或紧凑格式（token 节省）：

```
## User Identity
Name: 张三 | Language: zh-CN | Timezone: Asia/Shanghai
```

### 6.2 AgentCore.user_display_name

`user_display_name` 字段仍然保留在 `AgentCore` 中，从 `UserProfile.display_name` 填充。用于 stop 消息等场景：

```rust
// loop_.rs
format!("Agent stopped by {}", self.core.user_display_name.as_deref().unwrap_or("user"))
```

### 6.3 生命周期

```
Gateway 启动
  └─ load_resource_cache() → 加载 user_profiles.json
       │
       ├─ 有 active user  → 正常
       └─ 无 active user  → user_identity = None（Agent 降级）
                              ↓
Desktop App 完成 Onboarding
  └─ POST /api/users → 创建第一个用户
       └─ Hot Push UserProfileUpdate → Runtime
            ↓
        Agent 重建 identity context，下次 LLM 调用感知用户身份

Desktop App 更新用户资料
  └─ PUT /api/users/{id} → 更新字段
       └─ Hot Push UserProfileUpdate → Runtime
```

### 6.4 降级处理

当 `user_identity` 为 `None`（用户尚未完成 Onboarding 或所有用户均非 active）：

- `identity_context` 为 `None`
- `user_display_name` 为 `None`
- stop 消息回退为 `"Agent stopped by user"`
- LLM 不获取任何用户身份信息，正常工作

---

## 7. Proto 变更

### 7.1 新增消息

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

### 7.2 AgentHello 变更

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

### 7.3 ServerMessage 变更

```protobuf
message ServerMessage {
    // ... existing fields ...
    UserProfileUpdate user_profile_update = 38;  // NEW
}
```

---

## 8. 现有代码迁移

### 8.1 删除清单

| 文件 | 删除内容 |
|------|---------|
| `acowork-core/src/identity.rs` | 删除 `IdentityCategory`、`PrivacyLevel`、`IdentitySubscription`、`IdentityStore trait`。保留 `IdentityEntry` 标记 deprecated，或彻底移除。 |
| `acowork-core/src/protocol.rs` | 删除 `IdentityDelivery`、`IdentityQuery`（request/response）。保留 `AgentHelloResult.identity_entries` 标记 deprecated。 |
| `acowork-core/proto/gateway_ipc.proto` | 删除 `IdentityDelivery`、`IdentityQueryRequest`、`IdentityQueryResult` 消息。保留 `AgentHelloResult.identity_entries_json` 标记 reserved。 |
| `acowork-gateway/src/lifecycle/manager.rs` | 删除 `build_identity_delivery()`、`get_identity_deps()`、`load_user_display_name()` 及相关测试 |
| `acowork-gateway/src/gateway/state.rs` | `RunningAgentInfo.identity_entries` 删除 |
| `acowork-gateway/src/ipc/server.rs` | 删除 `handle_identity_query()`。`AgentHelloResult` 中移除 `identity_entries` 组装 |
| `acowork-runtime/src/grpc/client.rs` | 删除 `IdentityDelivery` 解析。`AgentHelloResult` 解析中移除 `identity_entries: vec![]` 硬编码 |
| `acowork-runtime/src/agent/agent_core.rs` | 无变更（`user_display_name` 保留） |
| 项目文件 | 移除 identity 工具相关引用 |

### 8.2 不删除但降级

| 项目 | 处理 |
|------|------|
| `agents/system-agent/manifest.toml` | 移除 `identity:query`/`identity:observe` capabilities |
| `agents/system-agent/prompts/system.md` | 移除 identity management 相关职责描述 |
| `docs/design/07-system-agent.md` | 已删除（本文档替代） |
| `docs/design/12-tool-system.md` | `identity_store`/`identity_query`/`identity_observe` 标记为 **已废弃**，指向本文档 |
| `docs/design/06-communication.md` §1.2 | `identity_delivery` 标记 deprecated，替换为 `user_identity` |

---

## 9. 多用户扩展预留

当前设计（v1.0）聚焦单用户场景，但数据模型和 API 已为多用户做好准备：

| 层级 | 单用户（v1.0） | 多用户（v2.0） |
|------|---------------|---------------|
| `user_profiles.json` | 存储 N 个历史用户，仅 1 个 `is_active=true` | 存储 N 个用户，仅 1 个 `is_active=true` |
| `AgentHelloResult` | 只推送 active user | 仅推送 active user（安全考虑：不泄露其他用户数据给 Runtime） |
| `GET /api/users` | 返回所有用户列表 | 返回所有用户列表 |
| `POST /api/users/{id}/activate` | 支持 | 支持 |
| Runtime 感知 | 只感知当前用户 | 只感知当前用户 |
| 用户切换 | Desktop App 调用 activate 端点 | Desktop App 调用 activate 端点，Gateway 热推送 |

**扩展要点：**
- `user_profiles.json` 始终保持所有历史用户的完整数据
- Runtime 永远只接收当前 active 用户的数据（安全边界）
- 用户切换通过 Gateway HTTP API → 版本 bump → 热推送实现
- 无需修改 proto 或 Runtime 代码即可支持多用户切换

---

## 10. 与 model/MCP 模式的对齐矩阵

| 模式要素 | model (provider_list.json) | MCP (mcp_list.json) | identity (user_profiles.json) |
|---------|---------------------------|---------------------|-------------------------------|
| 持久化文件 | `provider_list.json` | `mcp_list.json` | `user_profiles.json` |
| 缓存结构 | `ResourceCache.provider_list` | `ResourceCache.mcp_list` | `ResourceCache.user_profile_list` |
| 版本号 | `version: u64` | `version: u64` | `version: u64` |
| 启动加载 | `load_provider_list()` | `load_mcp_list()` | `load_user_profile_list()` |
| 保存 | `save_provider_list()` | `save_mcp_list()` | `save_user_profile_list()` |
| AgentHello 推送 | 版本比对后按需推送 | 版本比对后按需推送 | 版本比对后按需推送 |
| Hot Push | RuntimeConfigUpdate/LlmConfigDelivery | RuntimeConfigUpdate | UserProfileUpdate |
| HTTP API | `GET/POST /api/vault/providers` | MCP catalog API | `GET/PUT/POST /api/users` |
| 数据密钥分离 | 列表 vs 密钥分开 | 列表 vs 密钥分开 | N/A（用户身份不含密钥） |
