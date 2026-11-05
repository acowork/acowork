# 通信协议

> 版本：v3.7 | 更新日期：2026-07-12

> **v3.7 变更**：§0/§1/§1.5 全面对齐 [ADR-033](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) —— Gateway ↔ Runtime IPC 通道由 gRPC 双向流替换为 **MQTT pub/sub + HTTP 反向代理**。Intent 协议（§2）保持不变，仍是 Agent 间逻辑通信的语义合同（与传输无关）。
>
> **v3.6 变更**：新增 §1.5 Session 管理 IPC 消息 Proto 定义，支持 Session Actor 多会话并发模型。Session 相关 Gateway ↔ Runtime IPC 消息现在承载在 MQTT 主题 `acowork/sessions/...` 上，由 Gateway 反向代理 Runtime 的 localhost HTTP 处理 Session CRUD / 消息历史查询（见 [`docs/protocols/zh/mqtt.md`](../../zh/protocols/mqtt.md)、[`docs/protocols/zh/http.md`](../../zh/protocols/http.md)）。

**交叉引用**：
- Session Actor 架构：`15-conversation-persistence.md` §1.7
- Agent 运行时主循环：`03-agent-runtime.md` §2
- Session 生命周期与 JSONL 安全：`15-conversation-persistence.md` §1.5、§1.9

---

## 0. 通信架构总览

自 [ADR-033](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) 起，ACowork 平台有四条独立的通信通道，各司其职：

```
┌────────────────┐         ┌────────────────┐         ┌────────────────┐
│  Desktop App   │         │  Agent Runtime │         │  Agent Runtime │
│  (Tauri v2)    │         │  (Agent A)     │         │  (Agent B)     │
└───────┬────────┘         └───────┬────────┘         └───────┬────────┘
        │                          │                          │
        │ HTTP REST                │ MQTT + HTTP 反向代理     │
        │ + MQTT SUB               │ (rumqttc + localhost     │
        │                          │  HTTP server)            │
        ▼                          ▼                          ▼
┌──────────────────────────────────────────────────────────────────────┐
│                         Gateway (单进程)                              │
│                                                                      │
│  HTTP API (Axum)    MQTT Broker (rumqttd)   HTTP Reverse Proxy        │
│  ────────────────   ─────────────────────   ─────────────────────    │
│  REST :19876        :19875                   → Runtime localhost      │
│  Desktop / CLI      实时事件 / 状态           大数据查询 / 历史消息     │
│  CRUD / 对话 / 配置  chunk / tool_call / done  反代后端 (不解析 body)   │
│                                                                      │
│  Intent Router (转发 / 订阅) · Package / Lifecycle · Vault · Config  │
└──────────────────────────────────────────────────────────────────────┘
        ▲                          ▲                          ▲
        │                          │                          │
        │                          │ Intent 主题转发           │
        └──────────────────────────┼──────────────────────────┘
```

| 通道 | 消费者 | 协议 | 用途 |
|------|--------|------|------|
| **HTTP API** | Desktop App / CLI / Gateway → Runtime 反代 | REST (`http://127.0.0.1:19876`) | Agent 管理、对话触发、Vault、配置、大数据查询反代 |
| **MQTT** | Agent Runtime ↔ Gateway ↔ Desktop App | MQTT 3.1.1 over TCP (`127.0.0.1:19875`, rumqttd broker) | 实时事件推送（chat chunk / tool_call / done）、状态同步、设备生命周期（Will+Retained）、Intent 主题路由 |
| **HTTP 反向代理** | Gateway → Agent Runtime | HTTP（Runtime 自身监听 localhost 随机端口）| 会话历史拉取、消息分页查询、配置回写、AgentHello 等需要"等回复"的场景 |
| **Debug Protocol** | Desktop App (DevMode) | HTTP RPC（Gateway 反代 → Runtime `/api/debug/*`）+ MQTT 调试事件（`acowork/agents/{id}/debug/events/{type}`） | 步进调试、录制回放、Skill 热加载（ADR-048 后与生产 IPC 完全同构）|

**权威参考：**
- MQTT 主题树与 payload protobuf：[`docs/protocols/zh/mqtt.md`](../../zh/protocols/mqtt.md)
- HTTP REST：[`docs/protocols/zh/http.md`](../../zh/protocols/http.md)
- Gateway HTTP 反代：[`docs/design/zh/04-gateway.md`](./04-gateway.md) §9
- Debug Protocol：[`10-debug-protocol.md`](./10-debug-protocol.md)

**§1（Gateway Service API / gRPC）已整体退役** —— 旧 gRPC 合同层握手、帧格式、GatewayRequest/Response 枚举全部下线，相应职责拆分为 MQTT（事件）和 HTTP 反代（请求/响应）。历史合同定义仍可在 [`16-ipc-grpc-migration.md`](./16-ipc-grpc-migration.md) 查阅。

**§2（跨 Agent 通信 / Intent 机制）与传输无关** —— Intent 消息结构、Capability Registry、路由流程等仍是平台语义合同，载荷直接落到 MQTT 主题 `acowork/intents/...` 上，传输层不参与语义。

## 1. Gateway Service API（已退役 — 拆分为 MQTT + HTTP 反向代理）

> **ADR-033（2026-07-11）** 起，§1 描述的 gRPC 双向流 API 整体退役。原 §1.1–1.4 中的所有握手、帧格式、GatewayRequest/Response 枚举由以下两层承担：
>
> - **MQTT**（[`docs/protocols/zh/mqtt.md`](../../zh/protocols/mqtt.md)）— 实时事件、状态推送、设备生命周期（Will+Retained）、Key 分发（AgentHello 握手响应）
> - **HTTP 反向代理**（[`docs/design/zh/04-gateway.md`](./04-gateway.md) §9 + [`docs/protocols/zh/http.md`](../../zh/protocols/http.md)）— 会话历史、消息分页、Intent 触发、配置写回等"等回复"场景
>
> 旧合同层的完整记录（`16-ipc-grpc-migration.md`）保留作为历史参考；现有集成方请直接读上述两份现行协议文档。
>
> 之前 §1 的"合同层 vs 实现层"分层（消息格式 / JSON schema / 握手协议 vs 传输方式）不再适用 —— 现在合同层是 **MQTT 主题树 + mqtt_payload.proto + HTTP REST 路由**，没有"两套实现层"。

## 2. 跨 Agent 通信（Intent 机制）

Agent 通过 Gateway 的 Intent Router 发送消息请求调用另一个 Agent 的能力。

### 1.5 Session 管理 IPC 消息

> v3.6 新增（2026-05-06）：支持 Session Actor 多会话并发模型。
> v3.7 修订：传输层从 gRPC Bidirectional Stream 迁移到 **MQTT 主题 + HTTP 反向代理**；proto 定义仍有效，但落地路径变了。

Session 相关的 Gateway ↔ Runtime IPC 消息拆为两类传输：

| 类别 | 承载通道 | 触发场景 | 消息类型 |
|------|---------|---------|---------|
| **Session 事件流** | MQTT 主题 `acowork/sessions/{sid}/events/#` | Gateway 转发 Runtime 的 chat chunk / tool_call / done 等流式事件 | 见 [`docs/protocols/zh/mqtt.md`](../../zh/protocols/mqtt.md) §Session events |
| **Session 查询/管理** | HTTP 反向代理（Gateway → Runtime localhost server `/sessions/...`） | 列表查询、最新会话、单会话消息分页、CRUD | `SessionList` / `ConversationMessages` / `CreateSessionResponse` / `ActivateSessionResponse` / `DeleteSessionResponse`（proto 定义保留，但承载通道从 gRPC 切到 HTTP 反代）|

#### Proto 定义

```protobuf
// session.proto — Session 管理消息格式

// Runtime → Gateway：Session 列表查询响应
message SessionList {
    repeated SessionInfo sessions = 1;
}

message SessionInfo {
    string session_id = 1;
    string title = 2;
    int64 message_count = 3;
    string status = 4;  // "active" | "idle" | "ended"
    int64 created_at = 5;     // Unix timestamp ms
    int64 last_active_at = 6; // Unix timestamp ms
}

// Runtime → Gateway：Session 消息查询响应
message ConversationMessages {
    repeated ConversationLine lines = 1;
    bool has_more = 2;
    optional string cursor = 3;     // 下一批的起始行号
}

message ConversationLine {
    string id = 1;
    string role = 2;    // "user" | "assistant" | "tool_call" | "tool_result" | "think" | "system"
    string content = 3;
    map<string, string> metadata = 4;  // tool_name, tool_call_id, model 等
    int64 ts = 5;                       // Unix timestamp ms
}


// Runtime → Gateway：创建 Session 响应
message CreateSessionResponse {
    string session_id = 1;
    bool success = 2;
    optional string error = 3;
}

// Runtime → Gateway：激活 Session 响应
message ActivateSessionResponse {
    string session_id = 1;
    bool success = 2;
    optional string error = 3;
}

// Runtime → Gateway：删除 Session 响应
message DeleteSessionResponse {
    bool success = 1;
    optional string error = 2;
}
```

#### 与 MQTT/HTTP 反向代理的对应关系（v3.7 修订）


| 旧消息类型 | 新通道 | 协议 | 用途 |
|-----------|--------|------|------|
| `GatewayRequest` / `Response`（Key, Intent, Budget, Rate, Permission） | MQTT 主题 + HTTP 反代 | MQTT 3.1.1 / HTTP | Intent 触发走 MQTT 主题 `acowork/intents/...`；Budget / Rate 查询走 HTTP 反代 Runtime `/budget/...` `/rate/...` |
| `SessionList` / `ConversationMessages` / CRUD 响应 | HTTP 反向代理 → Runtime `/sessions/...` | HTTP/JSON | Session CRUD、消息查询、消息分页 |
| `IntentReceived`（含 action=chat_message）/ Chat 流式事件 | MQTT 主题 `acowork/agents/{id}/sessions/{sid}/messages/#` | MQTT 3.1.1（protobuf payload）| **Chat 消息与流式 chunk** |


**为什么 Session 事件流用 MQTT、Session CRUD 用 HTTP 反代**：
- Session 流式事件（chat chunk / tool_call / done）是典型的"一对多广播 + 多订阅方"模式，pub/sub 比请求/响应天然契合
- Session 查询/CRUD 需要返回大量结构化数据（历史消息列表），走 HTTP 反向代理更直接（请求/响应 + 分页 + cursor）
- Runtime 自己暴露 localhost HTTP server（随机端口），只对 Gateway 可见，不对外开端口

#### 未知 role 类型的降级处理

当前 `ConversationLine.role` 支持：
- `user` / `assistant` / `tool_call` / `tool_result` / `think` / `system`
- 未来可能扩展（如 `system_message` / `error`）

**降级规则**：
```
读到未知 role → 当 "system" 类型处理
前端行为：展示为普通消息（content 正常显示）
向后兼容：新 role 类型出现时不会破坏已有 Session 的读取
```

### 2.1 Intent 消息格式

#### 消息结构

```rust
/// Intent 消息——Agent 间通信的标准信封
struct IntentMessage {
    /// 消息类型标识，固定为 "intent"
    r#type: String,                    // "intent"

    /// 目标 Agent ID（必填，显式指定）
    /// ACowork 不支持隐式 Intent，target 必须是已安装 Agent 的 agent_id
    target: String,

    /// 请求的动作名称（必填）
    /// 必须匹配目标 Agent manifest 中声明的 capability action
    action: String,

    /// 动作参数（必填，至少为 {}）
    /// 结构必须匹配目标 Agent capability 声明的 input schema
    params: serde_json::Value,

    /// 调用模式
    /// true = 异步（发送即忘，结果通过 callback 查询）
    /// false = 同步（阻塞等待结果，超时由 timeout_ms 控制）
    async_: bool,

    /// 消息唯一标识（Gateway 生成）
    /// 用于关联请求与响应、追踪投递状态
    id: String,

    /// 发送方 Agent ID（Gateway 自动填充，Agent 不可伪造）
    from: String,

    /// 同步模式超时（毫秒，可选，默认 30000）
    /// 仅 async_=false 时有效。超时后 Gateway 返回超时错误
    timeout_ms: Option<u64>,

    /// 响应模式（可选，默认 "direct"）
    /// "direct" = 结果直接返回给调用方
    /// "callback" = 结果通过目标 Agent 的 callback_intent 推送（用于 observe 模式）
    response_type: Option<String>,
}

/// Intent 响应——目标 Agent 处理后返回的结果
struct IntentResponse {
    /// 消息类型标识
    r#type: String,                    // "intent_response"

    /// 原始 Intent 的消息 ID
    request_id: String,

    /// 响应状态
    status: IntentStatus,

    /// 结果数据（成功时填充）
    /// 结构应匹配目标 Agent capability 声明的 output schema
    result: Option<serde_json::Value>,

    /// 错误信息（失败时填充）
    error: Option<IntentError>,
}

enum IntentStatus {
    /// 处理成功
    Ok,
    /// 目标 Agent 处理失败
    Error,
    /// 目标 Agent 未安装
    AgentNotFound,
    /// 目标 Agent 启动失败
    AgentStartFailed,
    /// 同步等待超时
    Timeout,
    /// 目标 Agent 的 capability 不匹配（action 不存在）
    CapabilityNotFound,
    /// 参数校验失败
    InvalidParams,
    /// 发送方缺少 intent:send 权限
    PermissionDenied,
}

struct IntentError {
    /// 错误码（机器可读）
    code: String,                      // "AGENT_NOT_FOUND", "TIMEOUT", etc.
    /// 错误描述（人类可读）
    message: String,
}
```

#### JSON 示例

**同步 Intent（请求 + 响应）**：

```json
// 请求
{
    "type": "intent",
    "target": "com.example.calendar",
    "action": "create_event",
    "params": {"title": "Meeting", "time": "2026-01-01T10:00Z"},
    "async": false,
    "id": "msg-456",
    "from": "com.example.assistant",
    "timeout_ms": 10000,
    "response_type": "direct"
}

// 成功响应
{
    "type": "intent_response",
    "request_id": "msg-456",
    "status": "ok",
    "result": {"event_id": "evt-789", "status": "created"},
    "error": null
}

// 失败响应（目标 Agent 未安装）
{
    "type": "intent_response",
    "request_id": "msg-456",
    "status": "agent_not_found",
    "result": null,
    "error": {
        "code": "AGENT_NOT_FOUND",
        "message": "Agent com.example.calendar is not installed"
    }
}
```

**异步 Intent**：

> **⚠️ 已废弃** — 以下 identity:observe / identity:changed 示例已不再适用。
> 用户身份管理已迁移至 Gateway UserProfile，通过 UserProfileUpdate 推送。
> 详见 `18-user-identity-simplified.md`。

<details>
<summary>历史 identity Intent 示例（点击展开）</summary>

```json
// 请求
{
    "type": "intent",
    "target": "com.acowork.system",
    "action": "identity:observe",
    "params": {"fields": ["city"], "callback_intent": "com.example.weather"},
    "async": true,
    "id": "msg-789",
    "from": "com.example.weather"
}

// 即时响应（仅确认投递成功）
{
    "type": "intent_response",
    "request_id": "msg-789",
    "status": "ok",
    "result": {"subscribed": true},
    "error": null
}

// 后续变更通知（系统 Agent 推送）
{
    "type": "notification",
    "from": "com.acowork.system",
    "action": "identity:changed",
    "params": {"field": "city", "old_value": "Beijing", "new_value": "Shanghai"}
}
```

</details>



```json
// 订阅请求
{
    "type": "intent",
    "target": "com.acowork.system",
    "action": "identity:observe",
    "params": {"fields": ["city"], "callback_intent": "com.example.weather"},
    "async": true,
    "id": "msg-101",
    "from": "com.example.weather",
    "response_type": "callback"
}
```

#### 字段安全说明

- `from` 字段由 Gateway 在收到 Agent 的 IntentSend 请求后自动填充，Agent 不可自行指定。这防止了身份伪造攻击。
- `id` 由 Gateway 生成（UUID v4），确保全局唯一。Agent 在 IntentSend 请求中无需提供 id。
- `params` 的大小限制为 64 KB（超过时 Gateway 拒绝转发），防止大 payload 攻击。

### 2.2 Capability Registry

#### 设计原则

ACowork 不支持隐式 Intent，所有 Intent 调用必须显式指定 `target`（Agent ID）。因此 Capability Registry 只需回答一个问题：**这个 Agent 声明了这个 Action 吗？** 无需 priority 机制，无路由 ambiguity。

#### 数据结构

**单 HashMap，`"{agent_id}:{action}"` 作为 Key**

```rust
pub struct CapabilityRegistry {
    // Key: "com.weather.app:weather:query"
    // Value: CapabilityDef { version, params, description }
    capabilities: HashMap<String, CapabilityDef>,
}
```

#### 用途覆盖

| 用途 | 实现 | 复杂度 |
|------|------|--------|
| **安装依赖检查** | `capabilities.get("{}:{}".format(requires_agent, requires_action))` | O(1) |
| **运行时校验**（可选） | `capabilities.get("{}:{}".format(target, action))` | O(1) |
| **Agent 能力查询** | `capabilities.iter().filter(|(k, _)| k.starts_with("{}:", agent_id))` | O(n) |

Agent 数量有限，O(n) 全量扫描完全可接受。

#### 安装 / 卸载 / 重启

**安装时：**

```
1. 解析 manifest.capabilities
2. 依赖检查：遍历 manifest.requires，逐一查 Registry
   - 存在 → 继续
   - 不存在 → 返回错误 "requires '{agent}:{action}' not found"
3. 写入 Registry：
   for each (action, def) in capabilities:
       capabilities.insert("{}:{}".format(agent_id, action), def)
```

**卸载时：**

```
capabilities.retain(|k, _| !k.starts_with("{}:", agent_id))
```

**Gateway 重启：**

扫描 `~/.local/share/agent-gateway/agents/` 下所有已安装 agent 的 manifest 重建索引。

#### manifest 中 capabilities 声明格式

```json
{
  "capabilities": {
    "weather:query": {
      "input": {"city": "string", "date": "date?"},
      "output": {"temperature": "number", "condition": "string"},
      "description": "查询城市天气"
    },
    "weather:alert": {
      "input": {"city": "string"},
      "output": {"alert_level": "string"},
      "description": "获取天气预警"
    }
  }
}
```

#### 三个用途

1. **安装时依赖检查**：Agent 声明 `requires` 指向其他 Agent 的 capability，Gateway 校验这些 capability 是否已注册。
2. **运行时校验**（可选）：Agent A 向 Agent B 发送 Intent 时，Gateway 可校验 `target:action` 是否在 Registry 中。
3. **运行时查询**：Agent 可通过 `CapabilityQuery` 接口查询其他 Agent 的详细能力（见 2.4 节）。

### 2.3 Intent 路由流程

1. Agent A 通过 Socket 发送 Intent 到 Gateway。
2. Gateway 查找 target Agent B：
   - 若 B 已安装但未运行 → 按 B 的启动策略决定是否拉起（见下方）。
   - 若 B 未安装 → 返回错误 `"Agent not found"`。
3. Gateway 校验 Intent 的 action 和参数是否匹配 B 的 capability 声明。
4. Gateway 将 Intent 转发给 Agent B。
5. Agent B 处理后返回结果。
6. Gateway 将结果返回给 Agent A（同步模式）或缓存等待 Agent A 下次查询（异步模式）。

**目标 Agent 未运行时的启动策略：**

| 场景 | 行为 |
|------|------|
| 同步 Intent + B 未运行 | Gateway 拉起 B，A 阻塞等待（超时由 A 设置） |
| 异步 Intent + B 未运行 | Gateway 拉起 B，A 不阻塞，B 处理完后 Gateway 缓存结果 |
| B 启动失败 | Gateway 返回错误 `"Agent failed to start"` |
| B 的启动策略为"按需" | 正常拉起 |
| B 的 manifest 禁止被 Intent 唤醒 | Gateway 返回错误 `"Agent does not accept intents"` |

### 2.4 Capability 查询机制

Agent 获取其他 Agent 的能力列表有两种途径：

#### 途径 1：启动时注入（Capability Overview）

Agent Runtime 握手时，Gateway 主动推送当前已安装的所有 Agent 及其 **名字级能力摘要**（不含 input/output schema）。这是**最常用的方式**——Agent 在构建 prompt 时就知道系统里有哪些 Agent 能做什么，可以直接向 LLM 描述可用的协作能力。

```json
// Gateway 推送的 capability_overview（名字级摘要）
{
    "type": "capability_overview",
    "agents": [
        {
            "agent_id": "com.acowork.system",
            "running": true,
            "capabilities": []
        },
        {
            "agent_id": "com.example.calendar",
            "running": false,
            "capabilities": ["create_event", "query_events", "delete_event"]
        },
        {
            "agent_id": "com.example.todo",
            "running": false,
            "capabilities": ["create_task", "list_tasks"]
        }
    ]
}
```

**为什么只推名字级摘要？**

如果装了 50 个 Agent，每个 5 个 capability 的完整 schema，推送到 prompt 中约需 6000-8000 token——严重挤占上下文空间，稀释 LLM 对核心指令的注意力。名字级摘要同样规模只有 500-800 token，对 LLM 来说已足够做规划决策（"日历 Agent 能 create_event"就够了），精确参数在调用前按需查询即可。

**推送内容说明：**

- 只包含 capability 名称列表，不含 input/output schema。
- `running` 字段表示当前是否在运行，供 Agent 判断调用延迟预期。
- 总量控制在 1000 token 以内。如果已安装 Agent 过多导致超限，Gateway 按以下策略裁剪：
  1. 优先保留 `running: true` 的 Agent
  2. 其次保留本 Agent 在 manifest 中声明了 intent 依赖的 Agent
  3. 其余按 agent_id 字母序截断

**调用时按需获取详细 schema 的流程：**

```
LLM 决定调用日历 Agent 的 create_event
       │
       ▼
Agent Runtime 发送 CapabilityQuery { target: "com.example.calendar" }
       │
       ▼
Gateway 返回完整 capabilities（含 input/output schema）
       │
       ▼
Agent Runtime 将 schema 注入当前迭代的上下文，LLM 据此构造精确参数
       │
       ▼
LLM 输出 tool_call: intent_send(target="com.example.calendar", action="create_event", params={...})
```

#### 途径 2：运行时查询（CapabilityQuery）

当 Agent 需要精确确认某 Agent 当前状态或获取完整 capability schema 时，主动查询 Gateway：

```json
// 查询指定 Agent
{ "type": "capability_query", "target": "com.example.calendar" }

// 查询所有已安装 Agent
{ "type": "capability_query", "target": null }
```

Gateway 返回完整信息：

```json
{
    "type": "capability_query_result",
    "agents": [
        {
            "agent_id": "com.example.calendar",
            "running": true,
            "capabilities": {
                "create_event": {
                    "input": {"title": "string", "time": "datetime", "remind_before": "duration?"},
                    "output": {"event_id": "string", "status": "created|failed"}
                },
                "query_events": {
                    "input": {"from": "date", "to": "date"},
                    "output": {"events": "array<Event>"}
                }
            }
        }
    ]
}
```

**两种途径的适用场景：**

| 场景 | 途径 | 理由 |
|------|------|------|
| 构建系统 prompt 时描述可用协作能力 | 途径 1（启动注入） | 一次性获取，不需要额外通信 |
| 发送 Intent 前确认目标 Agent 是否安装 | 途径 1 | overview 已包含安装信息 |
| 需要完整的 input/output schema | 途径 2（运行时查询） | overview 只含概要信息 |
| 用户安装/卸载 Agent 后刷新能力视图 | 途径 2 | 启动时的 overview 可能已过期 |

#### 途径 1 的更新机制

启动时推送的 overview 是快照，不会随安装/卸载自动更新。Gateway 在检测到以下变更时，主动推送增量更新：

```json
// 新 Agent 安装
{
    "type": "capability_update",
    "action": "installed",
    "agent": {
        "agent_id": "com.example.todo",
        "running": false,
        "capabilities": ["create_task", "list_tasks"]
    }
}

// Agent 卸载
{
    "type": "capability_update",
    "action": "uninstalled",
    "agent_id": "com.example.todo"
}

// Agent 更新（capabilities 变化）
{
    "type": "capability_update",
    "action": "updated",
    "agent": {
        "agent_id": "com.example.calendar",
        "running": false,
        "capabilities": ["create_event", "query_events", "delete_event", "share_calendar"]
    }
}
```

Agent Runtime 收到更新后，刷新本地缓存的 capability 视图，并在下一轮迭代的上下文构建中反映变化。

### 2.5 WorkspaceContextUpdate（工作区上下文更新）

Gateway → Runtime 单播推送。携带已格式化的工作区上下文文本，用于注入 LLM 的 System Prompt。

```json
{
  "type": "workspace_context_update",
  "context_text": "## Workspace Environment\n...",
  "current_workspace_id": "ws-abc123",
  "current_workspace_path": "/home/user/projects/my-project"
}
```

**触发时机**：
- Agent 会话启动时主动推送
- 用户通过 Desktop App 切换当前工作区时推送

## 3. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| IPC 通道收敛（ADR-033） | 统一为 MQTT pub/sub + HTTP 反向代理 | 见 ADR-033 / ADR-034：MQTT 承担事件与状态、HTTP 反代承担 req/res 与历史查询；淘汰 gRPC 双向流与 WebSocket 流式推送 |
| Capability 发现方式 | 启动时注入 + 运行时查询 | 启动注入满足常见需求（构建 prompt），运行时查询满足精确需求 |
| Overview 内容粒度 | 名字级摘要（不含 schema） | 50 Agent × 5 capability 完整 schema 约 6000-8000 token，名字级仅 500-800 token |
| 精确参数获取 | 调用前 CapabilityQuery 按需查询 | LLM 规划只需知道"谁会什么"，精确 schema 在执行时才需要 |
| Overview 推送 vs 拉取 | 推送 | Agent 在启动时就需要知道协作环境，拉取需额外通信 |
| 安装/卸载后的更新 | Gateway 主动推送增量 | 避免 Agent 轮询，减少不必要的通信 |
| Overview 超限裁剪策略 | 保留 running + intent 依赖 + 字母序截断 | 确保最相关的 Agent 信息不丢失 |
| Intent 目标未运行 | Gateway 按需拉起 | Agent 开发者无需关心目标 Agent 的运行状态 |
