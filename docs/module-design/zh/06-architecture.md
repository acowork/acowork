# 架构：依赖关系、数据流、路线图、编译产物、测试

## 1. 模块间依赖关系

```
                    ┌──────────────┐
                    │ acowork-core│ ← 共享类型层
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
    ┌─────────▼──────┐ ┌──▼────────┐ ┌─▼────────────┐
    │acowork-runtime│ │acowork-  │ │acowork-     │
    │                │ │gateway    │ │sign          │
    │    依赖:       │ │           │ │              │
    │  · core        │ │ 依赖:     │ │ 依赖:        │
    │  · SQLite 记忆层      │ │ · core    │ │ · core       │
    │  · sign(验证)  │ │ · sign    │ │ (无运行时依赖)│
    └────────┬───────┘ │ · vault   │ └──────────────┘
             │         └───┬───────┘
             │             │
    ┌────────▼──────┐ ┌───▼──────────┐
    │acowork-sqlite│ │acowork-vault│
    │               │ │              │
    │ 依赖:         │ │ 依赖:        │
    │ · core(Memory │ │ · core       │
    │   trait)      │ │   (无)       │
    └───────────────┘ └──────────────┘
```

**关键约束**：
- `acowork-core` 不依赖任何其他内部 crate
- `acowork-sqlite` 仅依赖 `acowork-core` 的 Memory trait
- `acowork-runtime` 和 `acowork-gateway` 之间**没有直接依赖**，它们通过 IPC 通信
- `acowork-sign` 是独立工具 crate，不依赖运行时 crate

---

## 2. 数据流与通信

### 2.1 Bot 启动流程（数据视角）

```
Gateway CLI: "start com.example.weather"
    │
    ├─1→ PackageManager: 读取已安装 manifest → AgentManifest
    │
    ├─2→ Gateway UserProfile: 读取 identity → {name, city, language}
    │
    ├─3→ Vault: 获取 api_key_ref → SecretString
    │
    ├─4→ SandboxConfig: 从 manifest 生成沙箱参数
    │
    └─5→ LifecycleManager: spawn agent-runtime 进程
         参数: --package-path, --socket, --agent-id, 
               --workspace, --identity (JSON), --dev-mode
```

### 2.2 Bot 主循环（数据视角）

```
Agent Runtime 进程启动
    │
    ├─1→ PackageLoader: 解析 ZIP → (manifest, prompts, skills, config)
    │
    ├─2→ IPC Client: 连接 Gateway Socket → handshake
    │
    ├─3→ IPC Client: KeyRelease → SecretString (存入进程内存)
    │
    ├─4→ `acowork-sqlite`::open(workspace/memory/private.SQLite 记忆层)
    │
    └─5→ 主循环:
         每轮迭代:
         ├─ Context::build() → ChatMessage[]
         ├─ Provider::chat() → ChatResponse
         ├─ ToolDispatcher::dispatch() → ToolResult[]
         ├─ History::append()
         └─ IPC Client: UsageReport (异步)
```

### 2.3 Intent 跨 Bot 通信

```
天气 Agent → IPC Client → GatewayRequest::IntentSend
    │
    ▼
Gateway IntentRouter:
    ├─ 查找 target agent
    ├─ 若未运行 → LifecycleManager::start_agent()
    └─ 转发 Intent → Agent B 的 IPC 连接
    │
    ▼
日历 Agent ← GatewayResponse::IntentReceived
    ├─ 处理 Intent
    └─ 返回结果 → Gateway → 天气 Agent
```

---

## 3. 与路线图的映射

| Phase | 需实现的 Crate | 核心模块 |
|-------|--------------|---------|
| **Phase 1: MVP** | core, runtime, gateway, sign, vault | `core`: manifest + protocol + traits<br>`runtime`: agent/loop + package/loader + providers/openai + tools/builtin(核心17) + tools/memory(5) + tools/agent(intent_send, ask_user) + ipc/client<br>`gateway`: package_manager + lifecycle + ipc/server + vault<br>`sign`: keygen + sign + verify<br>`vault`: 加密存储 |
| **Phase 2: Memory** | + SQLite 记忆层 | `SQLite`: 全部模块（episodic + semantic + fulltext + hybrid + embedding）<br>`runtime`: memory/ 模块<br>`gateway`: system_agent/identity_injector |
| **Phase 2.5: DevFramework** | + runtime/debug | `runtime`: debug/ 全部模块<br>`gateway`: lifecycle 扩展（克隆 API） |
| **Phase 3: 安全沙箱** | + gateway/sandbox | `gateway`: sandbox/ 各平台实现<br>`runtime`: tools/wasm<br>`core`: permission 增强 |
| **Phase 4: 通信协调** | gateway 扩展 | `gateway`: intent/ + budget/ + rate/<br>`runtime`: tools/gateway 增强 |
| **Phase 5: 云端生态** | + desktop app | `apps/acowork-desktop`: Tauri 应用<br>`gateway`: package_manager/repository |

---

## 4. 编译产物

| 二进制 | 来源 Crate | 说明 |
|-------|-----------|------|
| `agent-runtime` | acowork-runtime | Bot 统一执行引擎 |
| `acowork-gateway` | acowork-gateway | Gateway 守护进程 |
| `acowork-keygen` | acowork-sign | 密钥对生成 |
| `acowork-sign` | acowork-sign | .bot 包签名 |
| `acowork-verify` | acowork-sign | .bot 包验签 |
| `acowork` | (CLI wrapper) | 统一 CLI 入口（聚合子命令） |

**统一 CLI 设计**（Phase 5 实现，Phase 1 先用独立二进制）：

```bash
# Phase 1: 独立二进制
agent-runtime /path/to/weather.bot --socket /tmp/gateway.sock
acowork-gateway start
acowork-keygen --alias my-key
acowork-sign --key my-key.pem --input weather.unsigned.bot
acowork-verify weather.bot

# Phase 5: 统一 CLI
acowork gateway start
acowork agent install weather.bot
acowork agent start com.example.weather
acowork keygen --alias my-key
acowork sign weather.unsigned.bot
acowork verify weather.bot
```

---

## 5. 测试策略

| 测试层级 | 位置 | 说明 |
|---------|------|------|
| 单元测试 | 各 crate `src/` 内 `#[cfg(test)]` | 每个 trait 实现都有测试 |
| 组件测试 | `tests/` per crate | 单 crate 内集成测试 |
| 集成测试 | workspace 根 `tests/` | 跨 crate 测试：Gateway + Runtime 交互 |
| 系统测试 | workspace 根 `tests/` | 完整流程：安装 Bot → 启动 → 对话 → 工具调用 → 停止 |
| Live 测试 | 手动 | 连接真实 LLM API 的端到端测试 |

---

> **下一步**：基于此模块设计，逐个模块细化接口定义。建议从 `acowork-core` 开始，因为它是所有 crate 的基础。
