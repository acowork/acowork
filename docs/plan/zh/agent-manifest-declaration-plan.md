# Agent manifest 能力声明重构方案

> 状态：v2（已按评审意见补 §7 推荐 MCP 前端交互、§8 Create Agent dialog 全字段覆盖；仍未改动生产代码）
> 范围：`manifest.toml` 声明面 —— config 默认值 / MCP 推荐声明 + 其前端作用 / capability 与 skill 的关系 / labels / 创建向导
> 关联：ADR-009、ADR-017、ADR-025、ADR-026、ADR-029、ADR-069、ADR-072、ADR-090、ADR-094、ADR-101

---

## 1. 现状事实（代码取证）

### 1.1 manifest 今天声明了什么

[core/acowork-core/src/manifest.rs:52](core/acowork-core/src/manifest.rs#L52) 的 `AgentManifest`：身份字段、`permissions`、`triggers`、`[llm]`、`[memory]`、`identity_deps`、`[[tools]]`（[:105](core/acowork-core/src/manifest.rs#L105)）、`[capabilities.*]`（[:108](core/acowork-core/src/manifest.rs#L108)）、`[resources]`、`[sandbox]`、`[skills] progressive`（[:120](core/acowork-core/src/manifest.rs#L120)）。

**没有** MCP 声明，**没有** `agent_config.json` 的默认值声明，**没有**标签。

### 1.2 `agent_config.json` 的落地路径并不统一

- `temperature` / `context_window`：`[llm].temperature` / `[llm].context_window` 是唯一 manifest 来源，首启由 [core/acowork-runtime/src/startup/session_init.rs:634](core/acowork-runtime/src/startup/session_init.rs#L634) 的 "Resolve & persist defaults" 块逐字段回填。
- `max_output_tokens`：`agent_config.json` 里**没有** manifest 来源；而 `[llm].max_tokens` 走的是**另一条通道** —— [core/acowork-runtime/src/agent/context.rs:695](core/acowork-runtime/src/agent/context.rs#L695) 直接把它当请求级 `max_tokens` 硬覆盖。同名字段、两套语义、两条链路。
- `max_iterations` / `max_sessions` / `shell_approval_threshold` / `approval_timeout_secs` / `compression_ratio_threshold`：来源是 `RuntimeConfig` 或代码常量，作者无法表达。
- `session_language` / `search` / `chat_router` / `system_prompt_override`：属用户或平台域，不应由包声明。

### 1.3 MCP 的三份状态，包无法参与

[core/acowork-runtime/src/agent_config.rs:25](core/acowork-runtime/src/agent_config.rs#L25) `AgentMcpConfig { catalog, local, active_names }`：`catalog` 由 Gateway 推送，`local` 由 agent 自己的 `mcp_install` 装，`active_names = None` 表示"一个都不激活"（[:81](core/acowork-runtime/src/agent_config.rs#L81)）。

预设注册表在**前端**：`apps/acowork-desktop/src/lib/mcp-presets.ts`，id 为 `playwright` / `brave-search` / `context7` 等；Gateway 与 Runtime 都不认识这些 id。

因此今天唯一"按包定制 MCP 工具子集"的能力，是 Rust 里的硬编码：

```
PM_DEFAULT_ENABLED_TOOLS          agent_config.rs:1067
default_enabled_tools_for("pm")   agent_config.rs:1077
```

包声明层面的 MCP 缺口，实际被一个 `match server_name { "pm" => ... }` 兜住了。

### 1.4 capability 的真实作用已经很薄

安装时 `register_from_manifest` 注册为 `{instance_id}:{action}`（[core/acowork-gateway/src/capability/registry.rs:86](core/acowork-gateway/src/capability/registry.rs#L86)）。但：

- `find_by_action` 已标注 deprecated（[:131](core/acowork-gateway/src/capability/registry.rs#L131)），"谁能做 X"的选路机制已随 ADR-073 退场；
- Intent 投递闸门只校验 `permissions` 的 `IntentSend` / `IntentReceive`（[core/acowork-gateway/src/intent/gate.rs:37](core/acowork-gateway/src/intent/gate.rs#L37)），**不看** capability。

结论：capability 今天只剩"展示 + handshake overview"。它的语义本应是**对外 intent 契约**（名字 + 输入输出 schema），但没有任何机制要求它和内部实现（skills / tools）对齐。

### 1.5 skills 的唯一来源是磁盘，而 manifest 侧没有它的位置

`SkillRegistry::load_from_dir` 扫描 `skills/*/SKILL.md`，frontmatter 自带 `name / description / triggers / tool_deps`（[core/acowork-runtime/src/skills/parser.rs:15](core/acowork-runtime/src/skills/parser.rs#L15)）。skill 还能在运行期后装（`install_skill_package`，`core/acowork-node/src/package/skills.rs:113`）。

而 `tool_deps` 在生产代码里**零消费** —— `check_tool_deps`（[:312](core/acowork-runtime/src/skills/parser.rs#L312)）与 `find_by_tool_dep` 只有单测在调。

现场证据：CreateWizard 把每个 skill 1:1 写成一个 capability（`apps/acowork-desktop/src/components/agent-list/CreateWizard.tsx:627`）。这就是"capability 与 skills 没有关联性、又被硬绑"的实际来源。

### 1.6 根因

两个层级没分开、四类对象没分开：

- 层级：(1) 作者出厂默认 → (2) 用户运行时状态 → (3) 代码兜底常量。今天三者互相越界（`[llm].max_tokens` 绕过第 2 层、`PM_DEFAULT_ENABLED_TOOLS` 用第 3 层代替第 1 层）。
- 对象：capability（对外契约）/ skill（内部流程）/ tool（执行原语）/ mcp（工具供给源）语义重叠，且互相之间没有任何可校验的引用关系。

---

## 2. 设计原则（不变量）

1. **单写入者**：manifest 是出厂默认与能力声明的唯一入口；`agent_*.json` 是用户运行时状态的唯一存储；代码常量只是"都没有声明时"的兜底。一个事实只有一个写入路径。
2. **四类对象各归其位**：
   - tool（builtin / WASM / RAG）：执行原语，`[[tools]]` 声明 opt-in（现状保持不变）；
   - mcp server：工具供给源，`[[mcp_servers]]` 声明**推荐**，安装与启用仍是用户动作；
   - skill：内部流程，唯一来源是包内 `skills/*/SKILL.md`（运行期可增删，manifest 不复制）；
   - capability：对外 intent 契约，可被其他 agent 寻址的动作。
3. **包内不出现可执行内容**的老原则不破；manifest 声明的 MCP 也**不携带安装动作的自动执行权**（ADR-090：安装由 Runtime 在用户首次启用时执行）。
4. **向后兼容**：新增字段一律 optional + serde default；老包零改动可解析；新字段只影响"首次安装 / 首次启动的 seed"，不影响已存在的用户配置。
5. **声明图可校验**：capability → skill、skill → tool/mcp 的引用必须在发布期或安装期能被静态检查，检查失败的语义是包的问题，不是运行期静默降级。

---

## 3. 方案一：`[config]` —— manifest 侧的 `agent_config.json` 默认值

### 3.1 字段白名单

只有"作者可调的行为默认值"进 `[config]`：

| 字段 | 类型 | 校验 | 落地目标 |
|---|---|---|---|
| `temperature` | `Option<f32>` | 0.0–2.0 | `AgentConfig.temperature` |
| `context_window` | `Option<u64>` | > 0 | `AgentConfig.context_window` |
| `max_output_tokens` | `Option<u64>` | > 0 | `AgentConfig.max_output_tokens`（每模型输出上限语义） |
| `max_iterations` | `Option<u32>` | > 0 | `AgentConfig.max_iterations` |
| `compression_ratio_threshold` | `Option<f64>` | 0.05–0.95 | `AgentConfig.compression_ratio_threshold` |

明确排除，并给出理由（写进 ADR，避免以后又"顺手加一个"）：

| 排除项 | 理由 |
|---|---|
| `shell_approval_threshold` / `approval_timeout_secs` | 安全边界归用户与平台，包不得降低审批门槛 |
| `max_sessions` | 宿主资源配额，不是 agent 属性 |
| `session_language` | 身份偏好，已有 `identity_deps` 通道 |
| `system_prompt_override` | 用户覆盖域，包已通过 `prompts/` 表达 |
| `search` / `chat_router` | 平台内部检索/路由策略，作者不该感知 |

### 3.2 与 `[llm].temperature` / `[llm].context_window` 的关系

迁移到 `[config]`，`[llm]` 的两个字段保留解析但标记 deprecated：

- 优先级 `[config]` > `[llm]`；
- 打包/发布期若检测到 `[llm].temperature|context_window|max_tokens` 且没有对应 `[config]` 值，给 warning（不拒绝）；
- `[llm].max_tokens` 迁为 `[config].max_output_tokens`，但**语义差异必须写清楚**：`max_tokens` 是请求级硬覆盖（赢过模型能力），`max_output_tokens` 是每模型上限（受模型能力约束）。迁移后硬覆盖通道保留一个版本周期供兼容读，然后删除。

### 3.3 落地入口（单一函数）

```rust
// acowork-core：纯投影，无 IO，可单测
pub struct ManifestConfigDefaults {          // 全部 Option，None = 作者没意见
    pub temperature: Option<f32>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_iterations: Option<u32>,
    pub compression_ratio_threshold: Option<f64>,
}
impl AgentManifest {
    pub fn config_defaults(&self) -> ManifestConfigDefaults;  // 含 [llm] 兼容读
}
```

```rust
// acowork-runtime：唯一 seed 入口，替换 session_init.rs:634 那段 if is_none 序列
pub fn seed_agent_config_from_manifest(
    manifest: &AgentManifest,
    runtime_cfg: &RuntimeConfig,
    cfg: &mut AgentConfig,
) -> bool;   // 返回是否 dirty（是否需要回写文件）
```

约束：`seed_*` **绝不覆盖非 None 值**（用户状态优先），且必须幂等（连续两次调用第二次返回 `false`）。测试边界就是这两条 —— 一次 seed 的字段来源矩阵 + 幂等性，替代今天散落在 session_init 里的 6 个 `if is_none` 分支。

### 3.4 前端占位

`AgentConfigResponse` 已有 `manifest_temperature` / `manifest_context_window`（[core/acowork-gateway/src/http/agent_config.rs:37](core/acowork-gateway/src/http/agent_config.rs#L37)）。补齐 `manifest_max_output_tokens` / `manifest_max_iterations` / `manifest_compression_ratio_threshold`，用于 Setup 面板的 placeholder：**空 = 用作者默认，清空即回退**，与 ADR-017 的头像回退语义一致。

---

## 4. 方案二：`[[mcp_servers]]` —— 推荐 MCP 供给声明

### 4.1 粒度评估（这是本方案的第一个实质决策）

| 粒度档 | 声明内容 | 可解析性 | 可校验性 | 结论 |
|---|---|---|---|---|
| 语义档 | "我需要联网检索能力" | ✗ 无映射到具体 server 的规则 | ✗ | 否决 |
| **服务器档** | name + 预设引用 / 托管引用 或 transport+url + `required_secrets` | ✓ | 安装期可查重、运行期可查激活 | **采纳为声明单位** |
| **工具档** | 该 server 下期望的工具名白名单 | ✓ | 启用后可对 `tools/list` 校验缺口 | **采纳为子句** |
| schema 档 | 工具 inputSchema 副本 | ✓ | ✗ 与 server 自述重复，必然过期 | 否决 |

结论：**服务器档 + 工具名白名单**。不复制任何工具 schema —— MCP `tools/list` 是工具契约的唯一来源；manifest 只声明"我要哪些"。

### 4.2 声明形态

```toml
# 形态 A：引用市场预设（推荐，包内无可执行内容）
[[mcp_servers]]
name = "brave-search"                 # 与 agent_mcp.json 的 name 同键：去重键、激活键
preset = "brave-search"               # 平台预设 id（见 4.4 前置依赖）
why = "联网检索最新资料"
tools = ["brave_web_search"]          # 期望工具子集，用户启用后作为默认 enabled
required = true                       # 缺失时以"能力受限"提示，不阻断启动

# 形态 B：引用 Gateway 托管的 MCP（catalog 自动注入的 pm / doc / memory）
[[mcp_servers]]
hosted = "pm"
why = "任务与项目管理数据"
tools = ["pm_list_my_tasks", "pm_claim_task", "pm_submit_task", "pm_check_task"]
required = true

# 形态 C：内联远程服务器（企业自建 MCP）
[[mcp_servers]]
name = "corp-kb"
transport = "http"
url = "https://mcp.corp.example.com/mcp"
required_secrets = ["CORP_MCP_TOKEN"]
tools = ["kb_search"]
```

形态 B 的必要性来自现状：`pm` / `doc` / `memory` 三个 MCP 由 Gateway 自动注入每个 agent 的 catalog（`core/acowork-gateway/src/mqtt/global_resources_builders.rs:335`），包无从"推荐安装"，但**确实需要声明"我用其中哪些工具"**。`hosted` 形态正是 `PM_DEFAULT_ENABLED_TOOLS` 那个硬编码的声明化替代。

v1 **禁止内联 stdio**（`transport = "stdio"` + `command`）。理由：stdio 声明等于"在你的机器上跑任意命令"，而它以"推荐"形态出现，收益远小于风险 —— 需要 stdio 的场景一律走预设引用（预设本身已经带了 `install` 规格与 `requiredEnv`，安装走 ADR-072 的管线）。若后续确需支持，必须新增 `Permission::McpInstall(Option<String>)` 作为门槛，这是独立决策。

`validate()` 新增规则：`preset` / `hosted` / 内联（`transport`+`url`）三者互斥且必居其一；`preset`/`hosted` 形态的 `name` 可省略（默认等于 preset id / hosted id）；内联 `http`/`sse` 要求 `permissions` 中存在覆盖该 URL 的 `Network` 权限；`tools`、`required_secrets` 名字非空且去重。

### 4.3 落地路径（推荐 ≠ 安装）

关键设计：**`[[mcp_servers]]` 落到一个独立的 `recommended` 列表，绝不写 `catalog` 或 `local`。**

```rust
// agent_mcp.json 第三列表（新增，serde default = 空）
pub struct AgentMcpConfig {
    pub catalog: Vec<McpServerConfigDef>,      // Gateway 推送（不变）
    pub local: Vec<McpServerConfigDef>,        // agent 自己装的（不变）
    pub recommended: Vec<RecommendedMcpServer>,// 包声明的推荐（新增，= manifest 投影）
    pub dismissed: Vec<String>,                // 用户忽略掉的推荐名（新增，用户状态）
    pub active_names: Option<Vec<String>>,     // 用户勾选（不变）
}

pub struct RecommendedMcpServer {
    pub name: String,
    pub preset: Option<String>,
    pub hosted: Option<String>,
    pub transport: McpTransportDef,
    pub url: Option<String>,
    pub required_secrets: Vec<String>,
    pub tools: Vec<String>,
    pub required: bool,
    pub why: Option<String>,
}
```

- 写入者：`recommended` 由安装/启动时从 manifest 投影（无用户状态成分）；`dismissed` 只由用户操作写入。
- 激活：`active_names` 保持不变，推荐项**不自动激活**。这就是 ADR-090 的语义：包可以建议，用户决定装不装、开不开。
- 启用后的工具默认子集：`tools` 白名单成为该 server 在 `agent_mcp_tools.json` 里的初始 `enabled` 集合。**这直接取代 `PM_DEFAULT_ENABLED_TOOLS` 的硬编码**，删除 `default_enabled_tools_for("pm")`。
- 运行期对账：MCP 连接建立、`tools/list` 返回后，比对 `recommended.tools`，缺失的记为 gap 并在 Tools 面板提示（工具级，不是服务器级 —— 服务器升级导致的工具改名必须可见）。
- **前端如何让这些声明真正起作用：见 §7。** 没有 §7 的交互，本节只是一堆没人读的 JSON。

### 4.4 前置依赖：预设注册表必须上移

今天预设只在 Desktop 前端（`apps/acowork-desktop/src/lib/mcp-presets.ts`）。manifest 里的 `preset = "brave-search"` 在移动端、在 Gateway API、在任何非 Desktop 客户端都解析不到。

因此本方案 Phase 3 的前置是：**把预设表上移为平台数据**（Gateway 内嵌静态表 + `GET /api/mcp-presets`），Desktop 改为从 API 读、本地表降级为离线兜底。否则 `preset` 引用只是把前端的隐式契约搬进了包里，跨端必坏。

### 4.5 推荐安装落 catalog，不落 local（决策）

结论：**`[[mcp_servers]]` 的推荐项被用户点「安装并启用」后，写入的是 Gateway 的全局 catalog（`POST /api/mcp-catalog`），不是该 agent 的 `local` 列表。**

事实依据（代码取证）：

| 事实 | 位置 |
|---|---|
| catalog 是**机器级共享注册表**，注释原文："a shared registry of server definitions (including credentials/API keys) that all agents can selectively activate. Analogous to the Vault for LLM providers" | `core/acowork-gateway/src/http/mcp_catalog_api.rs:3` |
| catalog 持久化在 Gateway 的 `data_dir/mcp_catalog.json`，由 Gateway 推送给每个 agent | 同上 `catalog_path()` |
| `local` 是**实例级**，由 agent 自己的 `mcp_install` / `mcp_uninstall` 工具写 | `core/acowork-runtime/src/agent_config.rs:29` |
| "装没装"从不持久化 —— **本机 PATH 就是共享状态**（ADR-090 D6）；第一个启用者做一次 install，后来者 health_check 直接通过 | `core/acowork-runtime/src/mcp_ensure.rs:1` |
| `required_secrets` 的过滤（ADR-090 D4）在 Gateway 侧对 catalog 生效；local 条目不经这条链路 | `core/acowork-runtime/src/agent_config.rs`、`global_resources_builders.rs` |

四条理由：

1. **MCP 是机器资源**。同一个 `brave-search` 被 3 个 agent 推荐，落 catalog 只配置一次（含凭据）；落 local 会得到 3 份配置、3 条 `install` spec，各自跑一遍安装与探测。
2. **复用唯一安装路径**。预设安装管线（ADR-072）写的就是 catalog，`POST /api/mcp-catalog` 是既有端点；落 local 需要新造一条"从预设安装到实例"的写路径，与本方案"不新增写路径"的原则冲突。
3. **凭据语义**。catalog 是凭据的既有归属地（类比 Vault）；把企业 MCP 的 token 拆散到 N 个 agent 配置文件里，既不安全也不可维护。
4. **`local` 的语义要留住**：它属于**运行期自装** —— agent 通过 `mcp_install` 工具临时给自己装一个 server，没有可信预设可复用。包声明的推荐是**出厂意图**，不是运行时决定，两者不应混在一个列表里（合并时 local 还会 shadow 同名 catalog 条目，见 `AgentMcpConfig::merged()`）。

三个必须一起处理的后果：

1. **可见性外溢**（预期行为，需在 UI 说明）：从 agent A 的工具面板装 `brave-search`，它会出现在本机每个 agent 的 MCP 列表里（**未激活**）。catalog 推送不会自动激活任何条目 —— `save_agent_mcp_config_catalog` 只在 `active_names` 为 `None` 时默认激活系统注入项（当前仅 `pm`）。走查稿 §A 需要补一句提示："安装后本机其他 Agent 也能看到它，但只在当前 Agent 激活。"
2. **卸载 agent 不清理 catalog**：与用户手动添加的条目一致。可选增强（v1 不做，仅记录）：catalog 条目加 `origin`（`manual` | `preset:<id>` | `agent:<agent_id>`），用于 UI 显示来源与将来清理孤儿。
3. **工具白名单仍是 per-agent**：`recommended.tools` 落到 `agent_mcp_tools.json`（实例级），与 catalog 的机器级配置正交 —— 正是 ADR-069 的分层（服务器共享、工具开关按 agent 各自决定）。

---

## 5. 方案三：capability 与 skill 的关系

### 5.1 定义（写进 ADR 与 design 文档）

| 对象 | 回答的问题 | 存放位置 | 生命周期 | 消费方 |
|---|---|---|---|---|
| capability | 别的 agent 能否寻址我这个动作、契约是什么 | manifest `[capabilities.*]` | 包（安装期注册） | Gateway capability 注册表 / overview / intent 契约 |
| skill | 我内部按什么流程做事 | 包内 `skills/*/SKILL.md`，可运行期后装 | 实例（可增删） | Runtime 提示注入 + `load_skill` |
| tool | 我用什么原语执行 | `[[tools]]` + `agent_tools.json` | 实例（用户开关） | ToolRegistry |
| mcp server | 我的工具从哪来 | `[[mcp_servers]]` + `agent_mcp*.json` | 实例（用户开关） | MCP 管理器 |

### 5.2 关系规则

1. **skill 不在 manifest 声明**。磁盘是唯一来源，因为 skill 可运行期增删（`install_skill_package`），任何 manifest 副本必然漂移。`[skills]` 将来只承载策略（`progressive`），不承载清单。
2. **capability 可选链接 skill**：`[capabilities.task_decomposition] skills = ["task-decomposition", "sprint-planning"]`。语义是"这个对外动作由这些内部流程实现"，用于文档、展示与校验，**不引入运行期调度**（不会因为写在这里就自动加载 skill）。
3. **校验图**（纯函数 `acowork_core::manifest::validate_graph`，发布期 + 安装期同一入口）：
   - `capabilities.*.skills` ⊆ 包内 `skills/` 目录名 → 违反则 **error**（引用了不存在的流程）；
   - `skills/*/SKILL.md` 的 `tool_deps` ⊆ `manifest.tools[].name` ∪ `mcp_servers[].tools` → 违反则 **warn**（`tool_deps` 可能引用宿主注入的 `mcp_memory__*` / `mcp_pm__*`，判死不成立，只提示）；
   - `mcp_servers[].tools` 在安装期无法校验（server 未装），运行期 `tools/list` 之后校验成 gap —— 见 4.3。
4. **CreateWizard 修正**：删除 skill→capability 的 1:1 生成（`CreateWizard.tsx:627`）。skill 只进包；capability 改为用户显式填写的可选列表。
5. **迁移**：老包里"同名 capability + skill"不做任何自动转换，行为不变；文档明确标注"同名不再携带任何隐含语义"，前端不再据此推断。

---

## 6. 方案四：`labels`

```toml
labels = ["project-management", "agile", "requirements"]
```

- 语义：**检索/过滤维度**，不是能力声明、不是展示标题。
- 约束（`validate()` 内，发布期早失败）：小写 `[a-z0-9-]`，长度 2–32，数量 ≤ 12，去重；无该字段 = 空数组。
- 边界：`label` ≠ `role`（单一展示标题）≠ `capabilities`（可寻址动作）≠ skill 名。v1 不做命名空间前缀，避免过早抽象。
- 消费面：`AgentListResponse.labels`（[core/acowork-gateway/src/http/agents.rs:140](core/acowork-gateway/src/http/agents.rs#L140)）+ `GET /api/agents?label=` 过滤 + Desktop 卡片 chip + 后续市场检索。

---

## 7. 推荐 MCP 的前端交互（必须产生实际作用）

> 逐屏走查稿（布局、状态、文案、待拍板项）：[agent-manifest-ui-walkthrough.md](agent-manifest-ui-walkthrough.md)。

> 立项原则：一个只写进 manifest、用户永远看不到的字段等于没做。下面把"推荐"定义成 4 个可观测结果，每个都落到具体控件与既有端点上。

### 7.1 四个实际作用

| # | 作用 | 用户看到什么 | 行为变化 |
|---|---|---|---|
| 1 | 可见 | Agent 工具面板顶部出现「来自 Agent 包的建议」分组，每行带作者写的 `why` | 用户不再需要自己去猜这个 Agent 缺什么 |
| 2 | 一键落地 | 「安装并启用」按钮 | 复用现有 preset 安装管线 + 激活接口，一次点击完成今天要跳 3 个界面的操作 |
| 3 | 真实改变工具集合 | 启用后该 server 的 `tools` 白名单成为默认 enabled 子集 | 用户不必在 30 个工具里手挑 4 个；这就是 `PM_DEFAULT_ENABLED_TOOLS` 的替代 |
| 4 | 缺口可见 | 必需但未满足 → Agent 卡片角标 + 详情"能力受限"；工具改名 → 行内"该 MCP 未提供 `<tool>`" | 作者声明的意图与实际环境不一致时能被发现，而不是静默降级 |

### 7.2 状态机（Gateway 合成，前端只渲染）

| state | 判定 | 行内文案 | trailing 动作 | 复用的既有端点 |
|---|---|---|---|---|
| `unavailable` | `recommended.name` 不在该 agent 的 catalog ∪ local 里 | 「未安装」+ why | 安装并启用 | `POST /api/mcp-catalog`（preset 安装管线）→ `PUT /api/agents/{id}/mcp-servers` 追加 name |
| `secrets_missing` | 在 catalog 里但 `required_secrets` 未填（Gateway 已按 ADR-090 D4 过滤掉） | 「缺少密钥：BRAVE_API_KEY」 | 填写密钥 | 复用 add-server 对话框的 secret 表单 |
| `installed_inactive` | 在 catalog/local 里，但不在 `active_names` | 「已安装，未启用」 | 启用 | `PUT /api/agents/{id}/mcp-servers` |
| `active` | 在 `active_names` 里 | 「已启用」+ 工具数 | 无（可展开看工具） | — |
| `dismissed` | 用户点过「忽略」 | 折叠在「已忽略 (n)」里，可恢复 | 恢复 | `PUT /api/agents/{id}/mcp-servers`（带 `dismissed_recommendations`） |

`missing_tools` 不在后端算：前端已经同时持有 `recommended[].tools` 与 `GET /agents/{id}/mcp-tools` 的 `servers[name][]`，一个纯函数 `missingRecommendedTools(recommended, toolConfig)` 取差集即可，避免 Gateway 二次代理 Runtime。

`dismissed` 存放位置的结论：它是**用户状态**，不得进 manifest；存 Runtime 侧 `agent_mcp.json`（跟随实例、跨设备一致），随 `PUT /agents/{id}/mcp-servers` 的扩展字段写入 —— 不新增路由。

### 7.3 装配点（三处，均有明确契约）

1. **ToolsTab（右侧面板 → 工具）**：新分组「来自 Agent 包的建议」置于 MCP Servers 卡片**之上**（它需要用户先处理），仅当 `recommended.length > 0` 时渲染。每行复用 `ExpandableRow`，展开体显示 `why` + 若已激活则列出缺失工具。
2. **Agent 列表卡片 / 详情**：存在 `required = true` 且 state ∉ {active, dismissed} 的条目时，卡片出一个小角标（沿用 sidebar count badge 的视觉语法），详情页加一行「能力受限：缺少必需 MCP `brave-search`」。**不阻断启动与聊天**（"建议"不是"依赖"）。
3. **创建/安装成功后**：`onCreated` 回调里把右侧面板切到 `tools` 标签并展开该分组（一次性）。这样"推荐"在用户第一次见到这个 Agent 时就出现在眼前，而不是埋在标签页里。

### 7.4 数据契约

- Runtime：`agent_mcp.json` 增 `recommended: Vec<RecommendedMcpServer>` 与 `dismissed: Vec<String>`（都是 `serde(default)`）。
- Gateway：`GET /api/agents/{id}/mcp-servers` 响应扩为 `{ active_servers, servers, recommended: [{ name, preset, hosted, transport, url, tools, required, why, required_secrets, state }] }`。`state` 由 Gateway 合成（它持有 catalog 与 secrets 过滤信息）；`recommended` 本体由 Runtime 提供。
- 前端：`resolveRecommended(recommended, catalog, active, dismissed)` 纯函数 → 渲染模型；`missingRecommendedTools()` 纯函数 → 缺口。
- 新增写路径仅一处：`PUT /api/agents/{id}/mcp-servers` 的可选字段 `dismissed_recommendations`。

### 7.5 降级

- 客户端不认识 `preset`（旧版本 / 移动端未上移预设表）→ 该行显示「当前客户端不支持该预设，请升级」，不崩、不隐藏。
- catalog 里同名但不同来源 → 视为 `installed_inactive`（用 name 关联，`preset` 只用于安装引导）。

### 7.6 测试边界

`resolveRecommended` 的 state × catalog × active × dismissed 矩阵（纯函数，无 DOM）；`missingRecommendedTools` 的差集与空值；`dismissed` 写入幂等。前端不新增 DOM 选择器查询（沿用 AGENTS.md 的列表渲染约定）。

---

## 8. Create Agent dialog：覆盖本方案的全部数据

> 逐屏走查稿见 [agent-manifest-ui-walkthrough.md](agent-manifest-ui-walkthrough.md) §屏 D。

### 8.1 现状缺陷（三个，都能验出来）

1. 4 步（basic / tools / skills / preview），缺 `[config]`、缺 `capabilities` 的显式声明、缺 `[[mcp_servers]]`、缺 `labels`。
2. skill → capability 1:1：前端 `CreateWizard.tsx:627` 与后端 `commands/create.rs` 的 `capabilities.insert(skill.name, ...)` 是同一条错误假设的两个副本；i18n 文案还在传播它（`createWizard.skillsHint`："每个 SKILL.md 会成为 manifest 的一个 capability"）。
3. **预览会骗人**：`buildPreviewText()` 在前端手搓 TOML，第一段是 `[package]` —— 这个表在 `AgentManifest` 里根本不存在（serde 默认忽略未知字段，真按它构造会被静默丢弃）。同一份数据有"预览实现"和"提交实现"两份，必然漂移。

### 8.2 目标：5 步，覆盖全部字段

| 步骤 | 覆盖字段 | 必填 | 校验 | 写入 manifest |
|---|---|---|---|---|
| 1 基础 | `agent_id`、`name`、`display_name`、`role`、`version`、`author`、`description`、**`labels`** | agent_id / name | 反向域名；label 走 §6 规则（前端同规则即时提示） | 顶部标量 + `labels` |
| 2 模型与参数 | `temperature`、`context_window`、`max_output_tokens`、`max_iterations`、`compression_ratio_threshold` | 全可空 | 范围校验；留空 = **不写** `[config]` | `[config]`（仅非空项） |
| 3 工具与 MCP | 内置工具多选（保持现状） + `[[mcp_servers]]` 多选 | 无 | preset/hosted 互斥；hosted 仅列 `pm`/`doc`/`memory`；preset 列表来自 `/api/mcp-presets` | `[[tools]]` + `[[mcp_servers]]` |
| 4 技能与能力 | 技能 ZIP 上传（保持） + `[capabilities.<action>]` 声明（action、description、可选 `skills` 链到本次上传的技能） | 无 | capability.skills ⊆ 已上传技能（`validate_graph`）；每行显示 skill 的 `tool_deps` 并与第 3 步交叉提示 | `[capabilities.*]`（`skills` 字段） |
| 5 预览 | 真实 TOML + 校验结果 | — | `validate()` error 阻塞、warn 提示 | — |

每步 `canNext` 都是"必填满足 + 该步校验通过"。第 2 步每个输入右侧显示平台兜底值（"默认 200000"），避免"空"的含义不明。

关于 `[[mcp_servers]].tools` 白名单：创建时服务器未连接，拿不到 `tools/list`，所以这一步**不收集工具名**（留空 = 启用后由 §7.2 的建议行让用户勾）。声明工具子集的能力保留在 manifest 里，但 wizard 不做输入 —— 这是刻意的取舍，避免让用户凭空填工具名。

capability 的可选输入/输出 schema：wizard v1 只收 `description`（+ 可选 `skills`），schema 用「高级（JSON）」折叠区手填，保持 `[capabilities.*]` 的完整性而不把 wizard 做成 JSON 编辑器。

### 8.3 单一构造点：manifest 只在一个地方被拼出来

| 现状 | 目标 |
|---|---|
| 前端 `buildPreviewText()` 手拼 TOML（与 schema 不一致） | 删除。预览改为 `invoke("preview_manifest", { draft })`，后端 `to_toml()` 返回真实文本 |
| 后端 `create_agent(...)` 11 个位置参数 | `create_agent(draft: CreateAgentDraft)` |
| 两处各自拼 manifest | 一个 `build_manifest(&CreateAgentDraft) -> Result<AgentManifest, String>`，预览与创建共用；`build_manifest` 末尾调用 `validate()` |

```rust
// apps/acowork-desktop/src-tauri/src/commands/create.rs
#[derive(serde::Deserialize)]
pub struct CreateAgentDraft {
    pub agent_id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub version: Option<String>,
    pub author: Option<String>,
    pub description: Option<String>,
    pub labels: Vec<String>,
    pub config: Option<acowork_core::manifest::ManifestConfigDefaults>,
    pub tools: Vec<String>,
    pub skill_files: Vec<Vec<u8>>,
    pub mcp_servers: Vec<McpServerDeclInput>,
    pub capabilities: Vec<CapabilityInput>,
}
```

### 8.4 i18n 影响

5 个 locale（en / zh-CN / zh-TW / ja / ko）都要补：新增 2 个步骤标题 + 约 20 条控件文案；并**必须改掉** `createWizard.skillsHint` 的旧文案（它现在是错误设计的说明书）。这是实打实的工作量，写进分期。

### 8.5 测试边界

- Rust：`build_manifest` 的 draft → manifest 映射矩阵（含 labels 非法、capability 引用了不存在的技能 → error、preset/hosted 互斥）；`preview_manifest` 与 `create_agent` 产出同一份定义（同一函数，用一个测试钉住）。
- 前端：`canNext` 各步矩阵、label chip 校验、capability.skills 多选来源正确；不新增 DOM 选择器。

---

## 9. 目标形态（示例 diff）

```toml
 agent_id = "com.acowork.project-manager"
+# 检索标签
+labels = ["project-management", "agile", "requirements"]
 
+# 作者默认值 → 首次启动 seed 进 agent_config.json（用户可改、可清空回退）
+[config]
+temperature = 0.5
+max_output_tokens = 32768
+max_iterations = 300
+context_window = 200000
+compression_ratio_threshold = 0.9
+
 [[tools]]
 name = "file_edit"
 
+# 推荐 MCP（用户确认后才安装/启用）
+[[mcp_servers]]
+hosted = "pm"
+why = "任务与项目管理数据"
+tools = ["pm_list_my_tasks", "pm_claim_task", "pm_submit_task", "pm_check_task"]
+required = true
+
+[[mcp_servers]]
+preset = "brave-search"
+why = "联网检索最新资料"
+tools = ["brave_web_search"]
+
 [capabilities.task_decomposition]
 description = "Break down project goals into actionable tasks"
+skills = ["task-decomposition", "sprint-planning"]
 
-[llm]
-temperature = 0.5          # deprecated → [config].temperature
-max_tokens = 32768        # deprecated → [config].max_output_tokens
-context_window = 200000   # deprecated → [config].context_window
```

---

## 10. 实施分期

| Phase | 交付物 | 触点 | 测试边界 |
|---|---|---|---|
| 0 | 本方案 | `docs/plan/zh/` | — |
| 1 | manifest 类型：`[config]`、`labels`、`[[mcp_servers]]`、`capabilities.*.skills`；`validate()` 新规则；`ManifestConfigDefaults`；`validate_graph` 纯函数 | `core/acowork-core/src/manifest.rs`（或拆 `manifest/`） | TOML 解析（含老包回归）+ 每一条校验规则的 red→green + 投影函数 |
| 2 | `seed_agent_config_from_manifest` 替换 session_init 的 if 序列；`agent_mcp.json` 新增 `recommended` / `dismissed` 投影；`default_enabled_tools_for` 删除、改读 manifest | `acowork-runtime/src/agent_config.rs`、`startup/session_init.rs`、`agent_core.rs` | seed 幂等性 + 不覆盖非 None + 老 `agent_mcp.json` 解析兼容 |
| 3 | 预设表上移 Gateway + `GET /api/mcp-presets`；`GET /agents/{id}/mcp-servers` 增 `recommended[].state`；`PUT` 增 `dismissed_recommendations`；agents list/detail 增 `labels` | `acowork-gateway/src/http/` | 预设 JSON 契约 + `state` 合成矩阵 + list 过滤 |
| 4a | **推荐 MCP 的真实交互**（§7）：ToolsTab 建议分组 + 4 个动作 + 忽略/恢复；Agent 卡片要求角标与"能力受限"行；创建成功后自动切 Tools 标签 | `right-panel/ToolsTab.tsx`、`stores/mcpStore.ts`、`agent-list/*`、`lib/types.ts` | `resolveRecommended` / `missingRecommendedTools` 纯函数矩阵；不新增 DOM 选择器 |
| 4b | **Create Agent dialog 5 步全字段**（§8）：labels / config / mcp / capability 四类输入；删掉 skill→capability；预览改走 `preview_manifest`；`create_agent(CreateAgentDraft)` + 单一 `build_manifest`；5 个 locale 文案（含修掉 `createWizard.skillsHint`） | `agent-list/CreateWizard.tsx`、`src-tauri/src/commands/create.rs`、`i18n/locales/*` | `build_manifest` 映射矩阵 + preview 与 create 同源 + 各步 `canNext` |
| 5 | ADR-104（zh + en 同提交）、`docs/design/{zh,en}/02-agent-package.md`、examples 更新、`dev/` 打包期 graph 校验接线 | docs / dev / examples | 打包脚本对非法包必须失败 |

Phase 1 与 Phase 2 各自独立可交付、可回滚：Phase 1 只加字段与纯函数（无行为变化）；Phase 2 换 seed 入口（行为等价 + 新增两处来源）。Phase 3 → 4a 之间有硬依赖（没有 `state` 合成就没有可渲染的建议行）；4b 只依赖 Phase 1（`build_manifest` 需要新类型），可与 2/3/4a 并行。

---

## 11. 风险

- **R1 `[config]` 双写**：若保留 `[llm].temperature` 且不设优先级与告警，就会出现"两个地方写一个值"。缓解：优先级固定 + 发布告警 + 一个版本周期后删兼容读。
- **R2 preset 引用跨端失效**：见 4.4，必须先上移预设表，否则 Phase 4 的 UI 只是本地能跑。
- **R3 `recommended` 被误当 `local`**：任何"安装期顺手写 `local`"的实现都破坏 ADR-090 的用户确认语义。缓解：类型上分开（`Vec<RecommendedMcpServer>` ≠ `Vec<McpServerConfigDef>`），编译期就不能混用。
- **R4 老包 capability 语义漂移**：加了 `skills` 链接后，前端若沿用"capability = skill"的推断会显示错。缓解：Phase 4b 同步改 CreateWizard 与展示，并在 design 文档中标注。
- **R5 推荐变成骚扰**：每次打开面板都提醒同一个不想装的服务器。缓解：`dismissed` + 「已忽略 (n)」折叠组，默认不再提示。
- **R6 wizard 扩容变成表单地狱**：5 步、每步都塞满可选字段会让创建变慢。缓解：`[config]` 全部可空且显示平台兜底值；MCP 步骤只选服务器不填工具名；`[capabilities]` 的 schema 收在"高级"折叠区。

---

## 12. 待确认决策

1. `[config]` 是否接管 `[llm].temperature` / `[llm].context_window` / `[llm].max_tokens`（推荐：接管，兼容读 + 发布告警 + 下个版本删除）。
2. `[[mcp_servers]]` v1 是否禁止内联 stdio（推荐：禁止，只允许 preset / hosted 引用与内联 http；stdio 若要做，需先新增 `McpInstall` 权限）。
3. 预设注册表是否上移为 Gateway 平台数据（推荐：是，作为 Phase 3 前置；不上移则 §7 的「一键安装并启用」只能在 Desktop 里工作）。
4. `capabilities.*.skills` 是否必填（推荐：选填，缺失只在发布期提示，不拦）。
5. 创建向导的步数取舍：5 步（基础 / 模型与参数 / 工具与 MCP / 技能与能力 / 预览，见 §8.2），还是把「工具与 MCP」「技能与能力」各自拆开变 7 步（推荐：5 步，保持每步一个心智单元）。

确认后按 Phase 1 起步；Phase 4a / 4b（前端与创建向导）已并入本次实施范围。