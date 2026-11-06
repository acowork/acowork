# ADR-069: MCP 工具级别 opt-in — agent_mcp_tools.json

> **面板位置修订（2026-11-06，[ADR-108](./ADR-108-right-panel-consolidation.md)）**：右侧「工具」面板已下线 —— 本文提到的「工具面板」/「Builtin Tools 卡片」/「MCP 卡片」现在位于**设置面板**（`AgentSetupTab`），入口图标与 Tab 一并消失。端点、Store、权限与开关语义均未改变；改动范围见 ADR-108。

**状态**：已接受
**日期**：2026-09-22
**决策者**：大鱼
**前置**：ADR-029（Builtin tools 持久化与使能控制）、ADR-065（统一 MQTT 客户端生命周期）
**影响范围**：

**新增模块**：
- `core/acowork-runtime/src/agent_config.rs`（新增 `AgentMcpToolsConfig` / `AgentMcpToolItem` 结构体 + `load/save/merge_mcp_tools_config` 函数）
- `core/acowork-runtime/src/tools/mcp_manager.rs`（`connect()` 增加 reconcile+filter；新增 `reconcile_and_persist_mcp_tools` / `connect_mcp_with_reconcile_and_filter`）
- `core/acowork-runtime/src/http/server.rs`（新增 `GET/PUT /agents/{id}/mcp-tools` 路由）
- `apps/acowork-desktop/src/lib/types.ts`（新增 `AgentMcpToolItem` 类型）
- `apps/acowork-desktop/src/components/results/ToolsTab.tsx`（MCP server 折叠卡片 + switch 列表）

**修改模块**：
- `core/acowork-runtime/src/agent/session/session_manager.rs`（连接 MCP 前 `set_work_dir`，走 reconcile）
- `core/acowork-runtime/src/startup/subsystems.rs` / `startup/gateway_loop.rs`（启动加载 + 热重载走 reconcile）

---

## 背景

### 现状

ADR-029 已经把 builtin tools 做到了 per-tool 的 opt-in 控制（`agent_tools.json` + 每个工具带 `enabled` 标志）。但 MCP 工具目前**只有 server 级开关**（`agent_mcp.json::active_names`），打开某个 server 后它的**所有工具**都会注入到 LLM 的 `tool_definitions`：

```rust
// 改前：全量注入
for prefixed_name in registry.tool_names() {
    if let Some(def) = registry.get_tool_def(&prefixed_name) {
        let wrapper = McpToolWrapper::new(prefixed.clone(), def, registry.clone());
        ...
    }
}
```

每个 MCP server 的工具名 (`mcp_<server>__<tool>`) 会出现在 `tool_definitions`，每个 tool schema 平均 250-400 tokens，server 暴露 N 个工具 = N × 300 tokens 的 system prompt 开销。

### 用户痛点

以 `pm` MCP server 为例，它暴露 12 个工具，但角色分工差异巨大：

| 工具 | 主要使用者 | 普通 agent 是否需要 |
|------|------------|---------------------|
| `pm_list_my_tasks` / `pm_claim_task` / `pm_submit_task` / `pm_check_task` | 工程师 | 是 |
| `pm_create_project` / `pm_create_task` / `pm_list_projects` / `pm_get_project` / `pm_list_tasks` / `pm_get_task` / `pm_update_task` / `pm_reparent_task` | PM | 否 |

普通 agent 拿到全部 12 个工具 ≈ 3600 tokens 纯噪声塞进 system prompt，PM agent 才需要全集。**全量注入完全浪费上下文空间。**

### 设计原则（本次讨论确定）

1. **后端是全量工具列表的唯一数据源**。前端只负责交互和对后端数据的展示，绝不维护任何硬编码的工具清单或默认值。如果接口提供的数据不够前端展示，改的是后端，不是让前端拷贝一份。
2. **接口提供完整列表**：每个工具条目带 `name` + `enabled` + `description`，前端拿到数据直接渲染，不需要拼接两个数据源。
3. **不写 manifest**：MCP 工具子集不进 `manifest.toml`，仍由 Gateway 默认值 + 用户 UI 调整。
4. **不留数据兼容代码**：项目在开发中，文件格式演进直接报错提示删除，不做静默迁移（`deny_unknown_fields`）。

---

## 目标

1. 新增 `agent_mcp_tools.json` 持久化文件，**按 MCP server 粒度存全量工具列表**，每个工具带 `enabled` 开关。
2. 启动/重连时后端用 MCP server 实际 `tools/list` **reconcile** 全量工具列表：刷新 `description`，保留用户 `enabled` 选择，为新发现工具按默认策略赋初值，然后持久化。
3. `McpManager::connect()` 按 reconcile 后的 `enabled` 过滤，丢弃未启用工具（防 server 升级漂移）。
4. 新增 `GET/PUT /agents/{id}/mcp-tools` HTTP API，返回/接收**完整工具列表**，前端直接渲染 + 逐行 toggle。
5. 启动加载 + 文件 watcher 热重载（沿用 MCP config 的现有机制）。

---

## 非目标

- **不动** `manifest.toml` 语法：MCP 工具子集不进入 manifest。
- **不动** `agent_mcp.json` 的 `active_names`：server 级开关保持现状，本 ADR 只在 server 已激活时增加 tool 级过滤。
- **不动** `is_system_injected_mcp_name`：仍只针对 `pm`。
- **不引入** proto 改动：`AvailableMcps.mcp_refs` 不新增字段。
- **不做** 旧格式自动迁移：`agent_mcp_tools.json` 若为 v1 形状（如 `{"pm": {"enabled_tools": [...]}}`），解析直接失败并提示删除，无自动迁移代码。

---

## 设计

### 数据结构（agent_mcp_tools.json）

新增文件 `workspace/config/agent_mcp_tools.json`，**扁平全量列表**：

```json
{
  "servers": {
    "pm": [
      { "name": "pm_list_my_tasks", "enabled": true,  "description": "List tasks assigned to me" },
      { "name": "pm_claim_task",    "enabled": true,  "description": "Claim a task" },
      { "name": "pm_submit_task",   "enabled": true,  "description": "Submit work results" },
      { "name": "pm_check_task",    "enabled": true,  "description": "Check approval status" },
      { "name": "pm_list_projects", "enabled": false, "description": "List all projects" },
      { "name": "pm_get_project",   "enabled": false, "description": "Get project details" }
    ]
  }
}
```

wire shape 与 HTTP 响应/请求体、前端渲染三处一致，前端零拼装。

```rust
/// Per-agent MCP tools config (flat per-server list).
///
/// Wire shape matches the GET response and PUT request body — three-way
/// identity between `agent_mcp_tools.json`, the HTTP wire format, and
/// the desktop render.
///
/// `deny_unknown_fields` ensures a v1-shape file
/// (`{"pm": {"enabled_tools": [...]}}`) fails to parse rather than
/// silently mapping to an empty config — no automatic migration by
/// design (project is in active development).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentMcpToolsConfig {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub servers: HashMap<String, Vec<AgentMcpToolItem>>,
}

/// Single MCP tool row inside a server's flat list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMcpToolItem {
    pub name: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
```

### 默认子集常量（纯后端，前端不可见）

```rust
/// Default `enabled_tools` subset for the `pm` system-injected MCP.
///
/// Picked so a regular (non-PM-role) agent gets the minimum useful
/// surface: read my own tasks, claim one, submit work, check approval
/// status. Anything else (project CRUD, task tree manipulation,
/// reparenting) belongs to PM-role agents, who can extend this via the
/// Tools panel.
pub const PM_DEFAULT_ENABLED_TOOLS: &[&str] = &[
    "pm_list_my_tasks",
    "pm_claim_task",
    "pm_submit_task",
    "pm_check_task",
];
```

> **前端绝不引用这个常量**——它完全存在于后端契约侧，由 `merge_mcp_tools_config` 在启动时物化进 `agent_mcp_tools.json`。前端拿到的永远是全量列表 + 每个条目的 `enabled`。

### 合并（reconcile）逻辑

`merge_mcp_tools_config` 把持久化选择与 MCP server 的实时 `tools/list` 合并，产出**全量**列表并持久化：

```rust
/// Reconcile persisted MCP tool choices with the live `tools/list`
/// from each connected server. Servers absent from `server_tools` are
/// dropped. The persisted row's `enabled` flag wins when present;
/// otherwise the server default decides. `description` is always
/// refreshed from the live `tools/list`.
pub fn merge_mcp_tools_config(
    persisted: &AgentMcpToolsConfig,
    server_tools: &HashMap<String, Vec<McpToolDescriptor>>,
) -> AgentMcpToolsConfig { ... }
```

合并规则（每条工具一行）：
1. **已有持久化行** → 保留用户的 `enabled` 选择（用户关掉的不会因重启被改回）。
2. **新发现的工具**（server 升级新增）→ 按 `default_enabled_tools_for(server)` 赋初值：`pm` 走 `PM_DEFAULT_ENABLED_TOOLS`，其他 server 默认全部 `enabled = true`。
3. **description** → 永远从实时 `tools/list` 刷新，防止 schema 变化产生过期描述。
4. **server 从 `tools/list` 消失** → 该 server 整段丢弃。

### 过滤逻辑

`McpManager::connect()` 增加 `work_dir` reconcile 前置步骤，然后按 reconcile 结果过滤：

```rust
pub async fn connect(
    &mut self,
    configs: &[McpServerConfigDef],
    tools_cfg: &AgentMcpToolsConfig,
) -> McpConnectResult {
    // 1. connect_all 拿全量工具
    // 2. 若 work_dir 非空 → reconcile_and_persist_mcp_tools(work_dir, &registry)
    //    （空 work_dir = 单元测试 → 用调用方传入的 tools_cfg 原样）
    // 3. 遍历 tool_names()，tool_allowed(&active_cfg, server, tool) 为 false 的跳过
    ...
}
```

`tool_allowed` 语义（flat list 版本）：

```rust
fn tool_allowed(tools_cfg: &AgentMcpToolsConfig, server_name: &str, tool_name: &str) -> bool {
    match tools_cfg.servers.get(server_name) {
        None => true,                                  // server 不在配置 → 放行
        Some(rows) => tool_enabled_in(rows, tool_name).unwrap_or(false),
        // row 存在 → 直接用它；row 缺失 → 保守不暴露
    }
}
```

### 落盘路径与辅助函数

```rust
// agent_config.rs
pub fn load_agent_mcp_tools_config(work_dir: &Path) -> Result<Option<AgentMcpToolsConfig>, String>
pub fn save_agent_mcp_tools_config(work_dir: &Path, cfg: &AgentMcpToolsConfig) -> Result<(), String>
pub fn merge_mcp_tools_config(
    persisted: &AgentMcpToolsConfig,
    server_tools: &HashMap<String, Vec<McpToolDescriptor>>,
) -> AgentMcpToolsConfig

// mcp_manager.rs
pub fn collect_server_tools_from_registry(registry: &McpRegistry)
    -> HashMap<String, Vec<McpToolDescriptor>>
pub fn reconcile_and_persist_mcp_tools(work_dir: &Path, registry: &McpRegistry)
    -> AgentMcpToolsConfig
pub async fn connect_mcp_with_reconcile_and_filter(
    work_dir: &Path,
    configs: &[McpServerConfigDef],
) -> McpConnectResult
```

### HTTP API

`GET /agents/{id}/mcp-tools`：

```json
{
  "agent_id": "dev",
  "servers": {
    "pm": [
      { "name": "pm_list_my_tasks", "enabled": true,  "description": "..." },
      { "name": "pm_claim_task",    "enabled": true,  "description": "..." }
    ]
  }
}
```

`PUT /agents/{id}/mcp-tools`：请求体与 GET 响应同构（`servers` → 每个 server 的完整 `AgentMcpToolItem` 列表），后端直接覆盖持久化 `agent_mcp_tools.json`，并广播 MCP 工具变更触发重连。前端 toggle 一行就发一次 PUT，**前端不维护任何默认值**。

### 前端

`ToolsTab.tsx`：
- MCP server 列表参照 DEBUG 面板 PROMPT 布局风格，**默认折叠**，点击展开二级列表。
- 二级列表 = 完整工具列表，每行一个工具 + 统一 switch 组件，缩进于一级列表。
- 数据全部来自 `GET /agents/{id}/mcp-tools`，`enabled` 直接渲染开关；用户切开关 → `PUT` 局部更新。**删除** `KNOWN_PM_TOOLS` / `PM_DEFAULT_ENABLED_TOOLS` 等硬编码。

---

## 兼容性与迁移

- 旧 v1 文件（`{"pm": {"enabled_tools": [...]}}`）会被 `deny_unknown_fields` 拒绝，日志提示删除文件后重建。**不做**静默迁移——项目在开发中，不留数据兼容垃圾代码。
- 唯一迁移动作是用户手动删除旧文件，由启动 reconcile 重新物化全量默认列表。

---

## 测试

- `merge_mcp_tools_config`：空持久化 + 12 个 pm 工具 → 12 行，`enabled` 匹配 `PM_DEFAULT_ENABLED_TOOLS`；非系统 server 全 `true`；用户选择覆盖默认；description 刷新；server 消失丢弃。
- `tool_allowed`：server 缺失放行；row 缺失保守关闭；row.enabled 直接生效；用户关闭优先于默认。
- `load_agent_mcp_tools_config`：文件缺失 → None；v1 形状 → 报错；合法文件 → 解析成功。
- `save_agent_mcp_tools_config`：原子写（tmp+rename）。
- 端到端：连接 `pm` server → reconcile 落盘 12 行默认 4 真 → 注入工具 = 4 个。
