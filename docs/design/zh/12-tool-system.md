# Tool System（工具系统）

> 版本：v3.5 | 更新日期：2026-10-23

> **v3.5 变更（已实施）**：`memory_recall` / `memory_store` 不再是内置工具——记忆已剥离为 Gateway 托管的独立服务 `acowork-memory`，Agent 侧以 MCP 工具 `mcp_memory__memory_recall` / `mcp_memory__memory_store` 访问（[ADR-100](../../adr/zh/ADR-100-memory-standalone-mcp-service.md)）。随之删除的还有 `memory:read` / `memory:write` 两个权限项，以及本文 §4 / §4.2.1 中描述的“主循环步骤② MemoryManager.retrieve() 自动注入”（auto-inject，ADR-100 决策 5）——记忆获取统一为工具主动调用。这些段落保留作历史设计记录，实际行为以 ADR-100 为准。

---

工具系统是 Agent 感知和操作世界的唯一通道。LLM 通过 tool_calls 调用工具，Runtime 解析并路由到对应的工具实现。所有工具执行都经过权限校验，自定义工具在 WASM 沙箱中隔离运行。

## 1. 工具分类

```
Tool Dispatcher
├── Built-in Tools     # Runtime 内置，Phase 1 即可用
├── WASM Tools         # .agent 包自带，Wasmtime 沙箱执行
├── RAG Tools          # 企业 RAG 接入，查询外部知识库
└── Gateway Tools      # 需要 Gateway 协调的操作（非 LLM tool_call 触发）
```

| 类型     | 来源                             | 执行环境       | LLM 可直接调用         | Phase                                     |
| -------- | -------------------------------- | -------------- | ---------------------- | ----------------------------------------- |
| Built-in | Runtime 内置                     | 宿主进程内     | 是                     | Phase 1                                   |
| WASM     | .agent 包 tools/ 目录            | Wasmtime 沙箱  | 是                     | Phase 1（声明+沙箱）/ Phase 3（完整权限） |
| RAG      | manifest 声明，指向企业 RAG 服务 | 远程 HTTP 调用 | 是                     | Phase 2                                   |
| Gateway  | Gateway Service API              | Gateway 进程   | 否（Runtime 内部发起） | Phase 4                                   |

## 2. Built-in Tools 清单

以下工具由 Agent Runtime 内置实现，Agent 可在 manifest 中声明使用，无需提供实现代码。

**平台基础设施级工具定义：** 内置工具的范围仅限**平台基础设施级**——即调用开放协议（HTTP/DNS/文件系统/操作系统 API）或本地计算（WASM/Embedding），不依赖特定第三方服务的付费 API。SaaS 集成（Jira/Notion/LinkedIn 等）由独立 Agent 提供，不内置。
| 工具名                 | 功能                             | 所需权限                  | 说明                                                                                                                                                                                                                      |
| ---------------------- | -------------------------------- | ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ~~`memory_recall`~~        | ~~语义检索私有 SQLite 记忆层~~          | ~~`memory:read`~~          | **已迁移** — ADR-100：记忆已剥离为独立服务，该工具现为 Gateway 托管记忆 MCP 的 `mcp_memory__memory_recall`，无权限域，经 `agent_mcp_tools.json` 做工具级 opt-in（ADR-069） |
| ~~`memory_store`~~         | ~~写入私有 SQLite 记忆层~~              | ~~`memory:write`~~         | **已迁移** — 同上，现为 `mcp_memory__memory_store`；即时提取 / 五种类型 / importance / privacy 参数语义不变，只是调用通道改了 |
| `http_request`         | HTTP 请求（GET/POST/PUT/DELETE） | `network:<url_pattern>`   | 支持 method 参数选择 HTTP 方法，JSON 响应自动解析，JSON body 和表单                                                                                                                                                       |
| `web_fetch`            | 获取网页内容                     | `network:<url_pattern>`   | HTML → Markdown 转换，Agent 直接获得可读文本                                                                                                                                                                              |
| `shell`                | 执行 shell 命令                  | `filesystem:exec`         | 受沙箱限制，超时可中断                                                                                                                                                                                                    |
| `file_read`            | 读取文件                         | `filesystem:read:<path>`  | 限制在工作区和授权目录内                                                                                                                                                                                                  |
| `file_write`           | 写入文件                         | `filesystem:write:<path>` | 限制在工作区和授权目录内                                                                                                                                                                                                  |
| `file_edit`            | 精确编辑文件                     | `filesystem:write:<path>` | 基于行范围的编辑（替换/插入/删除），比 file_write 更精准                                                                                                                                                                  |
| `glob_search`          | 文件名模式搜索                   | `filesystem:read:<path>`  | 支持 glob 模式匹配，返回文件路径列表                                                                                                                                                                                      |
| `content_search`       | 文件内容搜索                     | `filesystem:read:<path>`  | 类似 grep，支持正则表达式，返回匹配行及上下文                                                                                                                                                                             |
| `intent_send`          | 发送 Intent 到其他 Agent         | `intent:send:<target>`    | 通过 Gateway 路由                                                                                                                                                                                                         |
| ~~`identity_store`~~   | ~~写入用户身份信息~~             | ~~`identity:write`~~      | **已删除** — 用户身份管理已迁移至 Gateway UserProfile，详见 `18-user-identity-simplified.md`                                                                                                                              |
| ~~`identity_query`~~   | ~~查询用户身份信息~~             | ~~`identity:read`~~       | **已删除** — 用户身份管理已迁移至 Gateway UserProfile                                                                                                                                                                     |
| ~~`identity_observe`~~ | ~~订阅身份变更通知~~             | ~~`identity:observe`~~    | **已删除** — 用户身份管理已迁移至 Gateway UserProfile                                                                                                                                                                     |

### 2.1 平台支持矩阵

| 工具名                 | Windows | Linux | macOS | Android |  iOS  | 可用性级别         |
| ---------------------- | :-----: | :---: | :---: | :-----: | :---: | ------------------ |
| `http_request`         |    ✅    |   ✅   |   ✅   |    ✅    |   ✅   | 全平台             |
| `web_fetch`            |    ✅    |   ✅   |   ✅   |    ✅    |   ✅   | 全平台             |
| `shell`                |    ✅    |   ✅   |   ✅   |    ❌    |   ❌   | 仅桌面端           |
| `file_read`            |    ✅    |   ✅   |   ✅   |    ⚠️    |   ⚠️   | 全平台，移动端受限 |
| `file_write`           |    ✅    |   ✅   |   ✅   |    ⚠️    |   ⚠️   | 全平台，移动端受限 |
| `file_edit`            |    ✅    |   ✅   |   ✅   |    ⚠️    |   ⚠️   | 全平台，移动端受限 |
| `glob_search`          |    ✅    |   ✅   |   ✅   |    ⚠️    |   ⚠️   | 全平台，移动端受限 |
| `content_search`       |    ✅    |   ✅   |   ✅   |    ⚠️    |   ⚠️   | 全平台，移动端受限 |
| `intent_send`          |    ✅    |   ✅   |   ✅   |    ✅    |   ✅   | 全平台             |
| ~~`identity_store`~~   |    ❌    |   ❌   |   ❌   |    ❌    |   ❌   | **已删除**         |
| ~~`identity_query`~~   |    ❌    |   ❌   |   ❌   |    ❌    |   ❌   | **已删除**         |
| ~~`identity_observe`~~ |    ❌    |   ❌   |   ❌   |    ❌    |   ❌   | **已删除**         |

> ✅ 完整支持 | ⚠️ 受限支持（行为降级） | ❌ 不可用

### 2.2 跨平台降级策略

**可用性级别定义：**

| 级别               | 含义             | 对 Agent 的影响                                   |
| ------------------ | ---------------- | ------------------------------------------------- |
| 全平台             | 所有平台行为一致 | Agent 可无差别使用                                |
| 仅桌面端           | 移动端不支持     | Agent 声明 `required = true` 时，移动端安装被拒绝 |
| 全平台，移动端受限 | 移动端行为降级   | Agent 可使用但需适配降级行为                      |

**具体降级行为：**

`shell`（仅桌面端）：
- iOS 沙箱禁止执行外部进程，Android 也只有极少数场景可以（root 或 Termux）
- 桌面端也存在 shell 差异：Windows 是 cmd/PowerShell，Linux/macOS 是 bash/zsh，命令语法不同
- Agent 在 manifest 中声明 `required = true` 时，移动端安装检查直接拒绝
- Agent 声明 `required = false`（默认）时，移动端安装成功，但 `shell` 工具不注册到可用工具列表

`file_read` / `file_write` / `file_edit` / `glob_search` / `content_search`（移动端受限）：
- iOS App Sandbox 只允许访问自己容器内的文件
- Android Scoped Storage 限制了外部存储访问
- 降级行为：路径白名单自动收窄到 Agent 工作目录（`<agent_data_dir>/workspace/`），超出范围的路径请求返回权限错误
- `glob_search` / `content_search` 的搜索范围同样限制在 Agent 工作目录内
- 桌面端路径白名单由 `filesystem:read/write:<path>` 权限控制，可授权任意目录

**运行时平台检测：**

Runtime 启动时通过 `std::env::consts::OS` 和平台 API 确定当前平台，生成可用工具列表：

```
Runtime 启动
│
├─ 检测平台 (desktop / android / ios)
│
├─ 构建可用工具列表
│   ├─ 全平台工具 → 始终注册
│   ├─ 仅桌面端工具 → 仅 desktop 注册
│   └─ 移动端受限工具 → 注册但注入降级权限配置
│
└─ 将可用工具列表注入 LLM System Prompt
   （LLM 只能看到当前平台可用的工具，不会尝试调用不可用的工具）
```

**工具声明示例（manifest.toml）：**

```toml
[[tools]]
name = "http_request"
type = "builtin"
permissions = ["network:https://api.weather.com"]

# 记忆工具不再属于 builtin（ADR-100）：它由 Gateway 托管的 memory MCP 提供，
# 工具级 opt-in 写在 `agent_mcp_tools.json`（`mcp_memory__memory_recall`，ADR-069），
# manifest 里没有对应的 `[[tools]]` 条目，也没有 `memory:*` 权限可声明。

# shell 声明为可选——移动端安装不受阻，但工具不可用
[[tools]]
name = "shell"
type = "builtin"
required = false           # 默认 false，移动端安装不拒绝
permissions = ["filesystem:exec"]

# 某个 Agent 强依赖 shell（如 DevOps Agent），声明 required = true
# 移动端安装时会被拒绝，提示"此 Agent 需要桌面端环境"
[[tools]]
name = "shell"
type = "builtin"
required = true
permissions = ["filesystem:exec"]
```

## 3. WASM Tools（自定义工具沙箱）

自定义工具由 Agent 开发者提供 .wasm 文件，在 Wasmtime 沙箱中隔离执行。

### 3.1 运行时选型：Wasmtime

| 维度               | 选择                                 | 理由                                           |
| ------------------ | ------------------------------------ | ---------------------------------------------- |
| **运行时**         | Wasmtime（Bytecode Alliance）        | 标准合规性最强，安全模型最成熟                 |
| **编译器**         | Cranelift（默认）/ Winch（快速启动） | Cranelift 综合性能最优；Winch 用于启动敏感场景 |
| **WASI 版本**      | Preview 2                            | 目录级沙箱，能力安全模型最精细                 |
| **许可证**         | Apache 2.0                           | 无商业限制                                     |
| **crates.io 版本** | 锁定 LTS（如 v36.x）                 | 避免频繁 API 变更影响稳定性                    |

**选型对比（为何不选 Wasmer / Wasmi）：**

| 维度           | Wasmtime     | Wasmer           | Wasmi                |
| -------------- | ------------ | ---------------- | -------------------- |
| WASI Preview 2 | 完整支持     | 不支持           | 不支持               |
| 组件模型       | 参考实现     | 部分支持         | 不支持               |
| Fuel metering  | 成熟         | 有限             | 成熟                 |
| 供应商锁定     | 无（纯标准） | WASIX 锁定       | 无                   |
| 冷启动         | 5.2ms        | 6.8ms            | ~2ms                 |
| 执行时间       | 10.4ms       | 12.1ms           | ~45ms                |
| 安全审计       | 定期         | 无公开           | Runtime Verification |
| 适用场景       | 通用沙箱     | 需要非标准扩展时 | iOS / 极端资源受限   |

- **Wasmer 不选**：核心差异化特性 WASIX 是非标准扩展，二进制只能在 Wasmer 运行，存在厂商锁定风险；WASI Preview 2 缺失导致沙箱文件系统控制不够精细。
- **Wasmi 备选**：Phase 4 移动端适配时，iOS 禁止 JIT 编译，Wasmi（纯解释器）作为移动端 WASM 引擎。

### 3.2 WASM 工具声明

```toml
[[tools]]
name = "image_filter"
type = "wasm"
binary = "./tools/image_filter.wasm"
permissions = ["filesystem:read:./input"]

[tools.image_filter.resource_limits]
max_memory_mb = 50
max_execution_time_ms = 5000
```

**字段说明：**

| 字段                                    | 必需 | 说明                                     |
| --------------------------------------- | ---- | ---------------------------------------- |
| `name`                                  | 是   | 工具名称，LLM 通过此名称调用             |
| `type`                                  | 是   | 必须为 `"wasm"`                          |
| `binary`                                | 是   | .wasm 文件路径（相对于 .agent 包根目录） |
| `permissions`                           | 是   | 所需权限列表（安装时展示，运行时校验）   |
| `resource_limits.max_memory_mb`         | 否   | WASM 线性内存上限（默认 50）             |
| `resource_limits.max_execution_time_ms` | 否   | 执行超时（默认 5000）                    |

### 3.3 WASM 沙箱安全模型

```
Agent Runtime（宿主进程）
│
│  Tool Dispatcher
│     │
│     ▼
│  Wasmtime Engine
│  ┌──────────────────────────────────────┐
│  │  WASM Instance                       │
│  │  ┌────────────────────────────────┐  │
│  │  │  工具逻辑（不可信代码）          │  │
│  │  │                                │  │
│  │  │  只能访问：                     │  │
│  │  │  ├─ 自己的线性内存（受限大小）  │  │
│  │  │  ├─ Host 函数（显式注册的）     │  │
│  │  │  └─ WASI 权限（manifest 声明的）│  │
│  │  │                                │  │
│  │  │  不能访问：                     │  │
│  │  │  ├─ 宿主进程内存               │  │
│  │  │  ├─ 其他工具的内存/状态        │  │
│  │  │  ├─ 未声明的文件路径           │  │
│  │  │  ├─ 未声明的网络地址           │  │
│  │  │  ├─ LLM API Key               │  │
│  │  │  └─ 其他 Agent 的数据          │  │
│  │  └────────────────────────────────┘  │
│  │                                      │
│  │  安全控制层：                        │
│  │  ├─ Fuel metering（CPU 时间上限）    │
│  │  ├─ Memory limit（线性内存上限）     │
│  │  ├─ WASI Preview 2 能力安全         │
│  │  └─ 执行超时（max_execution_time_ms）│
│  └──────────────────────────────────────┘
```

**安全保障机制：**

| 机制            | 作用                                 | 配置来源                                          |
| --------------- | ------------------------------------ | ------------------------------------------------- |
| WASM 内存隔离   | 工具只能访问自己的线性内存，无法越界 | Wasmtime 引擎级保证                               |
| Fuel metering   | 限制 CPU 执行指令数，防止死循环      | Runtime 根据 `max_execution_time_ms` 换算         |
| Memory limit    | 限制线性内存大小，防止 OOM           | `resource_limits.max_memory_mb`                   |
| WASI 目录白名单 | 只能访问 manifest 声明的路径         | `permissions` 中的 `filesystem:read/write:<path>` |
| WASI 网络白名单 | 只能访问 manifest 声明的地址         | `permissions` 中的 `network:<url_pattern>`        |
| API Key 不可见  | WASM 工具无法读取 LLM API Key        | Host 函数不暴露 Key，使用 secrecy::SecretString   |

### 3.4 Host-WASM 通信协议

LLM 调用 WASM 工具时，Runtime 负责参数序列化和结果反序列化：

```
LLM 输出 tool_call:
  { "name": "image_filter", "arguments": {"image_url": "...", "filter": "grayscale"} }
       │
       ▼
Runtime 序列化参数为 JSON 字节:
  host_memory → wasm_linear_memory (通过 Host 函数参数传递)
       │
       ▼
WASM 工具执行:
  读取输入 → 处理 → 写入输出
       │
       ▼
Runtime 反序列化结果:
  wasm_linear_memory → host_memory
       │
       ▼
构造 tool result 返回给 LLM:
  { "filtered_image_url": "...", "status": "success" }
```

**Host 函数接口（Phase 1）：**

WASM 工具必须导出以下入口函数：

```rust
// WASM 侧必须导出的函数
#[no_mangle]
pub extern "C" fn execute(input_ptr: u32, input_len: u32) -> u32;

// WASM 侧可选导出的函数（用于描述工具的 JSON Schema）
#[no_mangle]
pub extern "C" fn schema_ptr() -> u32;
#[no_mangle]
pub extern "C" fn schema_len() -> u32;
```

**通信流程：**

1. Runtime 将 `tool_call.arguments` 序列化为 JSON 字节串
2. 将 JSON 字节串写入 WASM 线性内存
3. 调用 WASM 的 `execute(input_ptr, input_len)` 函数
4. WASM 工具处理后，将结果 JSON 写入线性内存，返回结果指针
5. Runtime 从线性内存读取结果 JSON，反序列化为 tool result

**Phase 3+ 升级路径：** 组件模型（Component Model）提供类型安全的接口定义，替代手动内存操作。WASM 工具可以用 WIT 文件定义接口，Wasmtime 自动生成类型安全的绑定。Phase 1 的 Host 函数方式保证初始简单性，组件模型保证长期扩展性。

### 3.5 WASM 工具开发工具链

Agent 开发者编写 WASM 工具的推荐流程：

```
1. 用 Rust 编写工具逻辑（目标：wasm32-wasip2）
   cargo new --lib image_filter
   # Cargo.toml: crate-type = ["cdylib"], target = wasm32-wasip2

2. 实现必须的导出函数：
   - execute(input_ptr, input_len) -> u32
   - schema_ptr() / schema_len()（可选，提供 JSON Schema）

3. 编译：
   cargo build --target wasm32-wasip2 --release

4. 将 .wasm 文件放入 .agent 包的 tools/ 目录

5. 在 manifest.toml 中声明工具（见 3.2）
```

**SDK 支持（Phase 2+）：** 提供 `acowork-tool-sdk` crate，封装内存分配、JSON 序列化、schema 导出等样板代码，开发者只需实现业务逻辑：

```rust
use acowork_tool_sdk::{tool, ToolInput, ToolOutput};

#[tool(name = "image_filter")]
fn execute(input: ToolInput) -> Result<ToolOutput, ToolError> {
    let image_url: String = input.get("image_url")?;
    let filter: String = input.get("filter")?;
    // ... 业务逻辑
    Ok(ToolOutput::from(json!({"filtered_image_url": result})))
}
```

### 3.6 WASM 工具的错误处理

| 错误类型     | 处理方式                  | 说明                    |
| ------------ | ------------------------- | ----------------------- |
| Fuel 耗尽    | 终止执行，返回超时错误    | 防止死循环              |
| 内存超限     | 终止执行，返回 OOM 错误   | WASM 内存分配失败       |
| 执行超时     | 终止执行，返回超时错误    | `max_execution_time_ms` |
| WASM Trap    | 终止执行，返回崩溃错误    | 除零、栈溢出等          |
| 权限不足     | 拒绝执行，返回权限错误    | 访问未声明的路径/网络   |
| 业务逻辑错误 | 返回 WASM 工具的错误 JSON | 工具自行返回错误信息    |

所有错误都不终止主循环。错误信息作为 tool result 返回给 LLM，由 LLM 决定下一步（换参数、换工具、或放弃）。

## 4. RAG Tools（企业知识库接入）

RAG 工具让 Agent 对接企业自建的 RAG 知识库，实现"双通道检索"——本地记忆层（个人记忆）和企业 RAG（集体知识）并行查询，结果拼接送入 LLM 上下文。ACowork 不托管 RAG 服务，只定义标准查询协议（请求/响应 JSON Schema），企业 RAG 自行适配此协议（详见 00-prd.md §1.13）。

**配置驱动 Opt-In**：RAG 不是默认能力，仅当 manifest 声明 `[[tools]] type = "rag"` 时使能。无 RAG 声明的 Agent，Tool Dispatcher 不注册 RAG 工具，MemoryManager.retrieve() 仅查 SQLite 记忆通道，行为与无 RAG 完全一致。

**混合双触发**：RAG 有两种触发方式，均由 manifest 配置驱动：

| 触发方式 | 时机                               | 查询参数                                       | 说明                           |
| -------- | ---------------------------------- | ---------------------------------------------- | ------------------------------ |
| 自动触发 | 主循环步骤② MemoryManager Retrieve | 用户消息作 query，top_k=3，score_threshold=0.7 | 背景知识注入，LLM 无需主动判断 |
| 显式触发 | 主循环步骤⑤ LLM tool_call          | LLM 自定义 query/filter/top_k                  | 针对性深入查询                 |

自动触发结果作为"背景上下文"注入，显式工具结果作为"工具返回值"追加到 History，两者在上下文中位置不同，语义不重叠。

### 4.1 RAG 工具声明

```toml
[[tools]]
name = "enterprise_knowledge"
type = "rag"
description = "查询企业产品知识库，获取产品参数、技术文档、销售话术等"
# RAG 服务地址（由 Agent 开发者或企业管理员配置）
[tools.enterprise_knowledge.rag_config]
endpoint = "https://rag.internal.company.com/api/query"
collection = "product_docs"
# 认证信息引用 Vault（不明文出现在 manifest 中）
auth_ref = "vault:company_rag_token"
auth_type = "bearer"              # bearer / api_key / oauth2
# 查询参数
max_results = 5
score_threshold = 0.7
```

**字段说明：**

| 字段                         | 必需 | 说明                                                  |
| ---------------------------- | ---- | ----------------------------------------------------- |
| `name`                       | 是   | 工具名称，LLM 通过此名称调用                          |
| `type`                       | 是   | 必须为 `"rag"`                                        |
| `description`                | 是   | RAG 知识库的描述，帮助 LLM 判断何时调用               |
| `rag_config.endpoint`        | 是   | RAG 查询服务的 HTTP URL                               |
| `rag_config.collection`      | 否   | RAG 中的集合/索引/命名空间，用于多租户隔离            |
| `rag_config.auth_ref`        | 是   | 认证凭据引用（Vault 密钥 ID），不明文存储             |
| `rag_config.auth_type`       | 否   | 认证方式，默认 `bearer`                               |
| `rag_config.max_results`     | 否   | 单次查询最大返回条数，默认 5                          |
| `rag_config.score_threshold` | 否   | 最低相关性阈值（0-1），低于此值的结果不返回，默认 0.7 |

### 4.2 RAG 工具执行流程（显式触发）

LLM 通过 tool_call 主动调用 RAG 工具，用于针对性深入查询：

```
LLM 输出 tool_call: { name: "enterprise_knowledge", arguments: { query: "Q3 产品发布计划" } }
       │
       ▼
Runtime 解析 tool_call
       │
       ├─ 从 Vault 获取认证凭据（一次性，不缓存在进程内存）
       │
       ├─ 构造 RAG 标准查询请求（POST endpoint）
       │   body: { query, collection, top_k, score_threshold }
       │   headers: { Authorization: Bearer <token> }
       │
       ├─ 发送 HTTP 请求（超时 10 秒）
       │
       ├─ 解析响应，标注来源（source_url / chunk_id）
       │
       └─ 构造 tool result 返回给 LLM
```

### 4.2.1 RAG 自动检索流程（主循环步骤②）

每轮迭代自动触发（仅 manifest 声明 RAG 时），用用户消息作轻量查询：

```
步骤② MemoryManager.retrieve()
  ├─ SQLite 记忆通道: hybrid_search + graph_expand  ← 始终执行
  └─ RAG 通道: RagClient.query(用户消息, top_k=3)  ← 仅 manifest 声明 RAG 时
     ├─ 成功 → 结果按来源标注 [Memory] / [RAG:enterprise_knowledge]
     ├─ 超时(5s) → 跳过 RAG 通道，仅用 SQLite 记忆结果
     └─ 不可达 → 同上，不阻塞 Agent
  结果合并、去重、按 token 预算裁剪后注入 LLM 上下文
```

### 4.3 RAG 工具的降级与安全

| 规则         | 说明                                                                                  |
| ------------ | ------------------------------------------------------------------------------------- |
| 离线降级     | RAG 服务不可达时，返回空结果，不阻塞 Agent 运行（自动触发和显式触发均降级）           |
| 凭据安全     | auth_ref 引用 Vault 密钥，Runtime 每次调用时从 Vault 获取，不缓存在进程内存或环境变量 |
| 结果标注     | 每条 RAG 结果标注 source_url 和 chunk_id，供 LLM 和用户追溯来源                       |
| 查询范围限制 | collection 字段限定查询范围，防止跨租户数据泄露                                       |
| 网络权限     | RAG 工具的 endpoint 受 `network:<url_pattern>` 权限控制                               |

### 4.4 与本地 Memory 的关系

RAG 工具和本地记忆层 是两条完全独立的检索通道：

| 维度       | 本地记忆层（memory_recall）               | 企业 RAG（rag tool）                   |
| ---------- | ------------------------------------------ | -------------------------------------- |
| 数据所有权 | 用户个人                                   | 企业所有                               |
| 存储位置   | 本地文件（rusqlite）                       | 企业 RAG 服务（远程）                  |
| 数据类型   | 个人偏好、交互历史、自传体                 | 产品文档、业务流程、内部规范           |
| 检索方式   | 向量 + 全文 + 关联扩散（图扩展）           | 向量检索 + 可选混合关键词 + 元数据过滤 |
| 隐私边界   | Agent 私有，打包分享时按 PrivacyLevel 过滤 | 企业管理，Agent 只读                   |

RAG 检索结果与本地记忆层 检索结果在瞬态层拼接后统一送入 LLM 上下文，但不整合进 Memory 系统的抽象层——两者查询范式和存储模型完全不同。

**RAG 配置驱动的 Runtime 行为差异**：

| Runtime 行为                   | manifest 无 RAG 声明 | manifest 有 RAG 声明              |
| ------------------------------ | -------------------- | --------------------------------- |
| 步骤② MemoryManager.retrieve() | 仅查 SQLite 记忆通道     | 并行查 SQLite 记忆层 + RAG 双通道        |
| 步骤② 上下文注入               | 仅 SQLite 记忆层检索结果   | SQLite 记忆层 + RAG 结果拼接，按来源标注 |
| 步骤③ LLM Tool Definitions     | 不含 RAG 工具        | 含 RAG 工具（可显式调用）         |
| 步骤⑤ Tool Dispatch            | 无 RAG 工具路由      | RAG 工具 → RagClient HTTP 调用    |

以下操作不属于"工具"（不由 LLM tool_call 触发），而是 Runtime 在特定流程中主动向 Gateway 发起的请求，通过 Gateway Service API 通信：

| 操作                            | 触发时机                      | 说明                                            |
| ------------------------------- | ----------------------------- | ----------------------------------------------- |
| `KeyRelease`                    | 启动握手后                    | 获取 LLM API Key（一次性）                      |
| `IdentityDelivery`              | 启动握手后                    | 获取用户身份信息（由 Gateway 主动推送）         |
| `CapabilityOverview`            | 启动握手后                    | 获取已安装 Agent 的能力摘要（Gateway 主动推送） |
| `IntentSend` / `IntentReceived` | LLM 调用 `intent_send` 工具时 | 跨 Agent 消息路由                               |
| `BudgetQuery`                   | 预算预检时                    | 查询剩余预算                                    |
| `UsageReport`                   | 每轮迭代后（异步）            | 上报 LLM 用量                                   |
| `RateAcquire`                   | 调用 LLM 前                   | 申请速率令牌                                    |
| `PermissionRequest`             | 运行时请求额外权限            | 弹窗让用户确认                                  |

**关键原则：** LLM 调用和工具执行不走 Gateway——Agent 直连 LLM API、本地执行工具。Gateway 只管必须集中化的协调。

## 5. 工具调度流程

Tool Dispatcher 在主循环步骤 ⑤ 中工作（见 03-agent-runtime.md）：

```
LLM 输出 tool_calls: [{name, arguments}, ...]
       │
       ▼
逐个处理每个 tool_call:
       │
       ├─ ① 查找工具定义
       │    ├─ 在 manifest.tools 中找到匹配的 name → 继续
       │    └─ 未找到 → 返回错误 tool result："未知工具"
       │
       ├─ ② 权限校验
       │    ├─ 检查 tool_call 是否需要声明了但未授权的权限
       │    ├─ 权限不足 → 返回错误 tool result
       │    └─ 权限通过 → 继续
       │
       ├─ ③ 路由到工具实现
       │    ├─ type = "builtin" → Built-in Tool 直接执行
       │    ├─ type = "rag" → RagClient HTTP 调用（仅 manifest 声明时注册）
       │    ├─ type = "wasm" → Wasmtime 沙箱执行
       │    └─ intent_send → Gateway 路由
       │
       ├─ ④ 执行工具
       │    ├─ 成功 → 构造 tool result
       │    └─ 失败 → 构造 error tool result（见 3.6）
       │
       └─ ⑤ 追加到对话历史 → 下一轮迭代
```

## 6. 设计决策记录

| 决策                                      | 选择                                                         | 理由                                                                                                                                                                                                                    |
| ----------------------------------------- | ------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| WASM 运行时                               | Wasmtime                                                     | 标准合规性最强（WASI Preview 2 + 组件模型），无厂商锁定，安全审计成熟                                                                                                                                                   |
| 不选 Wasmer                               | WASIX 锁定风险                                               | WASIX 是非标准扩展，二进制只能在 Wasmer 运行；WASI Preview 2 缺失                                                                                                                                                       |
| Wasmi 备选                                | iOS / 嵌入式                                                 | 纯解释器，iOS 禁止 JIT；攻击面极小（2 个依赖），通过安全审计                                                                                                                                                            |
| WASI 版本                                 | Preview 2                                                    | 目录级沙箱 + 能力安全，对不可信工具场景最安全                                                                                                                                                                           |
| Host-WASM 通信                            | Host 函数 + JSON（Phase 1）→ 组件模型（Phase 3+）            | Phase 1 保持简单，组件模型提供长期类型安全升级路径                                                                                                                                                                      |
| 工具执行失败                              | 返回错误给 LLM 决策                                          | LLM 有能力自主调整策略，比直接终止更灵活                                                                                                                                                                                |
| Fuel metering                             | 启用                                                         | 防止恶意/有缺陷的 WASM 工具死循环                                                                                                                                                                                       |
| API Key 对 WASM 不可见                    | secrecy::SecretString                                        | WASM 工具是不可信代码，绝不能拿到 LLM API Key                                                                                                                                                                           |
| SDK 延后                                  | Phase 2+                                                     | Phase 1 手动导出函数足够，SDK 降低门槛但不阻塞核心功能                                                                                                                                                                  |
| Builtin 范围                              | 仅平台基础设施级                                             | SaaS 集成（Jira/Notion/LinkedIn 等）由独立 Agent 提供，不内置；垂直能力走 WASM Tool 或独立 Agent                                                                                                                        |
| web_fetch 内置                             | 是                                                           | 几乎所有 Agent 都需要，是平台级基础设施                                                                                                                                     |
| file_edit/glob_search/content_search 内置 | 是                                                           | 文件操作三件套（读+写+编辑+搜索），缺少任一个都会导致 Agent 用 file_write 模拟低效操作                                                                                                                                  |
| RAG 工具类型                              | 独立 type="rag"，配置驱动 Opt-In                             | 企业 RAG 是外部服务接入，不是内置工具也不是 WASM 工具，需要独立的声明和执行模型；仅 manifest 声明时注册，无 RAG 的 Agent 零侵入                                                                                         |
| RAG 凭据安全                              | Vault 引用，运行时获取                                       | 与内置工具 API Key 管理一致，不明文出现在 manifest 或进程环境变量                                                                                                                                                       |
| RAG 触发模型                              | 混合双触发（自动 + 显式）                                    | 自动触发（步骤② Retrieve）解决"LLM 不知道该不该查"；显式触发（步骤⑤ tool_call）解决"需要更精确查询"；均由 manifest 配置驱动 opt-in（ADR-012，详见 plan-p4.md）                                                          |
| RAG 协议适配                              | ACowork 定义标准协议，企业 RAG 自适配                    | 不为各家 RAG 实现 adapter，企业侧确保其 RAG 服务兼容标准查询接口；遵循 PRD "纯对接，不托管"原则                                                                                                                         |
| identity_store 专用工具                   | 新增第 14~15 个内置工具（identity_query + identity_observe） | 身份数据有专属语义（字段级 confidence、变更通知、结构化查询），memory_store 通用 Fact 节点无法自然承载；专用工具有扩展点（字段校验、敏感加密、跨设备同步优先级）；内部实现基于 MemoryManager store 接口，不绕过记忆架构 |
| identity_store 仅系统 Agent 可用          | 权限 `identity:write`                                        | 系统 Agent 是身份数据唯一写入方；其他 Agent 通过 intent_send 汇报，由系统 Agent LLM 判断后写入                                                                                                                          |
