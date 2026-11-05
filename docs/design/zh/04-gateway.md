# Gateway 组件详细设计

> 版本：v3.2 | 更新日期：2026-07-12
> 本版本主要修订：§9 IPC 部分全面对齐 [ADR-033](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) —— Gateway ↔ Runtime 通道由 gRPC 双向流替换为 **MQTT pub/sub + HTTP 反向代理**。HTTP API 仍保留 REST 接口，流式事件从 WebSocket 改为 MQTT 主题订阅。

---

Gateway 是一个常驻的系统级进程，使用 Rust 实现。Gateway **不代理 Agent 的业务逻辑**（不代理 LLM 调用、不代理工具执行），只负责必须集中化的协调工作。

Gateway 同时为两类消费者提供服务：

```
┌──────────────────┐         ┌──────────────────┐
│  Agent Runtime   │         │  Desktop App     │
│  (多个进程)       │         │  / CLI           │
└────────┬─────────┘         └────────┬─────────┘
         │ MQTT + HTTP 反向代理       │ HTTP API
         │ (MQTT pub/sub + 反代)     │ (REST)
         ▼                            ▼
┌────────────────────────────────────────────────┐
│                Gateway (单进程)                 │
│                                                │
│  ┌─────────────┐  ┌──────────┐  ┌──────────┐  │
│  │ Package Mgr │  │ Lifecycle│  │ Intent   │  │
│  │             │  │ Manager  │  │ Router   │  │
│  ├─────────────┤  ├──────────┤  ├──────────┤  │
│  │ Key Vault   │  │ Budget   │  │ Rate     │  │
│  │             │  │ Tracker  │  │ Limiter  │  │
│  └─────────────┘  └──────────┘  └──────────┘  │
└────────────────────────────────────────────────┘
```

- **MQTT + HTTP 反向代理**：给 Agent Runtime 用的 IPC 通道 —— MQTT 承载实时事件推送（chat chunk、tool_call、done、设备状态 Will+Retained），HTTP 反向代理承载大数据查询（历史消息、会话查询），Runtime 自身不暴露对外端口
- **HTTP API**：给 Desktop App / CLI 用的 REST 接口（Axum，localhost only）

两者共享 Gateway 内部状态，只是接入层不同。

## 1. Package Manager

- **安装**：解压 `.agent` 到 `~/.local/share/agent-gateway/agents/<agent_id>/`，校验 manifest 完整性，记录版本。安装前必须验证包签名（详见 [02-agent-package.md](./02-agent-package.md)），签名无效或与已安装版本签名不一致则拒绝安装。
- **卸载**：删除对应目录，可选备份用户数据（含私有 SQLite 记忆层）。
- **升级**：保留 `data/` 和用户修改的 `config/`，替换其他文件。若 runtime_version 不兼容则提示用户。升级时校验新包签名证书指纹必须与已安装版本一致。
- **仓库支持**：可配置多个 HTTP 仓库源（类似 apt），定期检查更新。仓库提供的 .agent 包必须经过签名。

## 2. 生命周期管理器

**启动策略：**
- 按需启动：当收到匹配 trigger 的消息或用户显式调用时启动。
- 常驻：用户可标记某 Agent 开机自启。
- 定时启动：由 cron 表达式触发。（**Phase 3 实现**，Phase 2 仅支持按需启动和常驻模式）

**进程管理：**
- 使用 `std::process::Command` 创建子进程，设置独立工作目录、环境变量。
- 启动参数注入：Agent 包路径、Gateway MQTT broker 地址（`127.0.0.1:19875`）、Agent ID、工作区路径。
- **API Key 分发**：Agent Runtime 通过 MQTT 握手（`AgentHello` 消息，见 [ADR-034](../../adr/zh/ADR-034-mqtt-http-boundary.md)）从 Gateway 拉取 Key（不在环境变量中传递，避免 ps 泄露）。
- 健康检查：如果 Agent 进程退出，根据退出代码决定是否自动重启（可配置）。

**休眠与唤醒：**
- 采用杀死重启策略：空闲超时后直接杀死 Agent Runtime 进程，下次需要时重新 spawn。
- Agent 的状态通过私有 SQLite 持久化，启动时从 Memory 恢复上下文。
- 不使用 SIGSTOP/SIGCONT（Windows 不支持、进程仍占内存、状态序列化困难）。
- Agent 可在 manifest 中声明 `startup_timeout_ms`，Gateway 据此判断是否需要预热（提前拉起）。

## 3. Intent Router

**输入源：**
- 用户界面（CLI/GUI）发出的请求。
- 定时任务触发器。
- 其他 Agent 通过 Gateway 转发的 Intent 消息（见 [06-communication.md](./06-communication.md)）。

**路由规则：**
- 根据消息中的 `target` 字段直接路由到目标 Agent。
- 若目标 Agent 未运行，则按需启动。
- 若未指定 target，则匹配已安装 Agent 的 manifest 中 `triggers.message.pattern`。

## 4. 沙箱配置器

> **2026-04-25 决策（ADR-007）**：进程级沙箱（bubblewrap / AppContainer / Seatbelt）延后至 Phase 7。Phase 3~6 期间，Gateway 启动 Agent Runtime 时使用**策略级隔离**（权限校验 + 路径白名单），不做 OS 级强制隔离。下表保留完整设计供 Phase 7 参考。

Gateway 在启动 Agent Runtime 时根据 manifest 配置沙箱参数，之后由 OS 层面执行隔离。各平台实现方式不同，但隔离目标一致。

**跨平台隔离策略对照：**

| 隔离维度 | Linux | Windows | macOS | Android | iOS |
|---------|-------|---------|-------|---------|-----|
| **进程模型** | spawn 独立进程 | spawn 独立进程 | spawn 独立进程 | 单进程多线程 / Service | 单进程多线程 / Extension |
| **文件隔离** | bubblewrap `--bind`（Phase 7）| 受限令牌 + ACL（Phase 7）| App Sandbox（Phase 7）| 系统沙箱 | 系统沙箱 |
| **网络隔离** | `--unshare-net`（Phase 7）| Firewall API（Phase 7）| Network Extension（Phase 7）| 系统沙箱兜底 | 系统沙箱兜底 |
| **系统调用限制** | seccomp-bpf（Phase 7）| 无（靠 Job Object）| sandbox-exec（Phase 7）| 系统级 | 系统级 |
| **资源限制** | cgroups / rlimit（Phase 7）| Job Object limits（Phase 7）| rlimit | 系统级 | 系统级 |
| **WASM 引擎** | Wasmtime (JIT) | Wasmtime (JIT) | Wasmtime (JIT) | wasmi (解释器) | wasmi (解释器，iOS 禁止 JIT) |

> WASM 引擎选型详情见 [12-tool-system.md](./12-tool-system.md) 第 3.1 节。
| **数据目录** | XDG (`~/.local/share/`) | `%APPDATA%\AgentGateway\` | `~/Library/Application Support/AgentGateway/` | `context.getFilesDir()` | appSupportDir |

**Phase 3~6 策略级隔离**：Gateway 启动 Agent Runtime 时通过 `Command::new` 裸启动，依赖 Runtime 侧的权限校验器（PermissionChecker）做路径白名单和网络权限检查。

**Phase 7 进程级沙箱实现示例（保留供参考）：**
```bash
bwrap \
    --ro-bind /usr /usr \
    --ro-bind /lib /lib \
    --ro-bind /bin /bin \
    --ro-bind /usr/lib/agent-gateway/agent-runtime /app \
    --bind <agent_workspace> /workspace \
    --dev /dev \
    --proc /proc \
    --unshare-net \              # 默认禁止网络（需网络时按 manifest 白名单配置）
    --die-with-parent \
    agent-runtime /workspace/agent-package --socket /tmp/gateway.sock
```

**Windows（Phase 7）：**
- `CreateRestrictedToken` + Job Object + 文件系统 ACL

**macOS（Phase 7）：**
- `sandbox-exec` 配置文件

## 5. Key Vault

集中管理所有 LLM API Key，加密存储：

```
~/.config/agent-gateway/vault/
├── openai_key.enc
├── anthropic_key.enc
└── vault.key               # 主密钥，用户密码派生
```

- Agent manifest 中用 `vault:openai_key` 引用 Key，不存明文。
- Agent Runtime 启动后通过 MQTT 握手获取 Key（一次性传输，不通过环境变量）。
- Key 在 Rust 侧零拷贝/密封存储（使用 secrecy::SecretString），LLM Client 直接使用该 Secret 签名请求，WASM 插件层绝对没有 API 能读取到该字符串。

## 6. Budget Tracker

接收 Agent Runtime 上报的 LLM 用量，维护跨 Agent 的统计：

- 每个 Agent 有独立的日/月 Token 和费用限额。
- 超限时向 Agent 发送信号（stop / fallback / warn）。
- 提供预算查询接口供 Agent 本地预检。

## 7. Rate Limiter

协调多 Agent 对同一 LLM Provider 的并发请求，避免触发 API Rate Limit：

- Agent 调 LLM 前通过 Gateway 申请速率令牌（极轻量 RPC，< 0.1ms）。
- Gateway 基于 Provider 的 RPM/TPM 限制分配令牌。

## 8. 配置与数据存储

- **Gateway 自身配置**：`~/.config/agent-gateway/config.toml`（含 Vault 配置、仓库列表、默认 LLM 配置等）。
- **每个 Agent 的工作区**：`~/.local/share/agent-gateway/agents/<agent_id>/workspace/`：
  - `data/`：从包中复制，可读写。
  - `config/`：用户可修改的配置（初始来自包内 config）。
  - `memory/`：工作区 SQLite 存储文件（`private.sqlite`）——承载会话元数据与对话索引（ADR-082）。记忆节点存放在独立的记忆服务（`$HOME/.acowork/acowork-memory/instances/{instance_id}/memory.db`，ADR-101）。
  - `runtime/`：临时文件（pid、HTTP 端口）。
- **日志**：Gateway 收集所有 Agent 的 stdout/stderr，写入 `~/.local/share/agent-gateway/logs/`，支持按 Agent 过滤。

## 9. HTTP API（Desktop App / CLI 接入层）

> **当前实现状态**：HTTP API 已基于 Axum 完整实现，提供 REST 接口；流式事件经由 MQTT（详见 §9.1 双协议架构 与 [`docs/protocols/zh/README.md`](../../protocols/zh/README.md)）。以下接口定义反映实际实现，但为保持文档简洁仅列出核心路由。完整路由列表参见代码 `crates/acowork-gateway/src/http/routes.rs`。

Gateway 与 Agent Runtime 之间的 IPC 自 [ADR-033](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) 起统一为 **MQTT pub/sub + HTTP 反向代理**：MQTT 承担实时事件推送（chat chunk、tool_call、done、设备状态 Will+Retained），HTTP 反代承担大数据查询与会话历史拉取。HTTP API 作为 Desktop App 和 CLI 的统一接入层，对 Runtime 不可见。

### 9.1 为什么需要双协议（MQTT + HTTP）

| 维度 | MQTT | HTTP 反向代理 |
|------|------|---------------|
| 消费者 | Agent Runtime（Gateway ↔ Runtime 双向） | Gateway → Runtime（按需转发） |
| 监听端 | Gateway 内嵌 rumqttd broker（`127.0.0.1:19875`）| Gateway HTTP 反代（`127.0.0.1:19876`）|
| 通信模式 | 主题 pub/sub（多对多，多路复用）| 请求/响应（点对点）|
| 帧格式 | MQTT Payload（独立 protobuf 命名空间，见 [`core/acowork-core/proto/mqtt_payload.proto`](../../../core/acowork-core/proto/mqtt_payload.proto)）| 标准 HTTP/JSON |
| 认证方式 | localhost only（依赖本地回路保护）| localhost only + 可选 Bearer Token |
| 用途 | 实时事件推送、状态同步、Key 分发（AgentHello）、Intent 触发、Will+Retained 生命周期 | 会话历史、消息查询、配置回写、大数据量 payload 转发 |

两者共享 Gateway 内部逻辑（Package Manager、Lifecycle Manager 等），只是承载协议不同。MQTT 不承载 req/res —— 任何"等回复"的场景走 HTTP 反代，由 Gateway 在内部转换为对 Runtime localhost HTTP 的调用（Runtime 自身不暴露对外端口）。

### 9.2 HTTP Server 配置

```rust
// Gateway 进程启动时同时启动两个端口：
// 1. MQTT broker（内嵌 rumqttd，给 Agent Runtime + Desktop App 用）
// 2. HTTP API（Axum，给 Desktop App / CLI 用；同时承担 Gateway -> Runtime 的反向代理）

pub struct HttpConfig {
    /// 监听地址，默认 127.0.0.1
    pub host: String,
    /// 监听端口，默认 19876
    pub port: u16,
    /// 是否启用认证（Bearer Token），默认 false
    pub auth_enabled: bool,
}
```

> **CORS 始终 permissive**——所有部署场景都使用 `CorsLayer::permissive()`，不再保留
> `[http].cors_enabled` 配置项。原因：dev 模式 Vite (`:5173`) 与生产 Tauri 自定义协议
> (`tauri://localhost`) 都是跨源访问 Gateway (`:19876`)，hardcoded allowlist 一旦遇到浏览器
> 把 `localhost` 解析成不同 IP 字面量就会打穿；本地默认 bind `127.0.0.1`，permissive CORS
> 在 loopback 上没有额外风险。远端场景的 CSRF 防护靠 `Authorization: Bearer <token>`（由
> `auth_enabled` 控制），不是 CORS。

- 默认监听 `http://127.0.0.1:19876`，仅 localhost，不对外暴露
- 端口可在 `config.toml` 中配置
- 端口冲突时自动递增尝试（19876 → 19877 → 19878...），最终端口写入 pidfile 供 Desktop App 发现

### 9.3 路由定义

```rust
use axum::{Router, routing::{get, post, put, delete}, extract::WebSocketUpgrade};

pub fn http_routes() -> Router<GatewayState> {
    Router::new()
        // 健康检查
        .route("/health", get(health_check))

        // --- Agent 管理 ---
        .route("/api/agents", get(list_agents))
        .route("/api/agents/:id", get(get_agent_detail))
        .route("/api/agents/install", post(install_agent))
        .route("/api/agents/:id", delete(uninstall_agent))
        .route("/api/agents/:id/clone", post(clone_agent))   // :id = INSTANCE id (ADR-073)
        .route("/api/agents/:id/start", post(start_agent))
        .route("/api/agents/:id/stop", post(stop_agent))

        // --- 对话 ---
        .route("/api/agents/:id/message", post(send_message))
        .route("/api/agents/:id/stream", get(agent_stream_ws))

        // --- Vault ---
        .route("/api/vault/keys", get(list_keys))
        .route("/api/vault/keys", post(add_key))
        .route("/api/vault/keys/:provider", delete(remove_key))
        .route("/api/vault/keys/:provider", put(update_key))

        // --- 配置 ---
        .route("/api/config", get(get_config))
        .route("/api/config", put(update_config))

        // --- Agent 配置 ---
        .route("/api/agents/:id/config", get(get_agent_config))
        .route("/api/agents/:id/config", put(update_agent_config))

        // --- 用户/身份 ---
        .route("/api/users/profile", get(get_user_profile))
        .route("/api/users/profile", put(update_user_profile))

        // --- 模型 ---
        .route("/api/models", get(list_models))
        .route("/api/models/capabilities", get(get_model_capabilities))

        // --- 记忆 ---
        .route("/api/agents/:id/memory", get(query_memory))
        .route("/api/agents/:id/memory", post(store_memory))

        // --- Skills ---
        .route("/api/agents/:id/skills", get(list_skills))
        .route("/api/agents/:id/skills/:name", get(get_skill_detail))

        // --- 审批 ---
        .route("/api/approvals", get(list_pending_approvals))
        .route("/api/approvals/:id/approve", post(approve))
        .route("/api/approvals/:id/reject", post(reject))

        // --- 发布 ---
        .route("/api/agents/:id/publish/prepare", post(publish_prepare))
        .route("/api/agents/:id/publish/build", post(publish_build))
        .route("/api/agents/:id/publish/install-locally", post(publish_install_locally))
        .route("/api/agents/:id/publish/export", post(publish_export))

        // --- 文档/工作区 ---
        .route("/api/workspace", get(get_workspace))
        .route("/api/workspace/documents", get(list_documents))

        // --- MCP 目录 ---
        .route("/api/mcp/servers", get(list_mcp_servers))
        .route("/api/mcp/servers/:id/install", post(install_mcp_server))

        // --- 定时任务 ---
        .route("/api/cron/jobs", get(list_cron_jobs))
        .route("/api/cron/jobs", post(create_cron_job))

        // --- 系统 ---
        .route("/api/status", get(system_status))
}
```

### 9.4 核心接口详情

#### 9.4.1 Agent 管理

```json
// GET /api/agents
// → 200
{
    "agents": [
        {
            "agent_id": "com.example.weather",
            "name": "Weather Agent",
            "version": "1.0.0",
            "status": "running",       // running | stopped | error
            "dev": false,
            "pid": 12345               // running 时有值
        }
    ]
}

// POST /api/agents/install
// Request: { "path": "/path/to/weather.agent" }
// → 200 { "agent_id": "com.example.weather", "version": "1.0.0" }
// → 400 { "error": "invalid package" }
// → 409 { "error": "already installed" }

// POST /api/agents/{instance_id}/clone
// Request: { "mode": "skeleton" | "full", "new_agent_id": "com.example.weather",
//            "new_name": "天气副本" }      // new_name 可选，"" = "<名称> (克隆)"
// → 201 { "agent_id": "com.example.weather", "install_path": "{packages_dir}/{new_agent_id}/{new_instance_id}" }
//
// {instance_id} 是源实例（ADR-073）；new_agent_id 是包名，可以与源相同 ——
// 同一个包可以挂多个实例，唯一性在 node 生成的 instance id 上。
// 模式语义见 10-debug-protocol.md §7.2.1。
```

#### 9.4.2 对话

```json
// POST /api/agents/:id/message
// Request: { "content": "北京今天天气怎么样" }
// → 200 { "message_id": "msg-001", "status": "queued" }
// → 404 { "error": "agent not found" }
// → 503 { "error": "agent not running" }

// 流式事件改走 MQTT 主题订阅（ADR-033），WebSocket 通道已下线。
// 消息发送仍走 HTTP POST（一次性、不等流式响应）：
//
// → POST /api/agents/:id/message  { content: "..." }
//   → 200  { "message_id": "msg-001", "status": "queued" }
//
// 流式 chunk / tool_call / done 事件由 Desktop App 通过 MQTT broker 订阅：
//
// → Client: SUBSCRIBE acowork/agents/:id/sessions/:sid/messages/+
// ← Broker PUB: { "type": "chunk",       "delta": "今", "message_id": "msg-001" }
// ← Broker PUB: { "type": "chunk",       "delta": "天", "message_id": "msg-001" }
// ← Broker PUB: { "type": "tool_call",   "name": "http_request", "params": {...} }
// ← Broker PUB: { "type": "tool_result", "name": "http_request", "result": {...} }
// ← Broker PUB: { "type": "done",        "message_id": "msg-001", "usage": {...} }
```

#### 9.4.3 Vault

```json
// GET /api/vault/keys
// → 200
{
    "keys": [
        { "provider": "openai", "has_key": true, "key_preview": "sk-...abc" },
        { "provider": "anthropic", "has_key": false }
    ]
}

// POST /api/vault/keys
// Request: { "provider": "openai", "key": "sk-proj-..." }
// → 201 { "provider": "openai" }
// → 400 { "error": "invalid key format" }
```

Vault 的 HTTP API **不返回明文 Key**，只返回存在性和脱敏预览（前 3 字符 + `...` + 后 3 字符）。

#### 9.4.4 系统状态

```json
// GET /api/status
// → 200
{
    "gateway_version": "0.1.0",
    "uptime_seconds": 3600,
    "agents_running": 3,
    "agents_total": 7,
    "memory_usage_mb": 128
}

// GET /health
// → 200 { "status": "ok" }
```

### 9.5 HTTP API 与 MQTT 的关系

> **历史**：v3.1 及更早版本曾用 Socket API（Unix Socket / Named Pipe / Local TCP）作为 Gateway ↔ Runtime 的 IPC 通道，gRPC 时代为 Protocol Buffers 双向流（参见 [`16-ipc-grpc-migration.md`](./16-ipc-grpc-migration.md)）。自 [ADR-033](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) 起 IPC 收敛为 **MQTT + HTTP 反向代理**。

HTTP API 中的 Agent 管理操作（安装/卸载/启停）直接调用 Gateway 内部组件，与 MQTT 通道的处理逻辑共享：

```
POST /api/agents/:id/start
       │
       ▼
Gateway::lifecycle_manager().start_agent("com.example.weather")
       │
       ▼
（与 Agent Runtime 通过 MQTT 发起的状态变更走同一条代码路径）
```

对话消息的转发路径：

```
Desktop App → POST /api/agents/:id/message        (HTTP 触发)
       │
       ▼
Gateway → Intent Router → PUB intent/agents/:id/chat_message
       │                                       (MQTT 实时通道)
       ▼
Agent Runtime 处理 → PUB chat/stream/:sid (chunk/tool_call/done)
       │
       ▼
Broker 推送给 Desktop App (MQTT SUBSCRIBE chat/stream/:sid)
```

HTTP API 不是独立于 IPC 通道的旁路，而是 Runtime 协议的**管理面 + 反向代理封装**。运行时事件流走 MQTT，请求/响应走 HTTP 反代。

### 9.6 安全设计

| 措施 | 说明 |
|------|------|
| 仅监听 localhost | 默认 `127.0.0.1`，不对外暴露 |
| Vault Key 脱敏 | GET 接口不返回明文，POST 接口接收明文 |
| 无 CORS 白名单 | Gateway 不维护 origin allowlist（CORS 始终 permissive）——本地 loopback 0 风险 |
| 可选 Auth Token | Gateway 生成随机 token，Desktop App 首次连接时获取，后续请求携带 `Authorization: Bearer <token>` |
| Agent 安装校验 | 强制验证包签名（HTTP 与 MQTT 通道共享同一校验路径） |

Auth Token 机制（可选，Phase 5+）：
```
Gateway 启动时生成随机 token → 写入 ~/.config/agent-gateway/http_token
Desktop App 首次连接时读取该文件 → 后续请求携带
```

**Avatar 端点不受 Auth Token 约束（设计决策）：**

`GET /api/agents/:id/avatar` 是由浏览器 `<img>` 标签直接发起的请求，无法携带 `Authorization` 头。若该端点强制 Auth，远端 Gateway 场景下头像会全部无法加载。鉴于此：

- Avatar 端点永远允许匿名访问（即使开启了 `auth_enabled`）。
- 路径穿越保护仍然生效（canonicalize 镜像 + 前缀检查）。
- 远端 Gateway 部署时，依赖网络隔离 / TLS / 防火墙 / 反向代理来限制访问面，而非 Auth Token。

#### 9.6.1 远端 Gateway 场景

如果用户将 Gateway 部署在与 Desktop App 不同的机器上（例如家用服务器 / 云主机），需要手动调整以下配置：

| 配置项 | 本机默认值 | 远端场景必填 | 说明 |
|--------|------------|--------------|------|
| `[http].host` | `127.0.0.1` | `0.0.0.0` 或具体网卡 IP | 否则 Desktop App 跨主机请求会被 RST |
| `[http].auth_enabled` | `false` | **远端场景必填 `true`** | 远端 Gateway 已 bind 0.0.0.0，CORS permissive + 不带 token = 全网可调，需 bearer-token 鉴权 |
| `cors_enabled` 配置项 | _已删除_ | — | CORS 始终 permissive（见 §9.2），不需要配置开关 |

远端场景下，包内头像文件位于 Gateway 机器的 `install_path` 下，Desktop App 仍然通过 `GET /api/agents/:id/avatar` 拉取，由 Gateway 从本地文件系统读取后流式响应。头像 URL 拼接 `?v=<version>` 用于 HTTP 缓存击穿，Gateway 端响应头包含 `Cache-Control: public, max-age=31536000, immutable`。

### 9.7 Desktop App 发现 Gateway

Desktop App 需要自动发现 Gateway 的 HTTP API 端口：

```rust
// 发现策略（按优先级）：
// 1. 读取 Desktop App 自身配置中保存的地址
// 2. 读取 Gateway 的 pidfile：~/.local/share/agent-gateway/gateway.pid
//    pidfile 内容：{ "pid": 12345, "http_port": 19876, "mqtt_port": 19875 }
// 3. 尝试默认地址 http://127.0.0.1:19876/health
// 4. 提示用户手动配置
```

## 10. Gateway CLI

Gateway CLI 是一个独立的二进制（`acowork`），为无 GUI 场景提供命令行管理入口。CLI 通过 Gateway HTTP API 与 Gateway 通信，不直接操作 Gateway 内部状态。

### 10.1 命令接口

```bash
# Agent 管理
acowork install <path>          # 安装 .agent 包（含签名验证）
acowork uninstall <agent_id>    # 卸载 Agent
acowork list                    # 列出已安装 Agent 及状态
acowork start <agent_id>       # 启动 Agent
acowork stop <agent_id>        # 停止 Agent
acowork info <agent_id>        # 查看 Agent 详细信息（版本、权限、运行状态）

# 对话
acowork chat <agent_id>        # 进入交互式对话模式
acowork send <agent_id> <msg>  # 发送单条消息并输出响应

# Vault 管理
acowork vault list              # 列出已配置的 Key（脱敏显示）
acowork vault add <provider>    # 添加 API Key（交互式输入，不回显）
acowork vault remove <provider> # 移除 API Key

# 系统管理
acowork status                  # Gateway 状态（版本、运行 Agent 数、内存占用）
acowork config get [key]        # 查看 Gateway 配置
acowork config set <key> <val>  # 修改 Gateway 配置
```

### 10.2 与 Gateway 的通信

CLI 通过 Gateway HTTP API（`http://127.0.0.1:19876`）通信，与 Desktop App 共享同一套 REST 接口（见 §9）。发现机制也与 Desktop App 一致：pidfile → 默认端口 → 手动配置。

```
acowork CLI → HTTP API → Gateway
                    ↑
Desktop App 也走这条路径
```

### 10.3 设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| CLI 与 Gateway 独立 | 独立二进制 | Gateway 可无 CLI 运行（Desktop App 足够），CLI 可安装在不同机器远程管理（未来） |
| 通信方式 | HTTP API（不直接走 MQTT）| HTTP API 是标准 REST，CLI 天然适配；MQTT 用于 Gateway ↔ Runtime 的实时事件推送，CLI 不订阅流式事件 |
| API Key 输入 | 交互式不回显 | 防止 shell history 泄露 Key |
| CLI 框架 | clap | Rust 生态标准选择，与 acowork-sign 工具链保持一致 |

## 11. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| Gateway 不代理业务逻辑 | 纯协调层 | 避免单点瓶颈；Agent Runtime 直连 LLM 延迟更低 |
| 双协议层（自 ADR-033） | MQTT + HTTP 反向代理 | MQTT 承载实时事件推送（chat chunk、tool_call、done、设备状态），HTTP 反代承载大数据查询与会话历史（见 ADR-033 / ADR-034）|
| HTTP 框架 | Axum | Rust 生态最成熟的 HTTP 框架；Gateway 已在技术选型中确认 |
| HTTP 端口 | 127.0.0.1:19876 | 仅 localhost，安全；端口可配置；冲突时自动递增 |
| Vault HTTP 脱敏 | 不返回明文 | 防止 Desktop App 前端漏洞导致 Key 泄露；POST 接口接收明文即可 |
| Gateway 发现机制 | pidfile + 默认地址 | 简单可靠；pidfile 由 Gateway 启动时写入；Desktop App 按优先级尝试 |
| Desktop App 与 Gateway 独立 | 独立进程 | Gateway 可独立运行支持 CLI-only 用户 |
