# ADR-029：Builtin Tools 持久化与使能控制 — agent_tools.json

> **面板位置修订（2026-11-06，[ADR-108](./ADR-108-right-panel-consolidation.md)）**：右侧「工具」面板已下线 —— 本文提到的「工具面板」/「Builtin Tools 卡片」/「MCP 卡片」现在位于**设置面板**（`AgentSetupTab`），入口图标与 Tab 一并消失。端点、Store、权限与开关语义均未改变；改动范围见 ADR-108。

**状态**：草案（待决策）
**日期**：2026-07-17
**决策者**：大鱼
**前置**：ADR-009（Gateway 不再写入 agent workspace 文件）、ADR-015（Agent 启动时序）
**影响范围**：

**新增模块**：
- `core/acowork-runtime/src/agent_config.rs`（新增 `AgentToolsConfig` 结构体 + `load/save` 函数）
- `core/acowork-runtime/src/tools/registry.rs`（`activate()` 新增 `enabled_tools` 过滤参数）

**修改模块**：
- `core/acowork-core/src/protocol.rs`（`RuntimeConfigUpdate` 新增 `builtin_tools_enabled` 字段）
- `core/acowork-runtime/src/agent/agent_core.rs`（`builtin_tools` 改为 `Vec<BuiltinToolEntry>`，每个 entry 带 `enabled` 字段）
- `core/acowork-runtime/src/startup/agent_init.rs`（Phase A 加载 `agent_tools.json` 并过滤工具）
- `core/acowork-runtime/src/cli.rs`（`RuntimeConfigUpdate` handler 处理 `builtin_tools_enabled`）
- `core/acowork-runtime/src/agent/session/session_manager.rs`（新增 `UpdateBuiltinTools` SessionMessage + broadcast）
- `core/acowork-runtime/src/agent/session/session_task.rs`（处理 `UpdateBuiltinTools` 消息）
- `core/acowork-gateway/src/http/agents.rs`（新增 `GET/PUT /api/agents/{id}/builtin-tools` 端点）
- `core/acowork-gateway/src/http/routes.rs`（注册新路由）
- `core/acowork-gateway/src/http/agent_config.rs`（`AgentConfigResponse` 新增 `builtin_tools` 字段）
- `apps/acowork-desktop/src/stores/`（新增 `builtinToolsStore.ts` 或扩展 `mcpStore`）
- `apps/acowork-desktop/src/components/results/ToolsTab.tsx`（新增 Builtin Tools 区域）
- `apps/acowork-desktop/src/i18n/locales/*.json`（新增 i18n key）

---

## 背景

### 现状

当前所有 builtin tools 在 agent 启动时无条件全部激活，没有任何 enable/disable 控制机制：

```rust
// core/acowork-runtime/src/tools/registry.rs:43-44
/// All builtin tools are always active — manifest `[[tools]]` is reserved
/// for future scope restriction, not activation filtering.
```

`manifest.toml` 中的 `[[tools]]` 声明目前仅用于 RAG 工具的 opt-in 注册，对普通 builtin tools 无任何过滤效果。所有示例 agent 的 manifest.toml 中都明确注释：

```toml
# Builtin tools are always active — no need to declare them here.
# The [[tools]] section is reserved for future scope-limiting (optional).
```

### 用户需求

1. 用户需要能够**按 agent 粒度**控制哪些 builtin tools 是 enabled 的
2. 配置数据持久化到 `agent_tools.json`，**必须包含所有 builtin tools**，每个 tool 有 `enabled` 字段
3. 当 `agent_tools.json` 不存在时，初始化数据来自 `manifest.toml` 的 `[[tools]]` 声明；一旦文件存在，它就是唯一数据源
4. 启动流程与 `agent_mcp.json` 一致：持久化文件 → 加载到 `AgentCore` → 运行时生效
5. Gateway 提供 REST API：获取工具列表 + 设置工具使能状态
6. 前端右侧工具面板展示 builtin tools 列表，每个 tool 带 checkbox，点击后调用使能接口

### 现有参考模式

`agent_mcp.json` 提供了完整的参考模板：

| 方面 | agent_mcp.json | agent_tools.json (本 ADR) |
|------|----------------|--------------------------|
| 文件位置 | `{work_dir}/config/agent_mcp.json` | `{work_dir}/config/agent_tools.json` |
| 数据结构 | `AgentMcpConfig { catalog, local }` | `AgentToolsConfig { tools: Vec<AgentToolEntry> }` |
| 加载函数 | `load_agent_mcp_config()` | `load_agent_tools_config()` |
| 保存函数 | `save_agent_mcp_config()` | `save_agent_tools_config()` |
| 初始化来源 | 空文件（无 MCP 时） | `manifest.toml` 的 `[[tools]]` |
| 运行时更新 | `RuntimeConfigUpdate.mcp_servers` | `RuntimeConfigUpdate.builtin_tools_enabled` |
| 热更新机制 | `McpConfigNotifier` → `UpdateMcpTools` | `UpdateBuiltinTools` SessionMessage |

---

## 目标

1. 新增 `agent_tools.json` 持久化文件，包含所有 builtin tools 的 enable 状态
2. 初始化逻辑：文件不存在时从 `manifest.toml` 的 `[[tools]]` 生成；文件存在时以文件为准
3. 启动时加载到 `AgentCore.builtin_tools`（每个 entry 带 `enabled` 字段），用于过滤 `all_tools`
4. Gateway 提供 `GET/PUT /api/agents/{id}/builtin-tools` 两个 REST 端点
5. 运行时通过 `RuntimeConfigUpdate` 推送变更到 Runtime，Runtime 热更新 `AgentCore.all_tools`
6. 前端 ToolsTab 新增 Builtin Tools 区域，checkbox 控制 enable/disable

---

## 方案设计

### 1. 数据模型

#### 1.1 `AgentToolsConfig`（新增，`agent_config.rs`）

```rust
/// Per-agent builtin tools enable/disable configuration.
///
/// Persisted to workspace/config/agent_tools.json.
/// Contains ALL builtin tools — each with an `enabled` flag.
///
/// Initialization priority:
///   1. If agent_tools.json exists → load from file (single source of truth)
///   2. If agent_tools.json does NOT exist → generate from manifest.toml [[tools]]
///      (all declared tools enabled, undeclared tools disabled)
///   3. If manifest has no [[tools]] → all builtin tools enabled (backward compatible)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolsConfig {
    /// All builtin tools with their enable status.
    /// Must include every registered builtin tool.
    pub tools: Vec<AgentToolEntry>,
}

/// A single builtin tool entry in agent_tools.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolEntry {
    /// Tool name (matches Tool::name())
    pub name: String,
    /// Whether this tool is enabled for the agent.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool { true }
```

#### 1.2 `AgentCore` 扩展（`agent_core.rs`）

`builtin_tools` 从 `Vec<Arc<dyn Tool>>` 改为带 `enabled` 标志的包装结构体：

```rust
/// A builtin tool with its enable/disable state.
#[derive(Clone)]
pub struct BuiltinToolEntry {
    /// The tool implementation.
    pub tool: Arc<dyn Tool>,
    /// Whether this tool is enabled for the agent.
    pub enabled: bool,
}

impl BuiltinToolEntry {
    pub fn name(&self) -> &str { self.tool.name() }
    pub fn spec(&self) -> ToolSpec { self.tool.spec() }
}
```

`AgentCore` 中：

```rust
pub struct AgentCore {
    // ... 现有字段 ...

    /// Built-in tool registry — each tool carries an `enabled` flag.
    /// This is the single source of truth for both:
    ///   - Full tool list (for frontend GET /api/agents/{id}/builtin-tools)
    ///   - Enabled subset (for LLM dispatch via all_tools)
    pub(crate) builtin_tools: Vec<BuiltinToolEntry>,

    // ... 现有字段 ...
}
```

`rebuild_all_tools()` 方法只将 `enabled == true` 的 builtin tools 合并到 `all_tools`：

```rust
pub(crate) fn rebuild_all_tools(&mut self) {
    let mut merged: Vec<Arc<dyn Tool>> = self.builtin_tools
        .iter()
        .filter(|e| e.enabled)
        .map(|e| e.tool.clone())
        .collect();
    if let Some(ref mcp) = self.mcp_tools {
        merged.extend(mcp.clone());
    }
    self.all_tools = merged;
}
```

#### 1.3 `RuntimeConfigUpdate` 扩展（`protocol.rs`）

```rust
// 在 RuntimeConfigUpdate 中新增字段：
/// Builtin tools enable/disable configuration.
/// Each entry specifies a tool name and whether it should be enabled.
/// Partial update — only the listed tools are changed; unlisted tools keep current state.
#[serde(default, skip_serializing_if = "Option::is_none")]
builtin_tools_enabled: Option<Vec<AgentToolEntry>>,
```

#### 1.4 `AgentConfigResponse` 扩展（Gateway `agent_config.rs`）

```rust
/// Builtin tools enable/disable configuration (full list with enabled flags).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub builtin_tools: Option<Vec<AgentToolEntry>>,
```

### 2. 初始化流程

```
Agent 启动
    │
    ├── Step A: 注册所有 builtin tools（all_builtin_tools()）
    │   得到全量工具列表（含 platform-dependent shell tools）
    │
    ├── Step B: 检查 {work_dir}/config/agent_tools.json 是否存在
    │       │
    │       ├── 存在 → 从文件加载 AgentToolsConfig
    │       │       │
    │       │       └── Merge: 将文件中的 tools 与代码注册的全量 tools 合并
    │       │               ├── 文件中有、代码中也有 → 保留文件的 enabled 值
    │       │               ├── 代码中有、文件中没有 → 追加，enabled = true
    │       │               │   （Runtime 升级新增 tool 时自动启用）
    │       │               └── 文件中有、代码中没有 → 丢弃
    │       │                   （tool 已被移除，静默清理）
    │       │
    │       └── 不存在 → 从 manifest.toml 生成初始配置
    │               │
    │               ├── 遍历所有已注册 builtin tools
    │               ├── 如果 tool 在 manifest.toml [[tools]] 中 → enabled = true
    │               ├── 如果 tool 不在 manifest.toml [[tools]] 中 → enabled = false
    │               ├── 特殊规则：RAG tool 仅在 manifest 声明时加入列表
    │               ├── 特殊规则：shell tools 按平台检测结果动态加入
    │               └── 保存到 agent_tools.json
    │
    ├── Step C: 构建 AgentCore.builtin_tools
    │   将全量工具列表与 enabled 状态合并为 Vec<BuiltinToolEntry>
    │
    └── Step D: 调用 AgentCore.rebuild_all_tools()
         只将 enabled == true 的工具加入 all_tools
```

#### Merge 逻辑详解（关键）

当 `agent_tools.json` 中的 tools 数量少于代码注册的全量时（例如 Runtime 升级新增了 tool），需要 merge：

```rust
/// Merge code-registered tools with persisted config.
///
/// Rules:
/// - Tools present in both → use persisted `enabled` value
/// - Tools only in code (new tools after upgrade) → append with enabled = false
///   (opt-in: only user-explicitly-enabled tools are true)
/// - Tools only in file (removed tools) → silently dropped
pub fn merge_tools_config(
    code_tools: &[Arc<dyn Tool>],       // from all_builtin_tools()
    persisted: &[AgentToolEntry],        // from agent_tools.json
) -> Vec<BuiltinToolEntry> {
    let persisted_map: HashMap<&str, bool> = persisted
        .iter()
        .map(|e| (e.name.as_str(), e.enabled))
        .collect();

    code_tools.iter().map(|tool| {
        let enabled = persisted_map
            .get(tool.name())
            .copied()
            .unwrap_or(false); // 新 tool 默认禁用（opt-in）
        BuiltinToolEntry {
            tool: tool.clone(),
            enabled,
        }
    }).collect()
}
```

#### 初始化示例

**场景 A：manifest.toml 声明了部分工具**

```toml
# manifest.toml
[[tools]]
name = "memory_recall"

[[tools]]
name = "memory_store"

[[tools]]
name = "shell"
```

生成的 `agent_tools.json`：

```json
{
  "tools": [
    { "name": "memory_recall", "enabled": true },
    { "name": "memory_store", "enabled": true },
    { "name": "http_request", "enabled": false },
    { "name": "web_fetch", "enabled": false },
    { "name": "shell", "enabled": true },
    { "name": "file_read", "enabled": false },
    { "name": "file_write", "enabled": false },
    { "name": "file_edit", "enabled": false },
    { "name": "doc_reader", "enabled": false },
    { "name": "glob_search", "enabled": false },
    { "name": "content_search", "enabled": false },
    { "name": "intent_send", "enabled": false },
    { "name": "ask_user_question", "enabled": false },
    { "name": "codebase", "enabled": false },
    { "name": "todo_write", "enabled": false },
    { "name": "mcp_install", "enabled": false },
    { "name": "mcp_uninstall", "enabled": false }
  ]
}
```

**场景 B：manifest.toml 没有 `[[tools]]`（向后兼容）**

所有 builtin tools 默认 `enabled = true`，与当前行为一致。

**场景 C：agent_tools.json 已存在**

直接加载，manifest.toml 的 `[[tools]]` 被完全忽略。如果文件中的 tools 比代码少（升级新增了 tool），merge 逻辑自动补全。

### 3. 运行时热更新流程

```
用户点击 checkbox
    │
    ▼
Frontend PUT /api/agents/{id}/builtin-tools
    │
    ▼
Gateway 处理请求
    ├── 验证 agent 存在且运行中
    ├── 构建 RuntimeConfigUpdate { builtin_tools_enabled: Some([...]) }
    └── 通过 IPC push 到 Runtime
    │
    ▼
Runtime cli.rs 收到 RuntimeConfigUpdate
    ├── 解析 builtin_tools_enabled 列表（部分更新）
    ├── 合并到当前 AgentCore.builtin_tools（只更新列出的 tool）
    ├── 持久化到 agent_tools.json（全量写入）
    ├── 调用 AgentCore.rebuild_all_tools()
    └── broadcast UpdateBuiltinTools 到所有 session
    │
    ▼
SessionTask 处理 UpdateBuiltinTools
    ├── 更新 agent_loop.core.builtin_tools（enabled 状态）
    ├── 调用 agent_loop.core.rebuild_all_tools()
    └── 更新 context_builder 中的 tool_definitions（LLM 可见）
```

### 4. REST API

#### `GET /api/agents/{id}/builtin-tools`

获取当前 agent 的 builtin tools 配置（全量列表 + enabled 状态）。

**Response 200**：
```json
{
  "agent_id": "com.acowork.senior-engineer",
  "tools": [
    { "name": "memory_recall", "enabled": true },
    { "name": "memory_store", "enabled": true },
    { "name": "http_request", "enabled": false }
  ]
}
```

#### `PUT /api/agents/{id}/builtin-tools`

设置 builtin tools 的 enable 状态。

**Request Body**（部分更新——只传需要变更的 tool）：
```json
{
  "tools": [
    { "name": "http_request", "enabled": true }
  ]
}
```

Gateway 合并到当前配置后推送 Runtime，Runtime 持久化全量到 `agent_tools.json`。

**Response 200**：返回完整的当前配置（同 GET 响应）。

### 5. 前端实现

#### 5.1 Store（`builtinToolsStore.ts`）

```typescript
interface BuiltinToolEntry {
  name: string;
  enabled: boolean;
}

interface BuiltinToolsState {
  tools: BuiltinToolEntry[];
  loading: boolean;
}

interface BuiltinToolsActions {
  loadTools: (agentId: string) => Promise<void>;
  toggleTool: (agentId: string, toolName: string) => Promise<void>;
}
```

#### 5.2 ToolsTab 新增 Builtin Tools 区域

在现有 ToolsTab 中，在 "Web Search Providers" 区域上方新增 "Builtin Tools" 区域：

```tsx
{/* Builtin Tools */}
<div className="mb-3 space-y-1">
  <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
    {t("agentSetup.builtinTools")}
  </label>
  <div className="max-h-48 overflow-y-auto space-y-1 rounded-md border ...">
    {builtinTools.map((tool) => (
      <label key={tool.name} className="flex items-center gap-2 ...">
        <input
          type="checkbox"
          checked={tool.enabled}
          onChange={() => toggleTool(selectedAgentId, tool.name)}
          className="h-3.5 w-3.5 shrink-0 rounded accent-[var(--color-accent)]"
        />
        <span className="text-[11px] font-medium ...">{tool.name}</span>
      </label>
    ))}
  </div>
</div>
```

### 6. 边界情况处理

| 场景 | 处理方式 |
|------|---------|
| **RAG tool** | 仅在 manifest 声明 `[[tools]] type = "rag"` 时出现在列表中；默认 enabled |
| **Shell tools（多平台）** | 按平台检测结果动态加入列表（Windows 可能同时有 bash + powershell） |
| **mcp_install / mcp_uninstall** | 作为普通 builtin tools 出现在列表中，可被 disable |
| **agent_tools.json 损坏** | 回退到 manifest.toml 初始化，记录 warning |
| **新增 builtin tool（Runtime 升级）** | Merge 逻辑自动追加到 `builtin_tools`，默认 `enabled = false`（opt-in） |
| **tool 被从代码中移除** | Merge 逻辑自动丢弃，下次保存 `agent_tools.json` 时清理 |
| **所有 tools 被 disable** | Agent 仍可运行，但无法调用任何 builtin tool（MCP tools 不受影响） |
| **agent_tools.json 与 manifest.toml 冲突** | 以 agent_tools.json 为准（文件存在即唯一数据源） |
| **前端 GET 需要全量列表** | `AgentCore.builtin_tools` 本身就是全量（含 enabled 标志），直接返回 |

### 7. 向后兼容

1. **已有 agent 无 `agent_tools.json`**：首次启动时从 manifest.toml 生成，manifest 无 `[[tools]]` 则全部 enabled
2. **已有 `agent_tools.json` 缺少新 tool**：Merge 逻辑自动补全，新 tool 默认 `enabled = false`（opt-in，用户需显式启用）
3. **API 兼容**：`GET /api/agents/{id}/config` 响应中新增 `builtin_tools` 字段，不影响现有字段
4. **Frontend 兼容**：旧版 Frontend 忽略 `builtin_tools` 字段，所有 tools 保持 enabled
5. **`AgentCore.builtin_tools` 类型变更**：从 `Vec<Arc<dyn Tool>>` 改为 `Vec<BuiltinToolEntry>`，所有引用点需适配 `.tool` 访问

---

## 实施计划

### Phase 1：后端数据层（2-3 天）

1. **`agent_config.rs`**：新增 `AgentToolsConfig` / `AgentToolEntry` 结构体 + `load_agent_tools_config()` / `save_agent_tools_config()` 函数 + `merge_tools_config()` 合并函数
2. **`manifest.rs`**：新增 `AgentManifest::builtin_tool_names()` 方法，返回所有声明的 builtin tool 名称列表
3. **`agent_core.rs`**：新增 `BuiltinToolEntry` 结构体 + `builtin_tools` 改为 `Vec<BuiltinToolEntry>` + 修改 `rebuild_all_tools()` 过滤逻辑
4. **`registry.rs`**：`activate()` 新增 `enabled_tools: &[BuiltinToolEntry]` 参数，只激活 enabled 的工具

### Phase 2：初始化流程（1-2 天）

5. **`agent_init.rs`**：Phase A 中加载 `agent_tools.json`，不存在时从 manifest 生成，merge 后传入 `registry.activate()`
6. **`cli.rs`**：`RuntimeConfigUpdate` handler 处理 `builtin_tools_enabled` 字段

### Phase 3：热更新机制（1-2 天）

7. **`protocol.rs`**：`RuntimeConfigUpdate` 新增 `builtin_tools_enabled: Option<Vec<AgentToolEntry>>`
8. **`session_task.rs`**：新增 `UpdateBuiltinTools` SessionMessage 变体 + handler
9. **`session_manager.rs`**：新增 `apply_builtin_tools()` 方法 + broadcast

### Phase 4：Gateway API（1 天）

10. **`agents.rs`**：新增 `get_agent_builtin_tools()` / `update_agent_builtin_tools()` handler
11. **`routes.rs`**：注册 `GET/PUT /api/agents/{id}/builtin-tools` 路由
12. **`agent_config.rs`（Gateway）**：`AgentConfigResponse` 新增 `builtin_tools` 字段

### Phase 5：前端实现（1-2 天）

13. **`builtinToolsStore.ts`**：新增 Zustand store，管理 builtin tools 状态
14. **`ToolsTab.tsx`**：新增 Builtin Tools 区域，checkbox 控制 enable/disable
15. **`locales/*.json`**：新增 i18n key

### Phase 6：测试（1 天）

16. **单元测试**：`AgentToolsConfig` 序列化/反序列化回环测试、merge 逻辑测试、初始化逻辑测试
17. **集成测试**：`GET/PUT /api/agents/{id}/builtin-tools` 端点测试
18. **前端测试**：checkbox toggle → API 调用 → 状态更新

---

## 不（明确边界）

- 不修改 `manifest.toml` 的 `[[tools]]` 语义——manifest 仅作为初始化数据源
- 不涉及 MCP tools 的 enable/disable（MCP 已有独立的 `agent_mcp.json` + catalog 机制）
- 不涉及 WASM tools 的 enable/disable（WASM 是独立子系统）
- 不涉及 permission 变更——tool 被 disable 只是不注册到 LLM，不改变 permission 声明
- 不修改 `Tool` trait——enable/disable 是注册层逻辑，不是 tool 自身行为

---

## 附录：文件变更清单

| 文件 | 变更类型 | 说明 |
|------|---------|------|
| `core/acowork-runtime/src/agent_config.rs` | 新增 | `AgentToolsConfig` + `AgentToolEntry` + load/save/merge |
| `core/acowork-runtime/src/agent/agent_core.rs` | 修改 | 新增 `BuiltinToolEntry` + `builtin_tools` 类型变更 + `rebuild_all_tools` 过滤 |
| `core/acowork-runtime/src/tools/registry.rs` | 修改 | `activate()` 新增 `enabled_tools` 参数 |
| `core/acowork-runtime/src/startup/agent_init.rs` | 修改 | Phase A 加载 agent_tools.json + merge |
| `core/acowork-runtime/src/cli.rs` | 修改 | RuntimeConfigUpdate handler |
| `core/acowork-runtime/src/agent/session/session_task.rs` | 修改 | 新增 `UpdateBuiltinTools` |
| `core/acowork-runtime/src/agent/session/session_manager.rs` | 修改 | 新增 `apply_builtin_tools()` |
| `core/acowork-core/src/protocol.rs` | 修改 | RuntimeConfigUpdate 新增字段 |
| `core/acowork-core/src/manifest.rs` | 修改 | 新增 `builtin_tool_names()` |
| `core/acowork-gateway/src/http/agents.rs` | 修改 | 新增 builtin-tools 端点 |
| `core/acowork-gateway/src/http/routes.rs` | 修改 | 注册新路由 |
| `core/acowork-gateway/src/http/agent_config.rs` | 修改 | 响应新增字段 |
| `apps/acowork-desktop/src/stores/builtinToolsStore.ts` | 新增 | Zustand store |
| `apps/acowork-desktop/src/components/results/ToolsTab.tsx` | 修改 | 新增 Builtin Tools 区域 |
| `apps/acowork-desktop/src/i18n/locales/*.json` | 修改 | 新增 i18n key |
