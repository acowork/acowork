# ADR-104：manifest 能力声明面重构 —— `[config]` 默认值、`[[mcp_servers]]` 推荐声明、capability↔skill 关系、`labels`

- **编号**：ADR-104
- **状态**：已实施（P1–P5 全量落地）
- **日期**：2026-10-30
- **决策者**：ACowork 架构组
- **关联**：ADR-009（Gateway 边界红线）、ADR-017（manifest 与包格式）、ADR-025（能力寻址）、ADR-026（权限模型）、ADR-029（工具声明）、ADR-055（Node 拓扑）、ADR-061（上下文压缩比例阈值）、ADR-069（MCP 工具级 opt-in）、ADR-072（MCP 安装包规范）、ADR-074（上下文窗口语义）、ADR-090（MCP 安装归属 Node）、ADR-094（渐进式技能）、ADR-101（记忆服务化 —— 本 ADR 把"平台托管 MCP"作为一等声明形态固化）
- **配套方案**：[agent-manifest-declaration-plan.md](../../plan/zh/agent-manifest-declaration-plan.md)（含代码取证与粒度评估）、[agent-manifest-ui-walkthrough.md](../../plan/zh/agent-manifest-ui-walkthrough.md)（逐屏走查）
- **英文 counterpart**：[docs/adr/en/ADR-104-agent-manifest-declaration-surface.md](../en/ADR-104-agent-manifest-declaration-surface.md)（同 commit 维护）

---

## 摘要

**一句话**：把 `manifest.toml` 从"元数据 + 内置工具清单"补成**完整的能力声明面** —— 新增 `[config]`（`agent_config.json` 的行为默认值）、`[[mcp_servers]]`（推荐 MCP，用户确认后安装）、`[capabilities.*].skills`（能力与技能的真实关系）、`labels`（属性标签）；并把这四类声明**全部接入前端**：推荐 MCP 在工具面板可一键安装/启用/忽略，创建向导从 4 步扩到 5 步覆盖全部字段。

**一句话（为什么）**：一个只能声明"我有哪些内置工具"的 manifest，无法表达"我需要什么 MCP""我推荐什么参数""我的哪个能力由哪些技能实现"—— 于是这些知识只能以硬编码（`PM_DEFAULT_ENABLED_TOOLS`）或口头约定存在。声明面缺一块，平台就少一块可校验、可展示、可回滚的事实。

**四条不变量**（塑造全篇，否则会被读偏）：

1. **推荐 ≠ 安装。** `[[mcp_servers]]` 只是"作者建议"，落地成 `agent_mcp.json.recommended`，永不直接写 `catalog` / `local` / `active_names`。安装与启用是**用户动作**（ADR-090）。
2. **默认值 ≠ 覆盖。** `[config]` 只在 `agent_config.json` 的字段为 `None` 时回填，用户改过的值永不被包覆盖（幂等）。
3. **能力 ≠ 技能。** 技能是**内部流程**（不对外暴露），能力是**对外契约**（其他 Agent 通过 intent 寻址）。二者是多对多：一个能力可由多个技能实现，一个技能可服务多个能力。
4. **声明必须有可见后果。** 推荐的 MCP 若在 UI 上无处可见、无事可做，就不该加这个字段 —— 本 ADR 的推荐 MCP 有四种可观测后果（见 §5.1）。

---

## 1. 现状事实（代码取证）

### 1.1 manifest 之前声明了什么，缺了什么

`AgentManifest`（[core/acowork-core/src/manifest.rs](../../../core/acowork-core/src/manifest.rs)）此前包含：身份字段、`permissions`、`triggers`、`[llm]`、`[memory]`、`identity_deps`、`[[tools]]`、`[capabilities.*]`、`[resources]`、`[sandbox]`、`[skills].progressive`。

**缺失**：MCP 声明、`agent_config.json` 的默认值声明、标签、能力的实现关系。

### 1.2 `agent_config.json` 的落地路径并不统一

- `temperature` / `context_window`：`[llm].temperature` / `[llm].context_window` 是唯一 manifest 来源，首启由 `session_init` 的 "Resolve & persist defaults" 块逐字段回填。
- `max_output_tokens`：`agent_config.json` **没有** manifest 来源；而同名的 `[llm].max_tokens` 走的是**另一条通道** —— `agent/context.rs` 把它当请求级 `max_tokens` **硬覆盖**。同名字段、两套语义、两条链路。
- `max_iterations` / `compression_ratio_threshold`：来源是 `RuntimeConfig` 或代码常量，作者无法表达。
- `session_language` / `search` / `chat_router` / `system_prompt_override`：属用户或平台域，**不应**由包声明。

### 1.3 MCP 的三份状态，包无法参与

`AgentMcpConfig { catalog, local, active_names }`：`catalog` 由 Gateway 推送，`local` 由 Agent 自己的 `mcp_install` 装，`active_names = None` 表示"一个都不激活"。

预设注册表此前在**前端**（`apps/acowork-desktop/src/lib/mcp-presets.ts`），Gateway 与 Runtime 都不认识这些 id。因此"按包定制 MCP 工具子集"的唯一实现，是 Rust 硬编码：

```
PM_DEFAULT_ENABLED_TOOLS          agent_config.rs
default_enabled_tools_for("pm")   agent_config.rs
```

### 1.4 capability 的作用已经很薄

`[capabilities.*]` 已参与 intent 寻址（ADR-025），但**没人声明它由什么实现**。前端还传播了一个错误假设：`CreateWizard.tsx` 与 `commands/create.rs` 各自把"每个 SKILL.md → 一个 capability"写成 1:1，i18n 文案亦如此（"每个 SKILL.md 会成为 manifest 的一个 capability"）。

### 1.5 skill 的唯一来源是磁盘

`skills/<name>/SKILL.md` 是技能的唯一事实来源，manifest 侧没有它的位置 —— 所以"某个能力由哪些技能实现""某个技能需要哪些工具"这两条边，在声明层面**不可校验**。

### 1.6 根因

声明面按"当初够用"实现，四类知识（参数默认值、MCP 需求、实现关系、属性标签）落在代码、前端常量、口头约定里。**修法不是补字段，而是补字段 + 落地路径 + 可见后果 + 校验点**，四者缺一即为装饰。

---

## 2. 决策

| # | 决策 | 结论 |
|---|---|---|
| 1 | `[config]` 字段白名单 | 只收 **行为旋钮** 5 项：`temperature`、`context_window`、`max_output_tokens`、`max_iterations`、`compression_ratio_threshold`。**排除**安全边界（`shell_approval_threshold`、`approval_timeout_secs`）、宿主资源配额（`max_sessions`）、身份偏好（`session_language`）、用户自有覆盖（`system_prompt_override`）—— 包不得降低审批阈值或替用户声明偏好 |
| 2 | `[config]` 与 `[llm]` 的关系 | `[config]` 优先；缺省回退读 `[llm].temperature` / `[llm].context_window`（兼容既有包）。投影集中在 `AgentManifest::config_defaults()`，**唯一入口** |
| 3 | 默认值写入语义 | 只回填 `None` 字段，**永不覆盖**用户已设值；幂等（无变化不写盘）。`seed_agent_config_from_manifest()` 与 `AgentConfig::seed_from_manifest()` 共用 `fill_if_empty()` |
| 4 | `max_output_tokens` 语义 | 新增独立的**上限**语义写入 `agent_config.json`；`[llm].max_tokens` 的请求级覆盖语义**保持不动**，但 `context.rs` 改为读 `max_output_tokens_limit` 参数，不再直接读 manifest —— 同名字段的两套语义各自收敛到各自通道 |
| 5 | `[[mcp_servers]]` 粒度 | **服务器 + 工具名白名单**。不复制工具 input schema —— MCP 自己的 `tools/list` 是唯一真源 |
| 6 | `[[mcp_servers]]` 三形态互斥 | `preset`（引用平台预设）/ `hosted`（引用 Gateway 托管服务 `pm`/`doc`/`memory`）/ `transport`+`url`（内联远程）。**禁止内联 stdio** —— 否则包可以指定在用户机器上执行的命令，却只顶着"推荐"的名义 |
| 7 | 推荐落地位置 | 写入 `agent_mcp.json.recommended`（**不写** `catalog`/`local`/`active_names`）；已忽略项写 `dismissed`；包升级重新投影推荐，且把包已不再声明的 key 从 `dismissed` 中清出（否则旧 key 会永久压制未来的同号推荐） |
| 8 | 推荐安装去向 | **本机共享 MCP 目录（catalog）**，并带 `origin` 字段：`preset:<id>` / `agent-package:<agent_id>#<key>` / `user`。理由见 §4.3 |
| 9 | 预设注册表位置 | 上移至 **Gateway**（`acowork-gateway/src/http/mcp_presets.rs`），暴露 `GET /api/mcp-presets`、`GET /api/mcp-presets/{id}`。preset id 成为**包可见标识符**，其含义不能只存在于一个客户端 |
| 10 | capability ↔ skill | 多对多，`[capabilities.<action>].skills = ["<skill-name>"]`。引用不存在的技能是 **error**（打包与安装都拒绝）；技能的 `tool_deps` 无法从声明或平台托管 MCP 到达是 **warning**（不阻断） |
| 11 | `labels` | 小写 kebab，2–32 字符，≤12 个，去重。用途：Agent 列表筛选、市场属性、归类。**不**参与权限、寻址或生命周期 |
| 12 | 单一构造点 | `ManifestDraft::build(skills)` 是 manifest 的**唯一**拼装处；`create_agent(draft)` 与 `preview_manifest(draft)` 共用它。前端不再手拼 TOML |
| 13 | 安装/打包期校验 | `acowork-node` 安装、`acowork-sign` 打包都调用 `validate_graph()`：errors 拒绝，warnings 只记日志 |
| 14 | 硬编码退场 | 删除 `PM_DEFAULT_ENABLED_TOOLS` / `default_enabled_tools_for("pm")`，改由 manifest 的 `[[mcp_servers]].tools` 白名单驱动 |
| 15 | 生命周期状态由谁算 | **Runtime** 合成 4 态（`not_installed` / `installed_inactive` / `active` / `dismissed`），前端只渲染。两个客户端各自重算必然早晚不一致 |

---

## 3. 数据结构

### 3.1 `[config]`

```toml
[config]
temperature = 0.5             # 0.0–2.0
context_window = 128000       # > 0（0 不是"无限制"哨兵，ADR-074 §6）
max_output_tokens = 8192      # > 0，单次调用输出上限
max_iterations = 40           # > 0
compression_ratio_threshold = 0.9   # 0.05–0.95（ADR-061 的"节省占比"）
```

Rust：`ManifestConfigDefaults`，全字段 `Option` + `skip_serializing_if`，`is_empty()` 为真时**整段不写**（"没有意见"与"显式默认"不是一回事）。`validate()` 逐字段范围校验，报错点名具体字段。

### 3.2 `[[mcp_servers]]`

```toml
# 形态 A：引用平台预设（包内无可执行内容）
[[mcp_servers]]
preset = "brave-search"
why = "联网检索最新资料"
required_secrets = ["BRAVE_API_KEY"]
tools = ["brave_web_search", "brave_news_search"]
required = false

# 形态 B：引用 Gateway 托管服务
[[mcp_servers]]
hosted = "pm"
why = "任务与项目管理"
required = true

# 形态 C：内联远程（企业自建 MCP，必须被 Network 权限覆盖）
[[mcp_servers]]
name = "corp-kb"
transport = "http"
url = "https://mcp.corp.example/kb"
why = "内部知识库"
```

Rust：`McpServerDecl`，`key()` 解析身份键（`name` ?? `preset` ?? `hosted`），`source_kind()` 判定三形态，`validate()` 强制**互斥**、内联形态只允许 `http`/`sse` 且必须被 `Network` 权限覆盖、同一 manifest 身份键唯一。

### 3.3 `[capabilities.<action>].skills`

```toml
[capabilities.requirements_analysis]
description = "Analyze and document project requirements"
skills = ["requirements-analysis", "risk-assessment"]
```

Rust：`CapabilityDef.skills: Vec<String>`；`DraftCapability` 为作者态（多一个 `action` 键，便于向导就地改名）。

### 3.4 `labels`

```toml
labels = ["project-management", "agile", "team"]
```

Rust：`AgentManifest.labels: Vec<String>`，`is_valid_label()` 与前端 `manifestRules.isValidLabel()` 逐字对齐；`MAX_MANIFEST_LABELS = 12`。

### 3.5 声明图校验

`AgentManifest::validate_graph(&[SkillDecl]) -> ManifestGraphReport`：

1. `capabilities.*.skills` 必须能在包内解析 → **error**。
2. 每个 skill 的 `tool_deps` 必须可从 `[[tools]]` ∪ `[[mcp_servers]].tools` 到达，或属于平台托管 MCP（`mcp_<server>__<tool>` 且 `server ∈ {memory, pm, doc}`）、或属于本包已声明的服务器 → 否则 **warning**。

拆成两半的原因是**能力**：打包器（`acowork-sign`）能列出 `skills/<dir>/` 但无法解析 `SKILL.md` frontmatter（那个解析器在 Runtime，core 不能依赖它），所以"能力→技能"这半边是每个层都能查的，故定为 error；"工具可达性"这半边需要 frontmatter，故为 warning。

---

## 4. 落地路径

### 4.1 `[config]` → `agent_config.json`

```
manifest.toml [config]
  └─ AgentManifest::config_defaults()        （唯一投影点，含 [llm] 回退）
      └─ AgentConfig::seed_from_manifest()   （fill_if_empty：只填 None）
          └─ agent_config.json
```

调用点两处：启动（`session_init`）、安装/升级（`seed_agent_config_from_manifest`）。两者都经 `fill_if_empty()`，因此"manifest 永不覆盖用户值"是**一个函数**的性质，而不是散落的 if。

### 4.2 `[[mcp_servers]]` → `agent_mcp.json.recommended`

```
manifest.toml [[mcp_servers]]
  └─ project_recommended_mcp_servers()   （每次 Agent 启动重新投影）
      └─ agent_mcp.json { recommended: [...], dismissed: [...] }
          └─ build_recommendation_views(cfg, installed, active)
              └─ GET /agents/{id}/tools 的 recommended 字段
                  └─ 工具面板「Agent 包建议的 MCP」分组
```

`recommended` 是**派生状态**：每次启动重写，所以包升级能刷新自己的推荐；`dismissed` 也按包当前声明裁剪。`catalog` / `local` / `active_names` 由各自的 owner 维护，本流程**不碰**。

### 4.3 推荐安装去向：catalog，不是 local（决策 8 的展开）

三个候选各有代价：

| 去向 | 后果 | 判定 |
|---|---|---|
| `agent_mcp.json.local` | 随 Agent 私有，包升级/重装即丢；同一台机器 N 个 Agent 装同一个 MCP → N 份 spawn 配置、N 份密钥 | ✗ |
| 用户级 `mcp-catalog.json` 且**不加来源** | 别处凭空多出服务器，无法回答"这是谁装的" | ✗ |
| 用户级 `mcp-catalog.json` + `origin` | 复用 ADR-090 的既有安装管线（探测/安装/密钥一处）；可回答来源；其他 Agent 看得到但**不会自动激活** | ✓ |

第三条即本 ADR 的决策。`origin` 由 **Gateway 写入**（客户端不格式化它），三种取值：`preset:<id>`、`agent-package:<agent_id>#<key>`、`user`。

**幂等**：同名条目已存在时返回 `200` 且**不覆盖**任何字段 —— 尤其是 `env`。用户的 API key 是包升级唯一不能重置的字段，而推荐每次启动都会重跑。

### 4.4 生命周期状态（Runtime 合成，前端只渲染）

优先级：**dismissed > active > installed > not_installed**。

dismissed 优先于一切，因为它是列表里唯一的**用户意图**：早先装过/启用过的服务器不得让"我明确忽略过"的建议悄悄复活。

| state | 唯一主操作 |
|---|---|
| `not_installed` | 安装（落地进 catalog） |
| `installed_inactive` | 为本 Agent 启用 |
| `active` | 无（工具已可用） |
| `dismissed` | 恢复（收在"已忽略 N"折叠区里） |

### 4.5 忽略 / 恢复

`PUT /api/agents/{id}/mcp-recommendations/{key}`（Gateway 反代到 Runtime）。忽略是一次**持久写入**，不是本地隐藏 —— 换机器、换客户端都一致。

---

## 5. 用户可见行为

### 5.1 推荐 MCP 的四个实际作用（本 ADR 的准入条件）

1. **可见**：工具面板置顶「Agent 包建议的 MCP」分组，渲染作者的 `why`、所需密钥、将获得的工具名。
2. **一键可做**：`not_installed` 一次点击走完 `POST /api/mcp-catalog/from-declaration`（Gateway 拥有配方与 `origin`，客户端不拼）。
3. **真的改变工具集**：`installed_inactive` → 启用走与普通开关**同一条** `PUT /mcp-servers` 路径，App 内只有一种激活机制。
4. **缺口可见**：缺密钥时卡片点名 `BRAVE_API_KEY`，并指向唯一能补它的地方。

`recommended` 为空 → 整组不渲染（不写"暂无建议"的空文案）。

### 5.2 安装去向提示（必须显示）

从该分组安装的服务器进入**本机共享 MCP 目录**，所以卡片旁必须写："已加入本机 MCP 目录；其他 Agent 也能看到，但只在本 Agent 激活。" 不写这条，用户会在别的 Agent 工具面板里发现"凭空多出来的服务器"。

### 5.3 Agent 列表角标

仅当存在 `required = true` 且 state ∉ {`active`, `dismissed`} 时显示角标。dismissed 不算未满足（用户已明确拒绝，不再提醒）；全部满足时**不显示**任何"健康"角标。

### 5.4 Create Agent 向导：4 步 → 5 步

| 步骤 | 覆盖字段 | 可进入下一步的条件 |
|---|---|---|
| 1 基础 | `agent_id`、`name`、`display_name`、`role`、`version`、`author`、`description`、**`labels`** | 反向域名 + 名称非空 + 标签 ≤12 |
| 2 模型与参数 | `[config]` 五项 | 已填字段全部在范围内（全空合法） |
| 3 工具与 MCP | `[[tools]]` + `[[mcp_servers]]` | 总是可以（全部可选） |
| 4 技能与能力 | 技能 ZIP + `[capabilities.*]`（action / description / skills / 可选 JSON schema） | ZIP 全部解析成功、动作名合法且唯一、JSON 合法 |
| 5 预览 | — | 后端无 error |

两处结构性约束：

- **预览不再手拼。** 旧 `buildPreviewText()` 在前端拼 TOML，第一段是 `[package]` —— 该表在 `AgentManifest` 里根本不存在（serde 静默忽略未知字段，真按它构造会丢掉预览承诺的一切）。现在预览调 `preview_manifest(draft)`，与 `create_agent(draft)` 共用 `ManifestDraft::build`，**按构造不可能漂移**。
- **`[[mcp_servers]].tools` 在向导里不收。** 服务器未连接，拿不到 `tools/list`，让用户凭空填工具名只会产生假声明；工具子集在安装后于工具面板勾选。

### 5.5 区分：技能 vs 能力（写进 UI 文案）

向导第 4 步必须明说："技能是内部流程，不对外暴露；需要让其他 Agent 调用的动作请在下面的「对外能力」里声明。" 旧文案（"每个 SKILL.md 会成为 manifest 的一个 capability"）是错误知识的载体，已改写。

---

## 6. 拒绝的备选方案

| 备选 | 为什么拒绝 |
|---|---|
| 保留前端预设表，只加包内 `preset` 字符串 | preset id 成为包可见标识符后，其含义不能只存在于一个客户端；第二个客户端、Gateway 校验、安装管线都需要解析它 |
| 内联 stdio 形态 | 等于允许包在用户机器上指定可执行命令，却挂着"推荐"的名义 —— 与"包不含可执行内容"的包格式前提冲突 |
| 推荐直接写 `local` | 见 §4.3：私有、易丢、重复 spawn 配置与密钥 |
| 推荐写成 `active_names` | 会让"作者建议"变成"静默启用"，越过 ADR-090 的用户确认 |
| capability ↔ skill 保持 1:1 | 与事实不符（一个能力常由多技能实现），且把"内部流程"误当"对外契约" |
| `[config]` 允许声明安全阈值 | 包会获得降低审批门槛的能力，属于提权 |
| 前端自己算 MCP 推荐状态 | 两个客户端各自 join，必然早晚不一致；用户会看到"已安装"的服务器在另一个客户端显示"安装" |
| `max_output_tokens` 复用 `[llm].max_tokens` | 两者语义不同（请求级覆盖 vs 配置级上限），复用会把两条链路重新混在一起 |

---

## 7. 测试边界

| 层 | 断言什么 |
|---|---|
| `acowork-core` | `ManifestConfigDefaults` 范围矩阵；三形态互斥与内联 stdio 拒绝；`labels` 边界（2/32/12/去重/大小写）；`validate_graph` 的 error/warning 分界（含平台托管 MCP 可达性）；`config_defaults()` 的 `[config]` 优先 + `[llm]` 回退；`ManifestDraft::build` 的映射矩阵与"预览=创建" |
| `acowork-runtime` | seed 幂等 + 不覆盖用户值；`recommended` 投影不碰 `catalog`/`local`/`active_names`；4 态推导的优先级；忽略/恢复的持久化 |
| `acowork-gateway` | 预设注册表的 HTTP 投影；`from-declaration` 的 `origin` 三取值、幂等、`env` 不被重置；`labels` 透出到 Agent 列表/详情 |
| `acowork-node` / `acowork-sign` | 安装/打包对 dangling capability→skill 的拒绝；warning 不阻断 |
| Desktop | 推荐分组四态各一条 + 两个失败路径；向导每步门槛；**预览与创建发出的 draft 逐字段相同**；标签输入的提交/拒绝边界 |

前端侧另设"镜像即子集"原则：客户端校验只允许比 Rust **更严**（多拒不少拒）—— 多拒只是烦人，少拒会让对话框说"没问题"然后在创建时失败。

---

## 8. 风险与回滚

| 风险 | 处置 |
|---|---|
| `[config]` 与 `[llm]` 同时存在导致行为不明 | `config_defaults()` 是唯一投影点，`[config]` 优先已固化在单测里 |
| 预设 id 变更破坏已发布包 | `id` 是 API 契约，只能加别名不能改名（注册表注释已写明） |
| 推荐每次启动重投影带来写盘 | `project_recommended_mcp_servers` 内容无变化时返回 `false` 且不写盘 |
| 用户忽略的建议被包升级"复活" | `dismissed` 按包当前声明裁剪，但已忽略的 key 只要仍被声明就保留 |
| 推荐分组变成噪音 | 空列表不渲染；全部处理完默认折叠（标题显示计数） |

回滚：`[config]` / `labels` / `skills` 均为**可选字段**，旧客户端读取时被 serde 忽略；`[[mcp_servers]]` 清空即回到"没有推荐"。四类声明彼此独立，可分批回退。

---

## 9. 未决 / 延后

1. `[[mcp_servers]].tools` 的**向导内收集**（需先连服务器，属于"先安装后声明"的鸡生蛋问题）。
2. 推荐的**跨客户端一致呈现**（mobile 的推荐分组形态未与本 ADR 一同设计）。
3. `[config]` 是否应扩展到 `search` / `chat_router` 等**平台策略**字段 —— 目前判定为"用户/平台域"，若出现明确诉求需重新评估边界。

---

## 10. 实施落点（文件清单）

| 层 | 文件 |
|---|---|
| core | `core/acowork-core/src/manifest.rs`（四个新声明 + `validate` + `validate_graph` + `config_defaults` + `ManifestDraft`）、`core/acowork-core/tests/example_manifests.rs` |
| runtime | `agent_config.rs`（seed / `recommended` 投影 / 删除 `PM_DEFAULT_ENABLED_TOOLS`）、`usecases/mcp_recommendation.rs`（状态推导）、`agent/agent_core.rs`、`agent/context.rs` |
| gateway | `http/mcp_presets.rs`（注册表上移）、`http/mcp_catalog_api.rs`（`from-declaration`）、`http/agents.rs`（`labels`） |
| node / sign | `package/install.rs`、`packager.rs`（打包/安装期 graph 校验） |
| desktop | `lib/mcpPresets.ts`、`components/right-panel/McpRecommendationGroup.tsx`、`components/agent-list/CreateWizard.tsx`、`components/common/TagInput.tsx`、`lib/createWizardDraft.ts`、`lib/manifestRules.ts`、`src-tauri/src/commands/create.rs` |
| 示例 | `examples/*/manifest.toml` 七份全部补上 `labels` / `[config]` / `[[mcp_servers]]` / `capabilities.*.skills`，并以 `example_manifests.rs` 锁定为回归语料 |

`examples/agent-packages/*.agent` 是打包产物，未随本次改动重打（下次 `dev/common/build-agent` 时自然带上新段落）。

---

