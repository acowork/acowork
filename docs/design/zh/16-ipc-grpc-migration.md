# 16. IPC 通信层 gRPC 迁移设计（历史文档）

> 版本：v1.0 | 日期：2026-05-03
> **状态：历史文档** —— 记录 v3.1 的 gRPC 迁移历程。v3.2 已进一步迁移到 MQTT + HTTP 反向代理
> （见 [ADR-033](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md)）；当前通信协议见
> [06-communication.md](./06-communication.md)。

---

## 1. 概述

### 1.1 背景与动机

当前 Gateway 与 Agent Runtime 的 IPC 通信采用**自定义二进制帧协议**：5 字节定长头（`[body_len: u32 BE][msg_type: u8]`）+ JSON body。该协议在实现上存在以下结构性问题：

| 问题                             | 根因                                                                                                                              | 后果                                                                                                                    |
| -------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| **帧交错（Frame Interleaving）** | 单条连接上混用请求-响应、Server Push、Streaming 三种模式，且共用一个 `msg_type` 命名空间（0=request, 1=response, 2=stream_chunk） | Gateway 的 `tokio::select!` 可能在响应帧之前发送推送帧，Runtime 的 `send_and_recv()` 收到错误消息类型，导致协议握手失败 |
| **缺乏 Multiplexing**            | 无 `request_id` 字段，请求与响应仅靠发送顺序关联                                                                                  | 无法支持并发请求；任何乱序都会导致响应错配                                                                              |
| **单帧类型复用**                 | `TYPE_RESPONSE`（msg_type=1）同时承载正常响应和 Server Push                                                                       | 需要 `is_push_message()` 硬编码判别，易遗漏新变体                                                                       |
| **跨平台维护成本**               | 每新增一种消息，需同步修改帧编解码、枚举判别、推送缓冲逻辑                                                                        | 协议扩展成本高，容易引入回归 bug                                                                                        |
| **无内建流量控制**               | 自定义协议缺少背压机制                                                                                                            | 高频率 Stream Chunk 可能压垮接收方                                                                                      |

**为什么选择 gRPC：**

- **HTTP/2 多路复用**：天然支持同一连接上的并发流，彻底消除帧交错
- **双向流（Bidirectional Streaming）**：一个 `rpc Connect(stream ClientMessage) returns (stream ServerMessage)` 即可覆盖请求-响应、Server Push、Streaming 三种模式
- **protobuf 强类型**：编译期检查消息 schema，避免 JSON 反序列化失败
- **成熟的 Rust 生态**：`tonic` + `prost` 提供完整的异步 gRPC 实现
- **内建流量控制**：HTTP/2 窗口机制自动提供背压

### 1.2 目标

1. **根除帧交错问题**：通过 gRPC 流内建的多路复用，使请求-响应与推送消息在逻辑上隔离
2. **跨平台统一 transport**：全平台统一使用 TCP loopback（127.0.0.1）+ gRPC，消除平台差异代码
3. **protobuf 强类型消息合约**：所有 GatewayRequest/GatewayResponse 变体映射为 proto message，编译期校验
4. **支持三种通信模式**：
   - **请求-响应**（KeyRelease、BudgetQuery 等）
   - **Server Push**（IntentReceived、CapabilityUpdate、LLMConfigDelivery 等）
   - **Streaming**（LLM 输出 chunk 流）
5. **直接替换旧协议**：不保留旧自定义帧协议，不维护配置开关，一步到位完成迁移

### 1.3 非目标

- **不改变业务逻辑层**：`handle_key_release`、`handle_intent_send` 等 handler 函数的逻辑保持不动，仅替换其输入输出序列化方式
- **不引入服务网格或分布式部署**：本次迁移聚焦单机 IPC，远程 Gateway 场景不在范围内

> **设计原则**：项目处于开发阶段，不考虑向后兼容，选择最优技术方案直接实施。迁移期间允许 break everything，以最低维护成本换取最干净的架构。

---

## 2. 现有协议分析

### 2.1 帧格式

```
[4 bytes: body length (u32 big-endian)]
[1 byte:  message type (0=request, 1=response, 2=stream_chunk, 3=error)]
[N bytes: JSON body]
```

- **Header 大小**：5 字节固定
- **Body 编码**：`serde_json::to_vec()` 序列化的 JSON 文本
- **消息类型判别**：`msg_type` 字段为 u8，仅区分 request/response/stream/error，不区分具体业务类型

### 2.2 消息类型完整映射

#### GatewayRequest（17 个变体）

| #   | 变体                                                               | 通信模式          | 说明                                             |
| --- | ------------------------------------------------------------------ | ----------------- | ------------------------------------------------ |
| 1   | `KeyRelease { provider }`                                          | req-resp          | 请求 API Key                                     |
| 2   | `IntentSend { target, action, params, async_ }`                    | req-resp / stream | 发送 Intent；stream 模式下使用 TYPE_STREAM_CHUNK |
| 3   | `BudgetQuery { provider }`                                         | req-resp          | 查询剩余预算                                     |
| 4   | `UsageReport(UsageReport)`                                         | req-resp          | 上报 token 用量                                  |
| 5   | `RateAcquire { provider }`                                         | req-resp          | 申请速率令牌                                     |
| 6   | `PermissionRequest { request_id, permission, reason, timeout_ms }` | req-resp          | 运行时权限请求                                   |
| 7   | `IdentityQuery { fields }`                                         | req-resp          | 查询身份字段                                     |
| 8   | `CapabilityQuery { agent_id }`                                     | req-resp          | 查询能力列表                                     |
| 9   | `CronRegister { agent_id, schedule, action, params }`              | req-resp          | 注册定时任务                                     |
| 10  | `CronUnregister { cron_id }`                                       | req-resp          | 注销定时任务                                     |
| 11  | `CronList {}`                                                      | req-resp          | 列出定时任务                                     |
| 12  | `ContextUsageReport { agent_id, context }`                         | req-resp          | 上报上下文用量                                   |
| 13  | `AgentHello { agent_id, version, connection_role }`                | req-resp          | 注册连接                                         |
| 14  | `ListSessions`                                                     | req-resp          | 列出会话                                         |
| 15  | `GetSessionMessages { session_id, cursor, limit, direction }`      | req-resp          | 获取会话消息                                     |
| 16  | `CreateSession`                                                    | req-resp          | 创建会话                                         |
| 17  | `GetCurrentSessionId`                                              | req-resp          | 获取当前会话 ID                                  |

#### GatewayResponse（23 个变体）

| #   | 变体                                                                                                            | 通信模式 | 说明             |
| --- | --------------------------------------------------------------------------------------------------------------- | -------- | ---------------- |
| 1   | `AgentHelloResult { success, error }`                                                                           | req-resp | 注册确认         |
| 2   | `KeyReleaseResult { api_key, error }`                                                                           | req-resp | API Key 返回     |
| 3   | `IntentDelivered { message_id }`                                                                                | req-resp | Intent 投递确认  |
| 4   | `IntentReceived { from, action, params }`                                                                       | **push** | 收到外部 Intent  |
| 5   | `BudgetInfo { remaining_tokens, remaining_cost_usd }`                                                           | req-resp | 预算信息         |
| 6   | `UsageReportAck {}`                                                                                             | req-resp | 用量上报确认     |
| 7   | `ContextUsageAck {}`                                                                                            | req-resp | 上下文用量确认   |
| 8   | `RateToken { granted, retry_after_ms }`                                                                         | req-resp | 速率令牌         |
| 9   | `PermissionResult { request_id, granted, reason }`                                                              | req-resp | 权限结果         |
| 10  | `IdentityDelivery { entries }`                                                                                  | **push** | 身份数据推送     |
| 11  | `LLMConfigDelivery { provider, model, api_key, base_url, models, model_capabilities, max_output_tokens_limit }` | **push** | LLM 配置推送     |
| 12  | `IdentityQueryResult { values, confidence }`                                                                    | req-resp | 身份查询结果     |
| 13  | `CapabilityOverview { capabilities }`                                                                           | req-resp | 能力概览         |
| 14  | `CapabilityUpdate { agent_id, actions, removed }`                                                               | **push** | 能力增量更新     |
| 15  | `CronRegisterResult { cron_id, error }`                                                                         | req-resp | 注册结果         |
| 16  | `CronUnregisterResult { removed }`                                                                              | req-resp | 注销结果         |
| 17  | `CronListResult { entries }`                                                                                    | req-resp | 任务列表         |
| 18  | `WorkspaceContextUpdate { context_text, current_workspace_id, current_workspace_path }`                         | **push** | 工作区上下文推送 |
| 19  | `IterationLimitPaused { iteration, max_iterations, message }`                                                   | **push** | 迭代上限暂停通知 |
| 20  | `SessionList { sessions }`                                                                                      | req-resp | 会话列表         |
| 21  | `SessionMessages { messages, cursor, has_more }`                                                                | req-resp | 会话消息页       |
| 22  | `SessionCreated { session_id }`                                                                                 | req-resp | 会话创建确认     |
| 23  | `CurrentSessionId { session_id }`                                                                               | req-resp | 当前会话 ID      |

### 2.3 已知缺陷

1. **帧交错（Frame Interleaving）**：
   - Gateway `handle_connection` 使用 `tokio::select!` 同时等待：① Agent 请求 ② Server Push ③ CapabilityUpdate 广播
   - 当 Branch 2（Push）或 Branch 3（Broadcast）先就绪时，推送帧会被写入连接，**在** Branch 1 的响应帧之前到达 Runtime
   - Runtime 的 `send_and_recv()` 期望读取响应，却收到 Push 消息，导致协议错误

2. **单帧类型复用**：
   - `TYPE_RESPONSE` 同时承载 17 种响应变体和 6 种推送变体
   - 必须通过 `is_push_message()` 硬编码列表判别，新加推送变体容易遗漏

3. **无消息关联**：
   - 请求与响应仅靠 TCP 顺序保证关联，无法支持并发请求
   - `send_and_recv()` 是阻塞式单线程读取，任何并发需求都需要额外连接

4. **is_push_message 硬编码**：
   ```rust
   fn is_push_message(response: &GatewayResponse) -> bool {
       matches!(response,
           GatewayResponse::IntentReceived { .. }
           | GatewayResponse::CapabilityUpdate { .. }
           | GatewayResponse::LLMConfigDelivery { .. }
           | GatewayResponse::IdentityDelivery { .. }
           | GatewayResponse::WorkspaceContextUpdate { .. }
           | GatewayResponse::IterationLimitPaused { .. }
       )
   }
   ```
   - 列表遗漏会导致推送消息被误当作响应，或响应被误缓冲

---

## 3. gRPC 服务定义

### 3.1 .proto Schema 设计

```protobuf
syntax = "proto3";

package acowork.ipc.v1;

// ============================================================================
// Service Definition
// ============================================================================

service GatewayService {
  // Bidirectional streaming RPC: single persistent connection for all
  // request-response, server-push, and streaming interactions.
  rpc Connect(stream ClientMessage) returns (stream ServerMessage);
}

// ============================================================================
// Top-Level Envelope Messages
// ============================================================================

// Message sent from Agent Runtime (client) to Gateway (server).
message ClientMessage {
  // Unique request ID generated by the client. Used to correlate responses.
  // Monotonically increasing u64, starting from 1.
  uint64 request_id = 1;

  oneof payload {
    // --- Request payloads (req-resp mode) ---
    KeyReleaseRequest       key_release           = 2;
    IntentSendRequest       intent_send           = 3;
    BudgetQueryRequest      budget_query          = 4;
    UsageReportRequest      usage_report          = 5;
    RateAcquireRequest      rate_acquire          = 6;
    PermissionRequest       permission_request    = 7;
    IdentityQueryRequest    identity_query        = 8;
    CapabilityQueryRequest  capability_query      = 9;
    CronRegisterRequest     cron_register         = 10;
    CronUnregisterRequest   cron_unregister       = 11;
    CronListRequest         cron_list             = 12;
    ContextUsageReportRequest context_usage_report = 13;
    AgentHelloRequest       agent_hello           = 14;
    ListSessionsRequest     list_sessions         = 15;
    GetSessionMessagesRequest get_session_messages = 16;
    CreateSessionRequest    create_session        = 17;
    GetCurrentSessionIdRequest get_current_session_id = 18;

    // --- Streaming payload (stream mode) ---
    StreamChunk             stream_chunk          = 19;
  }
}

// Message sent from Gateway (server) to Agent Runtime (client).
message ServerMessage {
  // Echo of the ClientMessage.request_id for req-resp correlation.
  // For unsolicited push messages, request_id = 0.
  uint64 request_id = 1;

  oneof payload {
    // --- Response payloads (req-resp mode) ---
    AgentHelloResult        agent_hello_result    = 2;
    KeyReleaseResult        key_release_result    = 3;
    IntentDelivered         intent_delivered      = 4;
    BudgetInfo              budget_info           = 5;
    UsageReportAck          usage_report_ack      = 6;
    ContextUsageAck         context_usage_ack     = 7;
    RateToken               rate_token            = 8;
    PermissionResult        permission_result     = 9;
    IdentityQueryResult     identity_query_result = 10;
    CapabilityOverview      capability_overview   = 11;
    CronRegisterResult      cron_register_result  = 12;
    CronUnregisterResult    cron_unregister_result = 13;
    CronListResult          cron_list_result      = 14;
    SessionList             session_list          = 15;
    SessionMessages         session_messages      = 16;
    SessionCreated          session_created       = 17;
    CurrentSessionId        current_session_id    = 18;

    // --- Push payloads (server-initiated, request_id = 0) ---
    IntentReceived          intent_received       = 19;
    CapabilityUpdate        capability_update     = 20;
    LLMConfigDelivery       llm_config_delivery   = 21;
    IdentityDelivery        identity_delivery     = 22;
    WorkspaceContextUpdate  workspace_context_update = 23;
    IterationLimitPaused    iteration_limit_paused = 24;
  }
}

// ============================================================================
// Client Request Messages (Runtime -> Gateway)
// ============================================================================

message KeyReleaseRequest {
  string provider = 1;
}

message IntentSendRequest {
  string target = 1;
  string action = 2;
  // JSON-encoded params object, deserialized by the handler.
  string params_json = 3;
  bool async_ = 4;
}

message BudgetQueryRequest {
  string provider = 1;
}

message UsageReportRequest {
  string agent_id = 1;
  string provider = 2;
  uint64 tokens_used = 3;
  double cost_usd = 4;
  // ISO 8601 timestamp.
  string timestamp = 5;
  string error = 6;
}

message RateAcquireRequest {
  string provider = 1;
}

message PermissionRequest {
  string request_id = 1;
  string permission = 2;
  string reason = 3;
  uint64 timeout_ms = 4;
}

message IdentityQueryRequest {
  repeated string fields = 1;
}

message CapabilityQueryRequest {
  // Empty means query all agents.
  string agent_id = 1;
}

message CronRegisterRequest {
  string agent_id = 1;
  string schedule = 2;
  string action = 3;
  string params_json = 4;
}

message CronUnregisterRequest {
  string cron_id = 1;
}

message CronListRequest {
}

message ContextUsageReportRequest {
  string agent_id = 1;
  ContextUsageInfo context = 2;
}

message AgentHelloRequest {
  string agent_id = 1;
  string version = 2;
  string connection_role = 3;
}

message ListSessionsRequest {
}

message GetSessionMessagesRequest {
  string session_id = 1;
  string cursor = 2;
  uint32 limit = 3;
  string direction = 4;
}

message CreateSessionRequest {
}

message GetCurrentSessionIdRequest {
}

// ============================================================================
// Server Response Messages (Gateway -> Runtime)
// ============================================================================

message AgentHelloResult {
  bool success = 1;
  string error = 2;
}

message KeyReleaseResult {
  string api_key = 1;
  string error = 2;
}

message IntentDelivered {
  string message_id = 1;
}

message BudgetInfo {
  uint64 remaining_tokens = 1;
  double remaining_cost_usd = 2;
}

message UsageReportAck {
}

message ContextUsageAck {
}

message RateToken {
  bool granted = 1;
  uint64 retry_after_ms = 2;
}

message PermissionResult {
  string request_id = 1;
  bool granted = 2;
  string reason = 3;
}

message IdentityQueryResult {
  map<string, string> values = 1;
  map<string, float> confidence = 2;
}

message CapabilityOverview {
  // agent_id -> list of action names.
  map<string, StringList> capabilities = 1;
}

message StringList {
  repeated string items = 1;
}

message CronRegisterResult {
  string cron_id = 1;
  string error = 2;
}

message CronUnregisterResult {
  bool removed = 1;
}

message CronListResult {
  repeated CronEntryInfo entries = 1;
}

message SessionList {
  repeated SessionInfoDto sessions = 1;
}

message SessionMessages {
  repeated ConversationEntryDto messages = 1;
  string cursor = 2;
  bool has_more = 3;
}

message SessionCreated {
  string session_id = 1;
}

message CurrentSessionId {
  string session_id = 1;
}

// ============================================================================
// Server Push Messages (Gateway -> Runtime, unsolicited)
// ============================================================================

message IntentReceived {
  string from = 1;
  string action = 2;
  string params_json = 3;
}

message CapabilityUpdate {
  string agent_id = 1;
  repeated string actions = 2;
  bool removed = 3;
}

message LLMConfigDelivery {
  string provider = 1;
  string model = 2;               // empty = no preference
  string api_key = 3;
  string base_url = 4;            // empty = none
  repeated string models = 5;
  ModelCapabilitiesInfo model_capabilities = 6;
  uint64 max_output_tokens_limit = 7;
}

message IdentityDelivery {
  repeated IdentityEntry entries = 1;
}

message WorkspaceContextUpdate {
  string context_text = 1;
  string current_workspace_id = 2;
  string current_workspace_path = 3;
}

message IterationLimitPaused {
  uint32 iteration = 1;
  uint32 max_iterations = 2;
  string message = 3;
}

// ============================================================================
// Streaming Message
// ============================================================================

// StreamChunk replaces the old TYPE_STREAM_CHUNK frame.
// It carries the same IntentSend payload but is sent without awaiting a response.
message StreamChunk {
  string target = 1;
  string action = 2;
  string params_json = 3;
}

// ============================================================================
// Shared Data Types
// ============================================================================

message ModelCostInfo {
  double input_per_million = 1;
  double output_per_million = 2;
}

message ModelModalities {
  repeated string input = 1;
  repeated string output = 2;
}

message ModelCapabilitiesInfo {
  uint64 context_window = 1;
  uint64 max_output_tokens = 2;
  uint64 max_input_tokens = 3;
  bool supports_tool_calling = 4;
  bool supports_reasoning = 5;
  bool supports_attachment = 6;
  bool supports_temperature = 7;
  ModelCostInfo cost = 8;
  ModelModalities modalities = 9;
  string name = 10;
  string family = 11;
  string knowledge_cutoff = 12;
}

message ContextUsageInfo {
  uint64 context_window = 1;
  uint64 input_tokens = 2;
  uint64 output_tokens = 3;
  uint64 total_tokens = 4;
  uint64 max_input_tokens = 5;
  uint64 usable_context = 6;
  uint32 usage_percent = 7;
}

message IdentityEntry {
  string field = 1;
  string value = 2;
  float confidence = 3;
  string category = 4;   // "Identity" | "Preferences" | "Knowledge" | "Work"
  string privacy = 5;    // "Public" | "Personal" | "Sensitive"
  string source = 6;
  string updated_at = 7; // ISO 8601
}

message CronEntryInfo {
  string id = 1;
  string agent_id = 2;
  string schedule = 3;
  string action = 4;
  string params_json = 5;
}

message SessionInfoDto {
  string session_id = 1;
  string created_at = 2;     // ISO 8601
  uint32 message_count = 3;
  string title = 4;
}

message ConversationEntryDto {
  string id = 1;
  string ts = 2;             // ISO 8601
  string role = 3;           // "user" | "assistant" | "think" | "tool_call" | "tool_result" | "system"
  string content = 4;
  string metadata_json = 5;  // JSON-encoded optional metadata
}
```

### 3.2 消息分类与流向

| 消息                                              | 方向                        | 模式       | 说明                     |
| ------------------------------------------------- | --------------------------- | ---------- | ------------------------ |
| `KeyReleaseRequest` / `KeyReleaseResult`          | Runtime → Gateway → Runtime | req-resp   | API Key 获取             |
| `IntentSendRequest` / `IntentDelivered`           | Runtime → Gateway → Runtime | req-resp   | Intent 同步发送          |
| `IntentReceived`                                  | Gateway → Runtime           | **push**   | 外部 Agent 发来的 Intent |
| `BudgetQueryRequest` / `BudgetInfo`               | Runtime → Gateway → Runtime | req-resp   | 预算查询                 |
| `UsageReportRequest` / `UsageReportAck`           | Runtime → Gateway → Runtime | req-resp   | 用量上报                 |
| `ContextUsageReportRequest` / `ContextUsageAck`   | Runtime → Gateway → Runtime | req-resp   | 上下文用量上报           |
| `RateAcquireRequest` / `RateToken`                | Runtime → Gateway → Runtime | req-resp   | 速率令牌申请             |
| `PermissionRequest` / `PermissionResult`          | Runtime → Gateway → Runtime | req-resp   | 权限申请                 |
| `IdentityQueryRequest` / `IdentityQueryResult`    | Runtime → Gateway → Runtime | req-resp   | 身份查询                 |
| `IdentityDelivery`                                | Gateway → Runtime           | **push**   | 冷启动身份注入           |
| `CapabilityQueryRequest` / `CapabilityOverview`   | Runtime → Gateway → Runtime | req-resp   | 能力查询                 |
| `CapabilityUpdate`                                | Gateway → Runtime           | **push**   | 安装/卸载导致的增量更新  |
| `CronRegisterRequest` / `CronRegisterResult`      | Runtime → Gateway → Runtime | req-resp   | 定时任务注册             |
| `CronUnregisterRequest` / `CronUnregisterResult`  | Runtime → Gateway → Runtime | req-resp   | 定时任务注销             |
| `CronListRequest` / `CronListResult`              | Runtime → Gateway → Runtime | req-resp   | 定时任务列表             |
| `AgentHelloRequest` / `AgentHelloResult`          | Runtime → Gateway → Runtime | req-resp   | 连接注册                 |
| `LLMConfigDelivery`                               | Gateway → Runtime           | **push**   | 握手时推送 LLM 配置      |
| `WorkspaceContextUpdate`                          | Gateway → Runtime           | **push**   | 工作区上下文推送         |
| `IterationLimitPaused`                            | Gateway → Runtime           | **push**   | 迭代上限暂停通知         |
| `ListSessionsRequest` / `SessionList`             | Runtime → Gateway → Runtime | req-resp   | 会话列表                 |
| `GetSessionMessagesRequest` / `SessionMessages`   | Runtime → Gateway → Runtime | req-resp   | 分页消息查询             |
| `CreateSessionRequest` / `SessionCreated`         | Runtime → Gateway → Runtime | req-resp   | 会话创建                 |
| `GetCurrentSessionIdRequest` / `CurrentSessionId` | Runtime → Gateway → Runtime | req-resp   | 当前会话 ID              |
| `StreamChunk`                                     | Runtime → Gateway           | **stream** | LLM 输出 chunk，无响应   |

### 3.3 request_id 关联机制

```rust
// Runtime side: generate monotonically increasing request IDs
let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);

// Gateway side: echo request_id in the response
ServerMessage {
    request_id: client_request_id,  // echo back
    payload: Some(...),
}

// Push messages: request_id = 0 (indicates unsolicited)
ServerMessage {
    request_id: 0,
    payload: Some(ServerPayload::IntentReceived(...)),
}
```

- **客户端生成**：`u64` 自增，从 1 开始，`0` 保留给服务端推送
- **服务端回传**：响应消息中必须原样回传 `request_id`
- **并发安全**：同一 bidirectional stream 上的多个请求可并发发送，客户端通过 `request_id` 匹配响应
- **推送标识**：`request_id = 0` 表示 Server Push，客户端收到后直接路由到推送处理逻辑，不进入等待中的请求映射表

---

## 4. Transport 层设计

### 4.1 统一方案：gRPC over TCP Loopback（全平台）

**决策**：全平台统一使用 TCP loopback（`127.0.0.1`），不再维护 UDS、Named Pipe 等差异化路径。

**理由**：
- TCP loopback 不经过物理网卡，延迟与 UDS/Named Pipe 处于同一数量级（< 0.1ms）
- tonic 原生支持 TCP，无需自定义 transport 适配代码
- 一套代码覆盖所有平台（Linux/macOS/Windows），消除 `#[cfg(unix)]` / `#[cfg(windows)]` 条件编译
- 127.0.0.1 不暴露到外部网络，安全边界与 UDS 相当

```rust
// Gateway server (all platforms)
use tonic::transport::Server;

let addr = "127.0.0.1:19877".parse()?;
Server::builder()
    .add_service(gateway_service)
    .serve(addr)
    .await?;
```

```rust
// Runtime client (all platforms)
use tonic::transport::Endpoint;

let channel = Endpoint::from_static("http://127.0.0.1:19877")
    .connect()
    .await?;
```

**端口选择**：固定端口 `19877`（与现有 HTTP API 端口 `19876` 相邻，便于记忆和管理）。

### 4.2 Gateway HTTP API 评估：gRPC-Web 替代方案

当前 Desktop App 通过 Axum HTTP API（端口 19876）连接 Gateway。评估是否可用 gRPC-Web 统一：

| 维度                     | gRPC-Web                                                                                  | Axum HTTP API                               | 结论      |
| ------------------------ | ----------------------------------------------------------------------------------------- | ------------------------------------------- | --------- |
| **Tauri 前端调用**       | 需通过 `@protobuf-ts/grpcweb-transport` 或 `grpc-web-client`，在 WebView 中可行但增加依赖 | 原生 `fetch` / `axios` 直接调用，零额外依赖 | HTTP 更优 |
| **SSE / WebSocket 支持** | gRPC-Web 支持 server streaming，但客户端库较 HTTP SSE 复杂                                | Axum 原生支持 WebSocket + SSE，前端生态成熟 | HTTP 更优 |
| **浏览器兼容性**         | 需 grpc-web proxy（envoy 或 grpcweb wrapper）                                             | 无中间层，直接访问                          | HTTP 更优 |
| **API 一致性**           | 与 IPC 共用 proto 定义，schema 统一                                                       | 需手动维护 HTTP handler 与 proto 的对应     | gRPC 更优 |
| **调试工具**             | grpcurl / grpcui 专业工具                                                                 | 浏览器 DevTools / curl 随手可用             | HTTP 更优 |

**结论**：**保留 Axum HTTP API**。

- Tauri WebView 前端调用 gRPC-Web 需要额外 JS 依赖和代理层，复杂度高于原生 HTTP
- Desktop App 的 chat / settings / agent list 等界面使用 REST + WebSocket 已足够，无需 gRPC 的 streaming 能力
- Gateway 同时暴露两个端口：19876（HTTP API for Desktop）+ 19877（gRPC IPC for Runtime），职责清晰
- 未来若需要，可将 HTTP API 的 handler 内部通过 gRPC client 调用 GatewayService，实现架构统一

### 4.3 TLS 策略

| 场景                     | TLS           | 理由                                                             |
| ------------------------ | ------------- | ---------------------------------------------------------------- |
| 本地 IPC（127.0.0.1）    | **禁用**      | 本地进程间通信，无网络暴露风险；TLS 增加握手开销和证书管理复杂度 |
| 远程 Gateway（未来场景） | **可选 mTLS** | 若 Gateway 与 Runtime 跨主机部署，启用双向 TLS 认证              |

---

## 5. Gateway gRPC Server 设计

### 5.1 服务实现架构

```rust
use tonic::{Request, Response, Status};
use tokio::sync::{mpsc, RwLock};
use std::sync::Arc;
use std::collections::HashMap;

pub struct GatewayGrpcService {
    state: SharedState,
    session_mgr: SharedSessionMgr,
    perm_store: SharedPermissionStore,
    capability_tx: broadcast::Sender<ServerMessage>,
    bridge_tx: Option<broadcast::Sender<BridgeEvent>>,
    session_pending: Option<SessionPendingRequests>,
}

#[tonic::async_trait]
impl gateway_service_server::GatewayService for GatewayGrpcService {
    type ConnectStream = Pin<Box<dyn Stream<Item = Result<ServerMessage, Status>> + Send>>;

    async fn connect(
        &self,
        request: Request<Streaming<ClientMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let mut inbound = request.into_inner();
        let (outbound_tx, outbound_rx) = mpsc::channel::<Result<ServerMessage, Status>>(32);

        // Spawn handler task for this connection
        let state = Arc::clone(&self.state);
        let session_mgr = Arc::clone(&self.session_mgr);
        let perm_store = Arc::clone(&self.perm_store);
        let capability_tx = self.capability_tx.subscribe();
        let bridge_tx = self.bridge_tx.clone();
        let session_pending = self.session_pending.clone();

        tokio::spawn(async move {
            let mut pending_responses: HashMap<u64, oneshot::Sender<ServerMessage>> = HashMap::new();

            loop {
                tokio::select! {
                    // Branch 1: Incoming request from Runtime
                    msg = inbound.message() => {
                        match msg {
                            Ok(Some(client_msg)) => {
                                let request_id = client_msg.request_id;
                                let response = dispatch_grpc_request(
                                    client_msg, &state, &session_mgr, &perm_store,
                                    &bridge_tx, &session_pending
                                ).await;

                                // If it's a stream chunk (no response expected), skip
                                if is_stream_chunk(&client_msg) {
                                    continue;
                                }

                                // Send response back with matching request_id
                                let server_msg = ServerMessage {
                                    request_id,
                                    payload: Some(response),
                                };
                                let _ = outbound_tx.send(Ok(server_msg)).await;
                            }
                            Ok(None) => break, // Client closed stream
                            Err(e) => {
                                tracing::warn!("gRPC inbound error: {}", e);
                                break;
                            }
                        }
                    }

                    // Branch 2: Server push (IntentReceived forwarded to target)
                    push = push_rx.recv() => {
                        if let Some(msg) = push {
                            let server_msg = ServerMessage {
                                request_id: 0,  // 0 = unsolicited push
                                payload: Some(msg),
                            };
                            let _ = outbound_tx.send(Ok(server_msg)).await;
                        }
                    }

                    // Branch 3: CapabilityUpdate broadcast
                    cap = capability_tx.recv() => {
                        match cap {
                            Ok(msg) => {
                                let server_msg = ServerMessage {
                                    request_id: 0,
                                    payload: Some(msg),
                                };
                                let _ = outbound_tx.send(Ok(server_msg)).await;
                            }
                            Err(_) => {}
                        }
                    }
                }
            }

            // Cleanup on disconnect
            // ...
        });

        let output_stream = ReceiverStream::new(outbound_rx);
        Ok(Response::new(
            Box::pin(output_stream) as Self::ConnectStream
        ))
    }
}
```

**关键变化**：
- 替换 `handle_connection` + `tokio::select!` + 自定义帧读写
- 使用 tonic 的 bidirectional streaming：一个 `Streaming<ClientMessage>` 入站，一个 `Stream<ServerMessage>` 出站
- 推送消息和响应消息共用同一个出站 stream，但通过 `request_id = 0` 与客户端区分
- **不再使用 `TYPE_RESPONSE` 复用**：protobuf `oneof payload` 天然区分消息类型

### 5.2 会话管理

```rust
pub struct GrpcSession {
    pub agent_id: Option<String>,
    pub connection_role: String,
    pub push_tx: mpsc::Sender<ServerMessage>,
    pub authenticated: bool,
}

pub struct GrpcSessionManager {
    sessions: HashMap<String, GrpcSession>,  // conn_id -> session
}
```

**AgentHello 握手流程迁移**：

```
① Runtime -> Gateway: ClientMessage { request_id: 1, agent_hello: {...} }
② Gateway -> Runtime: ServerMessage { request_id: 1, agent_hello_result: { success: true } }
③ Gateway -> Runtime: ServerMessage { request_id: 0, llm_config_delivery: {...} }   (push)
④ Gateway -> Runtime: ServerMessage { request_id: 0, identity_delivery: {...} }     (push)
⑤ Gateway -> Runtime: ServerMessage { request_id: 0, capability_overview: {...} }   (push)
⑥ Gateway -> Runtime: ServerMessage { request_id: 0, workspace_context_update: {...} } (push)
```

与旧协议的区别：
- 步骤 ② 是 req-resp（`request_id = 1`）
- 步骤 ③~⑥ 是 push（`request_id = 0`）
- 所有消息在同一个 gRPC stream 上，但客户端通过 `request_id` 正确区分

### 5.3 与 HTTP API 的桥接

```rust
// Existing: HTTP handler uses session_pending oneshot for Runtime responses
// New: gRPC handler also uses the same session_pending mechanism

async fn handle_http_list_sessions(
    state: SharedState,
    session_pending: SessionPendingRequests,
    session_mgr: SharedSessionMgr,
) -> Result<Json<SessionList>, Status> {
    // 1. Find a connected Runtime session for the target agent
    let runtime_session = find_runtime_session(&session_mgr);

    // 2. Generate request_id and create oneshot
    let request_id = generate_http_request_id();
    let (tx, rx) = oneshot::channel();
    {
        let mut map = session_pending.lock().await;
        map.insert(request_id.clone(), tx);
    }

    // 3. Send IntentSend via gRPC push channel to Runtime
    let intent_msg = ServerMessage {
        request_id: 0,
        payload: Some(ServerPayload::IntentReceived {
            from: "http-api".to_string(),
            action: "list_sessions".to_string(),
            params_json: json!({ "request_id": request_id }).to_string(),
        }),
    };
    runtime_session.push_tx.send(intent_msg).await?;

    // 4. Wait for Runtime response (via IntentSend "session_response" action)
    let result = tokio::time::timeout(Duration::from_secs(5), rx).await?;
    Ok(Json(result?))
}
```

- `session_pending` oneshot 机制完全保留
- HTTP handler 通过 gRPC 内部通道向 Runtime 发送 `IntentReceived` 推送
- Runtime 处理后用 `IntentSend`（action=`session_response`）回复，gRPC server 端的 `dispatch_request` 识别并触发 oneshot

---

## 6. Runtime gRPC Client 设计

### 6.1 客户端架构

```rust
use tonic::transport::Channel;
use tokio::sync::{mpsc, oneshot, Mutex};
use std::collections::HashMap;

pub struct GatewayGrpcClient {
    // tonic gRPC client stub
    client: GatewayServiceClient<Channel>,
    // Outbound message sender (to gRPC stream)
    outbound_tx: mpsc::Sender<ClientMessage>,
    // Request ID counter
    next_request_id: AtomicU64,
    // Pending request map: request_id -> oneshot sender for response
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ServerMessage>>>>,
    // Push message handler callback
    push_handler: Box<dyn Fn(ServerMessage) + Send + Sync>,
}
```

```rust
impl GatewayGrpcClient {
    pub async fn connect(endpoint: &str) -> Result<Self, AcoworkError> {
        let channel = create_channel(endpoint).await?;
        let mut client = GatewayServiceClient::new(channel);

        // Establish bidirectional stream
        let (outbound_tx, outbound_rx) = mpsc::channel(32);
        let outbound_stream = ReceiverStream::new(outbound_rx);
        let response = client.connect(outbound_stream).await?;
        let mut inbound = response.into_inner();

        let pending = Arc::new(Mutex::new(HashMap::<u64, oneshot::Sender<ServerMessage>>::new()));

        // Spawn inbound receive loop
        let pending_clone = Arc::clone(&pending);
        tokio::spawn(async move {
            while let Some(result) = inbound.message().await.ok().flatten() {
                let msg = result;
                if msg.request_id == 0 {
                    // Push message: route to push handler
                    handle_push_message(msg);
                } else {
                    // Response: fulfill pending request
                    let mut map = pending_clone.lock().await;
                    if let Some(sender) = map.remove(&msg.request_id) {
                        let _ = sender.send(msg);
                    }
                }
            }
        });

        Ok(Self {
            client,  // Note: client is consumed by connect(); this is simplified
            outbound_tx,
            next_request_id: AtomicU64::new(1),
            pending,
            push_handler: Box::new(default_push_handler),
        })
    }
}
```

**关键变化**：
- 替换 `GatewayClient` + `AsyncTransportConnection` + 自定义帧读写
- **移除 `pending_push` 缓冲区**：gRPC stream 内建多路复用，推送消息和响应消息天然不混淆
- 入站消息统一由单个 `inbound.message()` 循环接收，通过 `request_id` 分流

### 6.2 请求-响应模式

```rust
pub async fn send_request<R>(&self, payload: client_message::Payload) -> Result<R, AcoworkError>
where
    R: From<server_message::Payload>,
{
    let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();

    {
        let mut pending = self.pending.lock().await;
        pending.insert(request_id, tx);
    }

    let msg = ClientMessage {
        request_id,
        payload: Some(payload),
    };
    self.outbound_tx.send(msg).await
        .map_err(|e| AcoworkError::Ipc(format!("Failed to send request: {}", e)))?;

    let response = tokio::time::timeout(
        Duration::from_secs(30),
        rx
    ).await.map_err(|_| AcoworkError::Ipc("Request timeout".to_string()))?
        .map_err(|_| AcoworkError::Ipc("Response channel closed".to_string()))?;

    R::from(response.payload.ok_or(AcoworkError::Ipc("Empty response payload".to_string()))?)
}
```

- 使用 `HashMap<u64, oneshot::Sender>` 替代 `send_and_recv()` 的阻塞循环
- 超时由 `tokio::time::timeout` 统一控制
- 并发请求安全：多个 `send_request` 调用可并行，各自分配独立 `request_id`

### 6.3 推送消息处理

```rust
fn handle_push_message(msg: ServerMessage) {
    match msg.payload {
        Some(ServerPayload::IntentReceived(intent)) => {
            // Route to Intent handler
            runtime.intent_router.handle_incoming(intent);
        }
        Some(ServerPayload::CapabilityUpdate(update)) => {
            runtime.capability_cache.apply_update(update);
        }
        Some(ServerPayload::LlmConfigDelivery(cfg)) => {
            runtime.set_llm_config(cfg);
        }
        Some(ServerPayload::IdentityDelivery(id)) => {
            runtime.set_identity(id);
        }
        Some(ServerPayload::WorkspaceContextUpdate(ctx)) => {
            runtime.set_workspace_context(ctx);
        }
        Some(ServerPayload::IterationLimitPaused(pause)) => {
            runtime.agent_loop.pause(pause);
        }
        _ => {
            tracing::warn!("Unknown push message type");
        }
    }
}
```

- 推送消息（`request_id = 0`）直接路由到业务逻辑
- 不再与 req-resp 混淆，无需 `is_push_message()` 判别

### 6.4 LLM Streaming

```rust
pub async fn send_stream_chunk(
    &self,
    target: &str,
    action: &str,
    params: serde_json::Value,
) -> Result<(), AcoworkError> {
    let msg = ClientMessage {
        request_id: 0,  // stream chunks don't need correlation
        payload: Some(ClientPayload::StreamChunk(StreamChunk {
            target: target.to_string(),
            action: action.to_string(),
            params_json: params.to_string(),
        })),
    };
    self.outbound_tx.send(msg).await
        .map_err(|e| AcoworkError::Ipc(format!("Failed to send stream chunk: {}", e)))?;
    Ok(())
}
```

- `StreamChunk` 作为独立 payload 类型，与 `IntentSendRequest` 区分
- 不等待响应，无 `request_id` 分配开销
- Gateway 收到后直接广播到 `bridge_tx`，不生成响应

### 6.5 重连机制

```rust
impl GatewayGrpcClient {
    pub async fn connect_with_retry(endpoint: &str) -> Result<Self, AcoworkError> {
        let mut backoff = ExponentialBackoff {
            initial_interval: Duration::from_millis(100),
            max_interval: Duration::from_secs(30),
            max_elapsed_time: Some(Duration::from_secs(300)),
            ..Default::default()
        };

        loop {
            match Self::connect(endpoint).await {
                Ok(client) => {
                    tracing::info!("Connected to Gateway gRPC at {}", endpoint);
                    return Ok(client);
                }
                Err(e) => {
                    if let Some(delay) = backoff.next_backoff() {
                        tracing::warn!("Connection failed: {}, retrying in {:?}", e, delay);
                        tokio::time::sleep(delay).await;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    pub async fn reconnect_and_reregister(
        &mut self,
        agent_id: &str,
        version: &str,
    ) -> Result<(), AcoworkError> {
        // Re-establish connection
        *self = Self::connect_with_retry(&self.endpoint).await?;

        // Re-send AgentHello
        self.send_agent_hello(agent_id, version).await?;

        // Re-flush pending usage reports
        self.flush_pending_reports().await?;

        Ok(())
    }
}
```

- 连接断开时，`inbound.message()` 返回 `None`，入站循环退出
- 使用 `backoff` 指数退避重连
- 重连后自动重发 `AgentHello`，Gateway 推送初始化序列（LLM config、Identity 等）
- 待发送的 UsageReport 继续由 `pending_reports` 缓冲，重连后 flush

---

## 7. 迁移策略：直接替换

### 7.1 一步到位

项目处于开发阶段，不考虑向后兼容，采用**直接替换**策略：

```
Step 1: 新建 gRPC 实现
    └── core/acowork-core/proto/gateway_ipc.proto
    └── tonic-build 代码生成（build.rs）
    └── acowork-gateway/src/grpc/server.rs — GatewayService 实现
    └── acowork-runtime/src/grpc/client.rs — GatewayGrpcClient 实现

Step 2: 替换所有引用
    └── 将 Runtime 中所有 GatewayClient 调用改为 GatewayGrpcClient
    └── 将 Gateway 中所有 handle_connection 入口改为 gRPC service
    └── 更新所有集成测试

Step 3: 删除旧代码
    └── 删除 Frame、GatewayRequest/Response（JSON 版本）
    └── 删除 AsyncTransportConnection、AsyncTransportServer trait
    └── 删除 gateway/src/ipc/transport.rs、runtime/src/ipc/transport.rs
    └── 删除 is_push_message、pending_push 缓冲逻辑
    └── 删除所有旧协议相关测试
```

### 7.2 不保留的配置项

| 不保留项               | 理由                           |
| ---------------------- | ------------------------------ |
| `--ipc-mode` 切换开关  | 直接替换，不需要兼容旧协议     |
| 旧 Socket API endpoint | Gateway 仅监听 gRPC 端口 19877 |
| 新旧协议并存代码       | 减少维护负担，代码更干净       |

### 7.3 测试策略

| 测试类型           | 内容                                                                             | 目标                                     |
| ------------------ | -------------------------------------------------------------------------------- | ---------------------------------------- |
| **单元测试**       | 每个 RPC payload 的序列化/反序列化、request_id 生成与匹配                        | 100% proto message 覆盖                  |
| **集成测试**       | Runtime <-> Gateway 端到端：AgentHello 握手、KeyRelease、IntentSend、BudgetQuery | 所有 17 个 request / 23 个 response 变体 |
| **并发测试**       | 10 个并发请求 + 5 个并发推送混合发送                                             | 验证 request_id 无错配                   |
| **帧交错回归测试** | 故意在 Gateway handler 中插入延迟，模拟 push 先于 response                       | 确认客户端正确分流，无旧 bug             |
| **断开重连测试**   | 强制断开 TCP，验证指数退避重连和自动重注册                                       | 确认恢复机制                             |
| **压力测试**       | 1000 个 StreamChunk 连续发送                                                     | 验证背压和内存稳定                       |

### 7.4 清理旧代码清单

迁移完成后，以下代码/文件应全部删除：

| 文件/模块                              | 删除内容                                                   | 说明                                  |
| -------------------------------------- | ---------------------------------------------------------- | ------------------------------------- |
| `core/acowork-core/src/protocol.rs`    | `Frame` 结构体及 `FrameError`                              | 5 字节头帧格式                        |
| `core/acowork-core/src/protocol.rs`    | `GatewayRequest` 枚举（JSON 版本）                         | 17 个 JSON 变体                       |
| `core/acowork-core/src/protocol.rs`    | `GatewayResponse` 枚举（JSON 版本）                        | 23 个 JSON 变体                       |
| `core/acowork-core/src/transport.rs`   | `AsyncTransportConnection` trait                           | 自定义 transport 连接 trait           |
| `core/acowork-core/src/transport.rs`   | `AsyncTransportServer` trait                               | 自定义 transport 服务器 trait         |
| `core/acowork-core/src/transport.rs`   | `TransportKind` / `classify_endpoint`                      | 平台 transport 分类                   |
| `core/acowork-core/src/transport.rs`   | `default_endpoint()`                                       | 旧 endpoint 生成逻辑                  |
| `acowork-gateway/src/ipc/transport.rs` | 全部内容                                                   | Unix Socket / Named Pipe server 实现  |
| `acowork-gateway/src/ipc/server.rs`    | `handle_connection` 函数                                   | 旧连接处理循环（含 `tokio::select!`） |
| `acowork-gateway/src/ipc/server.rs`    | `dispatch_stream_chunk` 中 `Frame::TYPE_STREAM_CHUNK` 处理 | 旧 streaming 帧处理                   |
| `acowork-runtime/src/ipc/transport.rs` | 全部内容                                                   | Unix Socket / Named Pipe client 实现  |
| `acowork-runtime/src/ipc/client.rs`    | `GatewayClient` 结构体                                     | 旧 IPC 客户端                         |
| `acowork-runtime/src/ipc/client.rs`    | `pending_push: VecDeque`                                   | 推送消息缓冲                          |
| `acowork-runtime/src/ipc/client.rs`    | `is_push_message()`                                        | 硬编码推送判别函数                    |
| `acowork-runtime/src/ipc/client.rs`    | `send_and_recv()`                                          | 阻塞式请求-响应循环                   |
| `acowork-runtime/src/ipc/client.rs`    | `send_stream_chunk()` 中 `Frame::TYPE_STREAM_CHUNK`        | 旧 streaming 帧发送                   |
| `acowork-gateway/src/ipc/session.rs`   | `PushSender = mpsc::Sender<GatewayResponse>`               | 旧推送通道类型（如有）                |
| 各 crate 测试文件                      | 所有 `Frame::from_bytes` / `to_bytes` 测试                 | 旧帧格式测试                          |
| 各 crate 测试文件                      | 所有 `test_ipc_server_*` 使用旧 transport 的测试           | 旧 transport 集成测试                 |

---

## 8. 依赖变更

### 8.1 新增依赖

| Crate         | 版本  | 用途                           | 添加到                               |
| ------------- | ----- | ------------------------------ | ------------------------------------ |
| `tonic`       | 0.12+ | gRPC 服务端/客户端框架         | `acowork-gateway`, `acowork-runtime` |
| `prost`       | 0.13+ | protobuf 编解码                | `acowork-core`（共享生成的类型）     |
| `tonic-build` | 0.12+ | 编译期 .proto -> Rust 代码生成 | `acowork-core` (build-dependencies)  |
| `prost-types` | 0.13+ | protobuf Well-Known Types      | `acowork-core`（如需要）             |
| `hyper-util`  | 0.1+  | hyper 工具类型（TokioIo 等）   | `acowork-gateway`, `acowork-runtime` |

Workspace `Cargo.toml` 更新：

```toml
[workspace.dependencies]
tonic = "0.12"
prost = "0.13"
tonice-build = "0.12"
hyper-util = "0.1"
```

`acowork-core/Cargo.toml`:
```toml
[dependencies]
prost = { workspace = true }

[build-dependencies]
tonice-build = { workspace = true }
```

`acowork-gateway/Cargo.toml`:
```toml
[dependencies]
tonic = { workspace = true }
hyper-util = { workspace = true }
```

`acowork-runtime/Cargo.toml`:
```toml
[dependencies]
tonic = { workspace = true }
hyper-util = { workspace = true }
```

### 8.2 可移除依赖

迁移完成后立即删除：

| 文件/模块                                                                 | 说明                                                           |
| ------------------------------------------------------------------------- | -------------------------------------------------------------- |
| `acowork_core::protocol::Frame`                                           | 5 字节头帧格式                                                 |
| `acowork_core::protocol::GatewayRequest` / `GatewayResponse`（JSON 版本） | 旧 JSON 序列化枚举                                             |
| `acowork_core::transport::AsyncTransportConnection`                       | 自定义 transport trait                                         |
| `acowork_core::transport::AsyncTransportServer`                           | 自定义 server trait                                            |
| `acowork-core/src/transport.rs`                                           | 整个 transport 模块（含 `TransportKind`、`classify_endpoint`） |
| `acowork-gateway/src/ipc/transport.rs`                                    | Unix Socket / Named Pipe server 实现                           |
| `acowork-runtime/src/ipc/transport.rs`                                    | Unix Socket / Named Pipe client 实现                           |
| `acowork-runtime/src/ipc/client.rs::GatewayClient`                        | 旧 IPC 客户端                                                  |
| `acowork-runtime/src/ipc/client.rs::pending_push`                         | 推送消息缓冲 VecDeque                                          |
| `acowork-runtime/src/ipc/client.rs::is_push_message`                      | 硬编码推送判别函数                                             |

### 8.3 二进制大小影响

| 项目                                       | 估算              |
| ------------------------------------------ | ----------------- |
| tonic + prost 新增                         | ~1.5 MB           |
| hyper-util（可能已存在，via reqwest/axum） | ~0.3 MB（增量）   |
| 生成的 proto 代码                          | ~0.2 MB           |
| **净增合计**                               | **~1.5 - 2.0 MB** |
| 可移除的自定义帧代码                       | ~0.1 MB           |
| **实际净增**                               | **~1.4 - 1.9 MB** |

注：Gateway 已依赖 `axum` + `tower` + `hyper`，Runtime 已依赖 `reqwest` + `hyper`，tonic 复用现有 hyper/tokio 基础设施，增量低于独立引入。

---

## 9. 风险与缓解

| 风险                                  | 影响 | 缓解措施                                                                                     |
| ------------------------------------- | ---- | -------------------------------------------------------------------------------------------- |
| **一次性替换导致功能回退**            | 高   | 编写覆盖全部 17 个 request / 23 个 response 的集成测试套件；在合并到主分支前必须通过全部测试 |
| **protobuf 与现有 JSON 类型映射遗漏** | 中   | 对照附录 10.2 映射表逐项核对；所有字段保持语义一致；提供 `From`/`TryFrom` 桥接 trait         |
| **gRPC 依赖增加编译时间**             | 低   | tonic/prost 为成熟 crate，编译缓存后可接受；Workspace 统一版本减少重复编译                   |
| **生成的 proto 代码与手写类型不一致** | 中   | 在 `acowork-core` 中提供 `From`/`TryFrom` 转换 trait，将 prost 类型与现有业务类型桥接        |
| **连接断开检测延迟**                  | 中   | 启用 HTTP/2 keep-alive + TCP keepalive；设置合理的超时阈值                                   |
| **调试复杂度增加**                    | 低   | gRPC 支持反射（grpc-reflection）；使用 grpcurl 调试；保留详细 tracing log                    |
| **团队学习成本**                      | 低   | tonic API 与 axum/tower 风格一致；提供示例代码和 migration guide                             |

---

## 10. 附录

### 10.1 完整 .proto 文件

文件路径建议：`core/acowork-core/proto/gateway_ipc.proto`

```protobuf
syntax = "proto3";

package acowork.ipc.v1;

service GatewayService {
  rpc Connect(stream ClientMessage) returns (stream ServerMessage);
}

message ClientMessage {
  // Unique request ID. Always present. 0 = stream chunk (no correlation needed).
  uint64 request_id = 1;
  oneof payload {
    KeyReleaseRequest       key_release           = 2;
    IntentSendRequest       intent_send           = 3;
    BudgetQueryRequest      budget_query          = 4;
    UsageReportRequest      usage_report          = 5;
    RateAcquireRequest      rate_acquire          = 6;
    PermissionRequest       permission_request    = 7;
    IdentityQueryRequest    identity_query        = 8;
    CapabilityQueryRequest  capability_query      = 9;
    CronRegisterRequest     cron_register         = 10;
    CronUnregisterRequest   cron_unregister       = 11;
    CronListRequest         cron_list             = 12;
    ContextUsageReportRequest context_usage_report = 13;
    AgentHelloRequest       agent_hello           = 14;
    ListSessionsRequest     list_sessions         = 15;
    GetSessionMessagesRequest get_session_messages = 16;
    CreateSessionRequest    create_session        = 17;
    GetCurrentSessionIdRequest get_current_session_id = 18;
    StreamChunk             stream_chunk          = 19;
  }
}

message ServerMessage {
  // Echo of ClientMessage.request_id for req-resp. 0 = unsolicited push.
  uint64 request_id = 1;
  oneof payload {
    AgentHelloResult        agent_hello_result    = 2;
    KeyReleaseResult        key_release_result    = 3;
    IntentDelivered         intent_delivered      = 4;
    BudgetInfo              budget_info           = 5;
    UsageReportAck          usage_report_ack      = 6;
    ContextUsageAck         context_usage_ack     = 7;
    RateToken               rate_token            = 8;
    PermissionResult        permission_result     = 9;
    IdentityQueryResult     identity_query_result = 10;
    CapabilityOverview      capability_overview   = 11;
    CronRegisterResult      cron_register_result  = 12;
    CronUnregisterResult    cron_unregister_result = 13;
    CronListResult          cron_list_result      = 14;
    SessionList             session_list          = 15;
    SessionMessages         session_messages      = 16;
    SessionCreated          session_created       = 17;
    CurrentSessionId        current_session_id    = 18;
    IntentReceived          intent_received       = 19;
    CapabilityUpdate        capability_update     = 20;
    LLMConfigDelivery       llm_config_delivery   = 21;
    IdentityDelivery        identity_delivery     = 22;
    WorkspaceContextUpdate  workspace_context_update = 23;
    IterationLimitPaused    iteration_limit_paused = 24;
  }
}

message KeyReleaseRequest       { string provider = 1; }
message IntentSendRequest       { string target = 1; string action = 2; string params_json = 3; bool async_ = 4; }
message BudgetQueryRequest      { string provider = 1; }
message UsageReportRequest      { string agent_id = 1; string provider = 2; uint64 tokens_used = 3; double cost_usd = 4; string timestamp = 5; string error = 6; }
message RateAcquireRequest      { string provider = 1; }
message PermissionRequest       { string request_id = 1; string permission = 2; string reason = 3; uint64 timeout_ms = 4; }
message IdentityQueryRequest    { repeated string fields = 1; }
message CapabilityQueryRequest  { string agent_id = 1; }
message CronRegisterRequest     { string agent_id = 1; string schedule = 2; string action = 3; string params_json = 4; }
message CronUnregisterRequest   { string cron_id = 1; }
message CronListRequest         {}
message ContextUsageReportRequest { string agent_id = 1; ContextUsageInfo context = 2; }
message AgentHelloRequest       { string agent_id = 1; string version = 2; string connection_role = 3; }
message ListSessionsRequest     {}
message GetSessionMessagesRequest { string session_id = 1; string cursor = 2; uint32 limit = 3; string direction = 4; }
message CreateSessionRequest    {}
message GetCurrentSessionIdRequest {}

message AgentHelloResult        { bool success = 1; string error = 2; }
message KeyReleaseResult        { string api_key = 1; string error = 2; }
message IntentDelivered         { string message_id = 1; }
message BudgetInfo              { uint64 remaining_tokens = 1; double remaining_cost_usd = 2; }
message UsageReportAck          {}
message ContextUsageAck         {}
message RateToken               { bool granted = 1; uint64 retry_after_ms = 2; }
message PermissionResult        { string request_id = 1; bool granted = 2; string reason = 3; }
message IdentityQueryResult     { map<string, string> values = 1; map<string, float> confidence = 2; }
message CapabilityOverview      { map<string, StringList> capabilities = 1; }
message StringList              { repeated string items = 1; }
message CronRegisterResult      { string cron_id = 1; string error = 2; }
message CronUnregisterResult    { bool removed = 1; }
message CronListResult          { repeated CronEntryInfo entries = 1; }
message SessionList             { repeated SessionInfoDto sessions = 1; }
message SessionMessages         { repeated ConversationEntryDto messages = 1; string cursor = 2; bool has_more = 3; }
message SessionCreated          { string session_id = 1; }
message CurrentSessionId        { string session_id = 1; }

message IntentReceived          { string from = 1; string action = 2; string params_json = 3; }
message CapabilityUpdate        { string agent_id = 1; repeated string actions = 2; bool removed = 3; }
message LLMConfigDelivery       { string provider = 1; string model = 2; string api_key = 3; string base_url = 4; repeated string models = 5; ModelCapabilitiesInfo model_capabilities = 6; uint64 max_output_tokens_limit = 7; }
message IdentityDelivery        { repeated IdentityEntry entries = 1; }
message WorkspaceContextUpdate  { string context_text = 1; string current_workspace_id = 2; string current_workspace_path = 3; }
message IterationLimitPaused    { uint32 iteration = 1; uint32 max_iterations = 2; string message = 3; }

message StreamChunk             { string target = 1; string action = 2; string params_json = 3; }

message ModelCostInfo           { double input_per_million = 1; double output_per_million = 2; }
message ModelModalities         { repeated string input = 1; repeated string output = 2; }
message ModelCapabilitiesInfo   { uint64 context_window = 1; uint64 max_output_tokens = 2; uint64 max_input_tokens = 3; bool supports_tool_calling = 4; bool supports_reasoning = 5; bool supports_attachment = 6; bool supports_temperature = 7; ModelCostInfo cost = 8; ModelModalities modalities = 9; string name = 10; string family = 11; string knowledge_cutoff = 12; }
message ContextUsageInfo        { uint64 context_window = 1; uint64 input_tokens = 2; uint64 output_tokens = 3; uint64 total_tokens = 4; uint64 max_input_tokens = 5; uint64 usable_context = 6; uint32 usage_percent = 7; }
message IdentityEntry           { string field = 1; string value = 2; float confidence = 3; string category = 4; string privacy = 5; string source = 6; string updated_at = 7; }
message CronEntryInfo           { string id = 1; string agent_id = 2; string schedule = 3; string action = 4; string params_json = 5; }
message SessionInfoDto          { string session_id = 1; string created_at = 2; uint32 message_count = 3; string title = 4; }
message ConversationEntryDto    { string id = 1; string ts = 2; string role = 3; string content = 4; string metadata_json = 5; }
```

### 10.2 现有消息到 gRPC 的映射表

#### GatewayRequest 映射

| #   | Rust 变体                                                          | gRPC Request Message         | 字段映射                                                                |
| --- | ------------------------------------------------------------------ | ---------------------------- | ----------------------------------------------------------------------- |
| 1   | `KeyRelease { provider }`                                          | `KeyReleaseRequest`          | `provider`                                                              |
| 2   | `IntentSend { target, action, params, async_ }`                    | `IntentSendRequest`          | `target`, `action`, `params_json` (JSON string), `async_`               |
| 3   | `BudgetQuery { provider }`                                         | `BudgetQueryRequest`         | `provider`                                                              |
| 4   | `UsageReport(UsageReport)`                                         | `UsageReportRequest`         | `agent_id`, `provider`, `tokens_used`, `cost_usd`, `timestamp`, `error` |
| 5   | `RateAcquire { provider }`                                         | `RateAcquireRequest`         | `provider`                                                              |
| 6   | `PermissionRequest { request_id, permission, reason, timeout_ms }` | `PermissionRequest`          | `request_id`, `permission`, `reason`, `timeout_ms`                      |
| 7   | `IdentityQuery { fields }`                                         | `IdentityQueryRequest`       | `fields` (repeated)                                                     |
| 8   | `CapabilityQuery { agent_id }`                                     | `CapabilityQueryRequest`     | `agent_id`                                                              |
| 9   | `CronRegister { agent_id, schedule, action, params }`              | `CronRegisterRequest`        | `agent_id`, `schedule`, `action`, `params_json`                         |
| 10  | `CronUnregister { cron_id }`                                       | `CronUnregisterRequest`      | `cron_id`                                                               |
| 11  | `CronList {}`                                                      | `CronListRequest`            | 空消息                                                                  |
| 12  | `ContextUsageReport { agent_id, context }`                         | `ContextUsageReportRequest`  | `agent_id`, `context` (ContextUsageInfo)                                |
| 13  | `AgentHello { agent_id, version, connection_role }`                | `AgentHelloRequest`          | `agent_id`, `version`, `connection_role`                                |
| 14  | `ListSessions`                                                     | `ListSessionsRequest`        | 空消息                                                                  |
| 15  | `GetSessionMessages { session_id, cursor, limit, direction }`      | `GetSessionMessagesRequest`  | `session_id`, `cursor`, `limit`, `direction`                            |
| 16  | `CreateSession`                                                    | `CreateSessionRequest`       | 空消息                                                                  |
| 17  | `GetCurrentSessionId`                                              | `GetCurrentSessionIdRequest` | 空消息                                                                  |

#### GatewayResponse 映射

| #   | Rust 变体                                                                                                       | gRPC Response/Push Message      | 字段映射                                                         |
| --- | --------------------------------------------------------------------------------------------------------------- | ------------------------------- | ---------------------------------------------------------------- |
| 1   | `AgentHelloResult { success, error }`                                                                           | `AgentHelloResult`              | `success`, `error`                                               |
| 2   | `KeyReleaseResult { api_key, error }`                                                                           | `KeyReleaseResult`              | `api_key`, `error`                                               |
| 3   | `IntentDelivered { message_id }`                                                                                | `IntentDelivered`               | `message_id`                                                     |
| 4   | `IntentReceived { from, action, params }`                                                                       | `IntentReceived` (push)         | `from`, `action`, `params_json`                                  |
| 5   | `BudgetInfo { remaining_tokens, remaining_cost_usd }`                                                           | `BudgetInfo`                    | `remaining_tokens`, `remaining_cost_usd`                         |
| 6   | `UsageReportAck {}`                                                                                             | `UsageReportAck`                | 空消息                                                           |
| 7   | `ContextUsageAck {}`                                                                                            | `ContextUsageAck`               | 空消息                                                           |
| 8   | `RateToken { granted, retry_after_ms }`                                                                         | `RateToken`                     | `granted`, `retry_after_ms`                                      |
| 9   | `PermissionResult { request_id, granted, reason }`                                                              | `PermissionResult`              | `request_id`, `granted`, `reason`                                |
| 10  | `IdentityDelivery { entries }`                                                                                  | `IdentityDelivery` (push)       | `entries` (repeated IdentityEntry)                               |
| 11  | `LLMConfigDelivery { provider, model, api_key, base_url, models, model_capabilities, max_output_tokens_limit }` | `LLMConfigDelivery` (push)      | 全字段一一对应                                                   |
| 12  | `IdentityQueryResult { values, confidence }`                                                                    | `IdentityQueryResult`           | `values` (map), `confidence` (map)                               |
| 13  | `CapabilityOverview { capabilities }`                                                                           | `CapabilityOverview`            | `capabilities` (map<string, StringList>)                         |
| 14  | `CapabilityUpdate { agent_id, actions, removed }`                                                               | `CapabilityUpdate` (push)       | `agent_id`, `actions`, `removed`                                 |
| 15  | `CronRegisterResult { cron_id, error }`                                                                         | `CronRegisterResult`            | `cron_id`, `error`                                               |
| 16  | `CronUnregisterResult { removed }`                                                                              | `CronUnregisterResult`          | `removed`                                                        |
| 17  | `CronListResult { entries }`                                                                                    | `CronListResult`                | `entries` (repeated CronEntryInfo)                               |
| 18  | `WorkspaceContextUpdate { context_text, current_workspace_id, current_workspace_path }`                         | `WorkspaceContextUpdate` (push) | `context_text`, `current_workspace_id`, `current_workspace_path` |
| 19  | `IterationLimitPaused { iteration, max_iterations, message }`                                                   | `IterationLimitPaused` (push)   | `iteration`, `max_iterations`, `message`                         |
| 20  | `SessionList { sessions }`                                                                                      | `SessionList`                   | `sessions` (repeated SessionInfoDto)                             |
| 21  | `SessionMessages { messages, cursor, has_more }`                                                                | `SessionMessages`               | `messages`, `cursor`, `has_more`                                 |
| 22  | `SessionCreated { session_id }`                                                                                 | `SessionCreated`                | `session_id`                                                     |
| 23  | `CurrentSessionId { session_id }`                                                                               | `CurrentSessionId`              | `session_id`                                                     |

#### Streaming 映射

| 旧协议                                                         | gRPC 映射                                                                               | 说明                      |
| -------------------------------------------------------------- | --------------------------------------------------------------------------------------- | ------------------------- |
| `Frame { msg_type: TYPE_STREAM_CHUNK, body: IntentSend JSON }` | `ClientMessage { request_id: 0, payload: StreamChunk { target, action, params_json } }` | 独立 payload 类型，无响应 |
