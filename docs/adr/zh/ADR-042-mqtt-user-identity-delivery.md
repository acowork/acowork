# ADR-042: User Identity 通过 MQTT 全局资源主题下发

**状态**：草案
**日期**：2026-07-21
**决策者**：大鱼

**前置**：
- [ADR-016](./ADR-016-ipc-grpc-migration.md)（IPC gRPC 迁移）
- [ADR-033](./ADR-033-mqtt-replace-grpc-websocket.md)（MQTT 替换 gRPC + WebSocket）
- [ADR-040](./ADR-040-runtime-adapter-use-case-layer.md)（移除 gRPC hello_config）
- [ADR-011](./ADR-011-compaction-as-distillation.md)（context compaction 引入 identity_context 注入机制）

---

## 1. 决策摘要

ADR-040 移除了 gRPC hello_config 路径（Runtime 不再主动从 Gateway 拉取 UserProfile），但 ADR-033 的 MQTT 重构**没有补上对应的 UserProfile 下发通道**，导致 `acowork-runtime` 启动时 `identity_context = None`：

```rust
// core/acowork-runtime/src/startup/agent_init.rs:568-571
// ADR-040: gRPC hello_config path removed. User identity is not yet
// available via MQTT; context builder is created without identity.
let identity_context: Option<String> = None;
```

identity_context 为空会让 `compaction system prompt` 拿不到用户的语言偏好（`Language: zh-CN` 字段），compact model 默认输出英文摘要 —— 实测 `Hi, 测试一下用户消息在上下文之后` 这种对话被压缩后出来的是英文 summary。

本 ADR 引入一条新主题 **`acowork/global/user_profile`**（Retained + QoS 1），让 Gateway 把当前 active user 的 profile 以 retained snapshot 方式下发，Runtime 订阅后写入 `identity_context`，同时支持运行期 hot-push（用户切换 active profile 时所有 Runtime 立即收到）。

**三条核心设计**：

1. **Owner 单一**：Gateway 是 UserProfile 的唯一权威（持有 Vault + 持久化文件 + 管理 active user），Runtime 不拥有 UserProfile，只能订阅。
2. **Payload 用 `AvailableUsers` wrapper**：跟 `AvailableProviders` / `AvailableMcps` 等 §3.1.1 兄弟消息命名一致；保留 `version` 字段做乱序保护。
3. **只下发 active user，不下发整张表**：单用户阶段 active 即可满足所有使用场景；多用户阶段通过 §3.4 `acowork/users/{user_id}/...` 解决（已预留）。

---

## 2. 根因分析

### 2.1 gRPC 时代的链路（ADR-016 引入）

```
Desktop
  │ POST /api/users (CRUD)
  ▼
Gateway (UserProfile in resource_cache.user_profile_list)
  │
  │ gRPC hello_config (PushUserProfile) ← 旧路径
  ▼
Runtime (IdentityContext = format_user_profile_context(profile))
  │
  ▼
ContextBuilder → SessionState.identity_context → compaction system prompt
```

### 2.2 ADR-033 切断 gRPC

ADR-033 决定用 MQTT 替代 gRPC + WebSocket，但**只覆盖了"事件流 + 实时状态同步"两类数据**，UserProfile 这种"启动期一次性配置数据"没有被映射到新通道。

### 2.3 ADR-040 删除 hello_config 路径

ADR-040 Phase 1 清理死代码时删除了 gRPC server + `connect_gateway_client`，但**没有意识到 UserProfile 也跟着一起没了**。`resource_pusher.rs::push_user_profile` 退化为 no-op stub，`users_api.rs` 的 4 处调用全部变成空操作。

### 2.4 链路完全断裂

| 环节 | gRPC 时代 | ADR-040 之后 |
|------|----------|-------------|
| Gateway 持久化 UserProfile | ✅ `user_profiles.json` | ✅ 仍写 |
| Gateway HTTP CRUD | ✅ `/api/users/*` | ✅ 仍能用 |
| Gateway → Runtime 下发 | ✅ `hello_config` | ❌ **断了** |
| Runtime `identity_context` | ✅ 启动时填好 | ❌ **始终 None** |
| Compaction language hint | ✅ 按 profile 语言 | ❌ 永远默认英文 |

### 2.5 设计漏洞诊断

| 维度 | 状态 | 证据 |
|------|------|------|
| mqtt.md §3.1.1 列出 user_profile | ❌ | `docs/protocols/zh/mqtt.md:117-163` 只列 5 种 |
| mqtt.md §7.4 矩阵有 user_profile 行 | ❌ | `docs/protocols/zh/mqtt.md:717-748` 26 行没有 |
| mqtt_payload.proto 有 `AvailableUsers` | ❌ | `core/acowork-core/proto/mqtt_payload.proto:67-108` 5 个，无 user |
| Gateway `MqttGlobalResourcesPublisher` 发 user_profile | ❌ | `topics::USER_PROFILE` 不存在 |
| Gateway `users_api.rs` 触发 publisher | ❌ | 4 处都调 `state.pusher.push_user_profile()`（no-op stub） |
| Runtime `AvailableResourceCache` 缓存 user_profile | ❌ | `core/acowork-runtime/src/mqtt/available_cache.rs:24-30` 只 5 字段 |
| Runtime `agent_init.rs` 构造 identity_context | ❌ | 第 571 行硬编码 `None` |

---

## 3. 决策

### 3.1 新增 MQTT 主题

```
acowork/global/
├── ...现有 5 种资源...
└── user_profile            # [Retained, QoS 1] 当前 active user 的 profile 快照
                            # payload = AvailableUsers {
                            #   version: u64,        // 镜像 user_profile_list.version
                            #   active_user: UserProfileRef {  // 空 = 无 active user
                            #     user_id, display_name, language, timezone,
                            #     city?, country?, occupation?, communication_style?,
                            #     custom_json,
                            #   },
                            # }
```

**Owner**：Gateway（数据源权威）。Gateway 后台 publisher loop 检测到 `user_profile_list.version` 变化或 active user 切换时重算 payload 并 PUBLISH retain=true。

**订阅者**：所有 Runtime（`SUB acowork/global/#` 已在 `client.rs:455`，无需新增订阅）。

**QoS**：1（状态变更不能丢，与其他 §3.1.1 主题一致）。

### 3.2 Payload 设计

```protobuf
message AvailableUsers {
  uint64 version = 1;                // 镜像 user_profile_list.version
  UserProfileRef active_user = 2;    // 空 = 无 active user
}

message UserProfileRef {
  string user_id = 1;
  string display_name = 2;
  string language = 3;               // BCP 47 (e.g. "zh-CN", "en-US")
  string timezone = 4;               // IANA (e.g. "Asia/Shanghai", "UTC")
  optional string city = 5;
  optional string country = 6;
  optional string occupation = 7;
  optional string communication_style = 8;
  string custom_json = 9;            // HashMap<String, String> 序列化为 JSON
}
```

**字段裁剪理由**：
- `avatar` / `builtin_avatar`：纯 UI 渲染字段，Runtime 不需要
- `created_at` / `updated_at`：管理 UI 用，Runtime 不需要
- `is_active`：subscribed topic 只下发 active，所以该字段在 wire 上恒为 true，省略
- `custom`：序列化为 JSON 字符串，避免 protobuf 嵌入 map<string,string> 增加复杂度

### 3.3 Runtime 端启动等待

Runtime 启动后，identity context 构建走三步：

```
1. SUBSCRIBE acowork/global/# (ADR-039 bootstrap 已有)
2. 等待 acowork/global/user_profile retained 到达（≤ 5s timeout）
3. 拿到后调 format_user_profile_context() → identity_context
```

**Timeout 处理**：5s 内未收到（Gateway 还没起、还没装 profile 等场景），fallback 到 `identity_context = None`（向后兼容当前行为）。`acowork/global/user_profile` retained 后续到达会通过 `SessionMessage::UpdateIdentityContext` 自动 broadcast 给所有活跃 session（详见 §3.4）。

### 3.4 运行期 hot-push 路由

```
Gateway PUBLISH acowork/global/user_profile (retain=true)
  │
  ▼
Runtime MQTT event loop
  │ 解码 AvailableUsers → 取 active_user
  │ 调 format_user_profile_context() → identity_context: Option<String>
  ▼
SessionManager.update_user_identity(profile)
  │
  │ broadcast 到所有 session 的 ContextBuilder
  ▼
SessionMessage::UpdateIdentityContext { identity_context }
  │
  ▼
session_task.rs:1368 处理：context_builder + session.identity_context 同步
```

复用现有 `UpdateIdentityContext` 路由（`session_task.rs:108-109` + `session_manager.rs:1740-1746`），零侵入。

### 3.5 改造范围

| 文件 | 改动 |
|------|------|
| `core/acowork-core/proto/mqtt_payload.proto` | 新增 `AvailableUsers` / `UserProfileRef` message + `DataEnvelope.payload` 加 `available_users` 字段（field 15，保留 10-14 给现有 5 种）|
| `docs/protocols/zh/mqtt.md` §3.1.1 | 树状图加 `user_profile` 一行 |
| `docs/protocols/zh/mqtt.md` §7.4 | 矩阵加一行 |
| `core/acowork-gateway/src/mqtt/global_resources_publisher.rs` | `topics::USER_PROFILE` 常量 + `publish_user_profiles()` + `build_available_users()` + `publish_all()` 调用 |
| `core/acowork-gateway/src/http/users_api.rs` | 4 处 `state.pusher.push_user_profile()` 改为 `state.mqtt_publisher_trigger.trigger()` |
| `core/acowork-runtime/src/mqtt/available_cache.rs` | `user_profile: Option<AvailableUsers>` 字段 + `update_from_mqtt` 解析分支 |
| `core/acowork-runtime/src/startup/agent_init.rs` | 删掉硬编码 `None`，改为"等 cache 5s → 取 active user → 构造 identity_context" |
| `core/acowork-runtime/src/mqtt/client.rs` | MQTT event loop 检测到 `acowork/global/user_profile` 时调 `session_manager.update_user_identity()` |
| `core/acowork-runtime/src/agent/session/session_manager.rs` | `update_user_identity` 已经存在，无需改动（已支持 `Option<UserProfile>`）|

`session_task.rs:1368` 的 `UpdateIdentityContext` 处理逻辑**无需改动** —— 已经走完整 broadcast 流程（line 1740-1746 的 `for handle in self.sessions.values()` 已经在 broadcast）。

---

## 4. 评估的备选方案

### 4.1 方案 B：HTTP late-bind via shared cache（已否决）

Runtime 启动后主动 `GET /api/users/active` 拉一次 profile。

**否决理由**：
- 违反 §3.5 "按数据源 pub/sub" 原则 —— UserProfile 既是低频变化的"权威快照"（适合 retained），又不是真正的"全量列表"
- 启动时多一个失败模式（Gateway 未启动、网络问题）
- 后续 hot-push 还是要走别的路径（如 `users_api.rs:267` 的 update_active），分裂

### 4.2 方案 C：把 UserProfile 塞进 `agents/{id}/config`（已否决）

把 user_identity 塞进 Runtime 自己持有的 `agent_config.json`。

**否决理由**：
- 违背数据所有权：UserProfile 是用户级数据（不是 agent 级），不应该散在每个 agent 的 config
- 多用户切换场景完全失效
- 引入 cross-agent 同步问题

---

## 5. 验证

### 5.1 单元测试

- `MqttGlobalResourcesPublisher::test_publisher_publishes_retained_snapshot` 已覆盖 `acowork/global/#` 通配符订阅，新增 `assert!(received_topics.contains(&"acowork/global/user_profile"))`
- `AvailableResourceCache::test_update_from_mqtt_providers` 模式扩展，新增 `test_update_from_mqtt_user_profile`

### 5.2 集成测试

- 手动压缩带 CJK 的对话，期望输出**中文摘要**（不再是英文）
- Desktop Settings 切换 active profile，期望 Runtime 立刻收到新 profile（不需要重启 agent）

### 5.3 回归

- 单用户 + 无 profile 场景：5s timeout 后 identity_context=None，compaction 走对话语言检测（v6 prompt），行为不退化
- 多 Runtime 场景：每个 Runtime 各自收到 retained，独立缓存，互不影响
- 重连场景：MQTT reconnect 触发 `run_bootstrap()` 重做 §3.1.1 订阅，retained 自动重新投递

---

## 6. ADR 范围外（明确不做）

- 多用户 ACL 隔离 —— §10 `acowork/users/{user_id}/` 已预留，不在本 ADR 处理
- Runtime → Gateway 反向写 UserProfile —— Runtime 永远只读 UserProfile，CRUD 走 Desktop HTTP
- Avatar / builtin_avatar 下发 —— 仅 UI 用，没必要进 protocol buffer
- 兼容老格式（如 gRPC 残留的 `PushUserProfile` payload）—— ADR-040 已删除，不保留

---

## 7. 后续追踪项

- [ ] 测试 `compaction` 在 active profile = None 时的 fallback 行为（v6 prompt "对话模糊时 fallback 到 identity"）
- [ ] ADR-033 §3.4 多用户主题激活时，user_profile 主题是否需要变 `acowork/users/{uid}/profile`？还是保持全局？取决于多用户阶段运行时每个 Runtime 看到的是哪个 user 的视角