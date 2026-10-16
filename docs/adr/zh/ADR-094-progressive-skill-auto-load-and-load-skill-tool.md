# ADR-094：Per-Agent Skills 自动加载（Progressive 默认） + load_skill 内置工具

- 状态：已接受（2026-10 修订：去耦化重构 + 摘要/当轮 skill 槽位分离，见 §7.1）
- 日期：2026-（原始）
- 决策者：ACowork 架构组

## 背景与目标

ACowork 的 agent 包通过 `skills/<name>/SKILL.md` 携带可复用的工作流
定义（frontmatter `name` + `description` + Markdown 正文）。当前实现里，
**skills 必须由用户手动粘贴到聊天中**才会被 LLM 看到 —— `SkillMode::Manual`
（默认） 是出厂行为。

手动粘贴有两个明显问题：

1. **LLM 不知道 agent 拥有哪些 skills**，也就不会主动调用它们。
2. **每次新会话都要重做一遍**，违反"会话上下文应该聚焦于用户问题，而不是
   skills 元数据"的原则。

业界惯例（Claude Code、MCP、Cursor、Windsurf）都将工具元信息（name +
description）注入 system prompt，再通过专用工具让 LLM 按需拉取完整说明。
本 ADR 把这一惯例落到 ACowork 的 skill 系统里。

## 决策

### 1. 存储与覆盖优先级

Skill 模式的解析优先级：

| 优先级 | 来源 | 字段 |
|---|---|---|
| 1（最高） | `agent_tools.json::load_skill.enabled` | `true` → Progressive；`false` → Manual |
| 2 | `manifest.skills.progressive` | 作者默认（默认 `false`） |
| 3 | 兜底 | Manual |

实现位于 [core/acowork-runtime/src/cli.rs](../../core/acowork-runtime/src/cli.rs)
的 `resolve_skill_mode()`（被 `agent_init` / `http/prompts` 双方调用），
替换之前的 `AgentSkillsOverride + .agent_skills.json` 死代码路径。

**单一权威源是 `load_skill.enabled` —— 没有任何第二个字段。**

### 2. UI 入口

`load_skill` 工具的启用开关位于 Tools 面板的 **Builtin Tools 列表**
（与 `memory_recall` / `file_read` / `http_request` 等其他内置工具同列），
走标准 `PUT /agents/{id}/builtin-tools` endpoint。

不再有"Agent Setup → Session Parameters 卡片"上的独立 "Auto-load skills"
Switch —— 2026-10 修订版已删除：

- 删除 `WiredField = "skillsAutoLoad"` 联合变体
- 删除 `WIRE_FIELD["skillsAutoLoad"]` 映射
- 删除 Session Parameters 卡片末尾的 `<Switch id="skills-auto-load-switch" />`
- 删除 `AgentProfileSettings.skillsAutoLoad` 字段
- 删除 i18n `agentSetup.skillsAutoLoad*` 三条字符串

### 3. 新内置工具：`load_skill`

[core/acowork-runtime/src/tools/builtin/load_skill.rs](../../core/acowork-runtime/src/tools/builtin/load_skill.rs)
（新增）。LLM 看到 system prompt 里的 skills 摘要后，按需调用此工具拉取
某个 skill 的完整 `SKILL.md` body：

- 入参：`{ name: string }` —— 与 SKILL.md frontmatter `name` 严格匹配
- 行为：从 `<package_dir>/skills/<name>/SKILL.md` 重新加载（不缓存，每次
  fresh-load —— 跟 `http::skills::load_registry` 同一契约），返回完整
  metadata + instructions
- 复用现有 SKILL.md parser（ADR-009 §V-A 的单一权威 parser），不引入第二份

### 4. 强联动：load_skill 是 Progressive 的唯一开关

`load_skill.enabled` 同时控制两件事：

1. **LLM 能否调用 `load_skill` 工具** —— 走标准的
   `UpdateBuiltinTools { enabled: [...] }` 广播路径，
   `ContextBuilder::tool_definitions` 增删一项。
2. **system prompt 是否注入 skills 摘要** —— 走
   `UpdateSkillInstructions { instructions: Option<String> }` 广播路径，
   `ContextBuilder::skill_summary` 增删一段（渲染后仍以
   `skill_instructions` 这个 section key 出现在 wire 上，见 §7）。

两条路径必须在同一次 toggle 里同步派发，**否则 LLM 会看到不一致的状态**（要么有摘要但没工具，要么有工具但 LLM 不知道有哪些 skill 可调）。

实现：把 `load_skill` toggle 集中到一个 helper
[`broadcast_builtin_tools_update`](../../core/acowork-runtime/src/http/server.rs)
—— 它先 dispatch `UpdateBuiltinTools`，**紧接着** dispatch
`UpdateSkillInstructions`（调用
[`rebuild_and_dispatch_skill_instructions`](../../core/acowork-runtime/src/http/prompts.rs)）。
两条 dispatch 走同一条 agent-level 链路（`dispatch_agent_level_config` →
`SessionManager::apply_*` → per-session `ContextBuilder::set_*`），
future session 模板（`SessionManagerConfig`）也同步更新。

#### 为什么必须联动 —— 三个用户场景

| 场景 | 不联动会出现 | 联动后的状态 |
|---|---|---|
| 用户开 load_skill，希望 LLM 知道有哪些 skill | 工具开了但 system prompt 没有摘要 → LLM 不知道有 skill 可调 | 工具 + 摘要都开了 |
| 用户关 load_skill，但 Progressive 仍 enabled → 工具消失但摘要仍在 | LLM 看到 `["greeting", "code_review"]` 但调 `load_skill("greeting")` 报"tool not available" | 工具 + 摘要都关了 |
| 用户首次开 load_skill | （无） | 单次 PUT 同步打开，零额外配置 |

#### 为什么不能用"两个独立字段 + 关联逻辑"

"Auto-load skills" + "load_skill 工具" 两个独立 Switch 加关联逻辑（任一
切换自动改另一个）的方案已经在 v1 试过，遇到了三个回归：

1. **UI 漂移**：Agent Setup 面板与 Tools 面板之间没有实时同步 —— Desktop
   重启后 Agent Setup 显示 ON，Tools 面板显示 OFF，必须两边手动对齐。
2. **PUT 顺序竞争**：用户并行 PUT 两次（一次改 progressive，一次改
   load_skill.enabled），后写覆盖前写 → 最终配置 ≠ 用户意图。
3. **第二份字段 → 第二份数据源 → 第二份失效路径**：`agent_config.json.skills_auto_load`
   跟 `agent_tools.json::load_skill.enabled` 必须在三处保持一致（CLI resolve、
   HTTP prompts rebuild、SessionManager template）—— 任何一处漏更新就
   是 bug。

强联动 = 单一字段 + 单一 dispatch helper + 三处共路径，**根除**上述三类问题。

#### 为什么不让 load_skill 自身"自动启用 Progressive"

- `load_skill` 是一个 peer of every other built-in tool（`memory_recall`
  / `file_read` / `http_request` / ...），不应该享受"自我启用"特权。
- "工具 enabled 默认推断 Progressive 开启" = 静默改用户配置，违反 opt-in
  契约。
- 反过来才合理：**Progressive 模式开启的充分必要条件就是
  `load_skill.enabled == true`**。tool 状态是 source of truth，Progressive
  是其下游派生状态。

### 5. 缓存友好性

- `resolve_skill_mode` 在 `agent_init` 启动时一次性解析，结果固化为
  启动时的 system prompt
- session 启动后 system prompt **不再改变**，保证 prompt cache 命中
- 用户在 session 中切 `load_skill` Switch → 立即 dispatch
  `UpdateBuiltinTools` + `UpdateSkillInstructions` 两条事件 →
  `ContextBuilder` 在下一次 `build_chat_request` 前同步重建模板 →
  当前 session 内的下一次 LLM 请求就看到新状态（既有 fast channel）

### 6. 死代码清理

之前的 `{work_dir}/.agent_skills.json` 路径（`AgentSkillsOverride` +
`resolve_skill_mode` 旧版）只有 reader，**没有 writer、没有 HTTP API、
没有 UI 入口** —— 死代码等价物。本 ADR 一并删除：

- 删除 [core/acowork-runtime/src/cli.rs](../../core/acowork-runtime/src/cli.rs) 中 `AgentSkillsOverride` + 旧 `resolve_skill_mode`
- 重写 `resolve_skill_mode` 为读 `agent_tools.json::load_skill.enabled`
- `startup/mod.rs` 的 `super_mod` re-export 保留（仅加注释）

2026-10 修订版额外清理：

- 删除 `AgentConfig.skills_auto_load: Option<bool>` 字段
  （[agent_config.rs](../../core/acowork-runtime/src/agent_config.rs)）
- 删除 `UpdateAgentConfigRequest.skills_auto_load` 字段
  （[http/server.rs](../../core/acowork-runtime/src/http/server.rs)）
- 删除 `ConfigField::SkillsAutoLoad` 枚举变体
  （[usecases/agent_config.rs](../../core/acowork-runtime/src/usecases/agent_config.rs)）
- 删除 `RuntimeConfigOverrides.skills_auto_load` 字段及
  `is_empty` / `merge` / `apply_to_config` / `From<&AgentConfig>` 中
  相关投影
  （[session_manager.rs](../../core/acowork-runtime/src/agent/session/session_manager.rs)）
- 删除 AgentSetupTab 中 Switch + WiredField + Debounce + i18n 条目
  （[AgentSetupTab.tsx](../../apps/acowork-desktop/src/components/right-panel/AgentSetupTab.tsx) + [i18n](../../apps/acowork-desktop/src/i18n/locales/)）
- 删除 `agentSetupConfigSync` 中 `skills_auto_load` wire 字段
  （[agentSetupConfigSync.ts](../../apps/acowork-desktop/src/components/right-panel/agentSetupConfigSync.ts)）
- 删除 `AgentProfileSettings.skillsAutoLoad` 字段
  （[agentStore.ts](../../apps/acowork-desktop/src/stores/agentStore.ts)）

### 7. skills 摘要作为独立 section 透传（复用既有数据模型）

Progressive 模式注入的 skills 摘要（仅 name + description）必须以**独立 section key** 出现在 `compute_section_sizes` 输出里，**否则**前端 usage 菜单的 skills 分类（[`SKILL_SECTION_KEYS`](../../apps/acowork-desktop/src/lib/contextUsageBreakdown.ts)）永远匹配不到（百分比恒为 0%），Debug 面板的 `SECTION_LABELS["skill_instructions"]` 也永远没有数据。

本节明确规定：skills 摘要**不嵌入** `SystemPromptParts.base` 字符串，
而是作为 `Option<String>` 单独返回，由 caller 显式
`context_builder.set_skill_summary(skills)`。`compute_section_sizes`
已有对应 emit 逻辑（`if let Some(skills) = builder.skill_instructions() { push skill_instructions section }`），无需新增。

事件链与 `UpdateSystemPrompt` / `UpdateIdentityContext` 完全对称（agent-scoped template + per-session broadcast）：

| 环节 | 复用 |
|---|---|
| 字段 | `ContextBuilder::skill_summary: Option<String>`（会话级摘要）+ `ContextBuilder::active_skill: Option<String>`（当轮 command 选中，见 §7.1） |
| 共享模板 | `SessionManagerConfig::skill_summary: Option<String>`（新增） |
| Inbound 事件 | `InboundMessage::UpdateSkillInstructions { instructions: Option<String> }`（新增） |
| Session 事件 | `SessionMessage::UpdateSkillInstructions { instructions: Option<String> }`（新增） |
| 广播 | `SessionManager::apply_skill_instructions()`（新增） |
| wire 协议 | `SectionMeta { key, size_bytes, token_estimate, hash }` + `OnContextBuiltParams { sections: ContextSections }`（**既有**，通用） |
| Debug 渲染 | `DebugPanel::SECTION_LABELS["skill_instructions"]` + `SECTION_ORDER`（既有） |
| Usage 渲染 | `contextUsageBreakdown::SKILL_SECTION_KEYS` + `ContextUsageIcon::CATEGORY_META`（既有） |

**核心约束**：不发明新数据结构、新事件类型、新 section key —— `skill_instructions` 这个 key 从 emit 到 wire 到 render 全链路已经存在，**只是 `prompt_builder` 之前把它吞进了 base 字符串**。本节修复的是这个数据流断层，不是架构。

### 7.1 摘要槽位与当轮 skill 槽位分离（2026-10 修订）

上面的 **section key `skill_instructions` 保持不变**（前端 usage 菜单与 Debug
面板零改动），但 `ContextBuilder` 内部**不再用同一个字段**承载两种生命周期：

| 槽位 | 生命周期 | 写入方 | clear 的含义 |
|---|---|---|---|
| `skill_summary` | 会话常驻 | Phase A 播种、`apply_skill_instructions` 广播、debug patch | 切到 Manual，摘要撤下 |
| `active_skill` | 仅当轮 | `SessionMessage::ChatMessage::active_skill`（前端 `command` → `SkillRegistry` 解析） | 本轮没选 skill，撤下上轮的，防跨轮泄漏 |

渲染时由 `ContextBuilder::skill_section()` 组合（摘要在前、
`### Selected skill` 在后），对外仍是一个 `## Skill Instructions` 区块；
`skill_instructions()` 返回该组合文本，`compute_section_sizes` 的 emit 逻辑不变。

**为什么必须拆**：两个槽位原本共用一个字段，于是"防止 skill 跨轮泄漏"的
per-turn 清理（`None` 分支，也就是正常聊天的默认路径）**顺带摧毁了**会话级
摘要 —— 表现是 Progressive 模式下 LLM 永远看不到 skills 列表，新会话第一条
消息即失效。清理对 `active_skill` 是正确的，对 `skill_summary` 是毁灭性的；
挤在同一字段里就必然出错。

**为什么不在 caller 侧存一份摘要副本再恢复**：那会让摘要出现第二个权威源，
此后每个写入方（播种 / 热更新广播 / debug patch / per-turn）都必须记得同步它，
漏一个就是下一个同类 bug —— 与本 ADR §4 拒绝"两个独立字段 + 关联逻辑"的理由
同构。拆字段让误用在类型层不可表达：public `clear_skill_instructions()` 已
删除，任何想"清 skill"的代码必须显式回答清哪个生命周期。

**遗留（不在本节范围）**：`active_skill` 落在 Block A（system prompt）会让
该区块在选用 skill 的轮次变化，影响 ADR-060 的 prompt cache 命中。这是拆分前
就存在的行为，本次修复既不引入回归也不顺手改；若要挪到非缓存段需单独决策。

## 拒绝的方案

### 方案 A：保留 `.agent_skills.json` 单独文件

- 现状：reader 已有但无 writer/HTTP/UI
- 修复方案 = 新建完整 infra（`AgentSkillsOverrideService` trait + `usecases` 路由 +
  HTTP API + Gateway 反代 + Frontend fetch + UI）
- 工作量：~200 行代码 + 多个文件
- 缺点：跟 `agent_config.json`（temperature、max_iterations 等同类配置）
  职责重叠，破坏 ADR-040 分层
- **拒绝理由**：YAGNI + DRY

### 方案 B：把 progressive 设为 manifest 默认 true（不可覆盖）

- 优点：用户零配置
- 缺点：所有 agent 一律开 Progressive，违反"per-agent 应该是 per-agent 配置"
  原则；用户无法对单个 agent 关闭（即使他们不想）
- **拒绝理由**：违反"per-agent 默认值应该 per-agent 控制"的设计原则

### 方案 C：v1 方案 —— 两个独立字段 + 隐式联动

- 实施：保留 `agent_config.json::skills_auto_load` Switch 与
  `load_skill` 工具 Switch 各自独立，PUT 处理器在 progressive 切到
  true 时自动写 `load_skill.enabled = true`（反之亦然）。
- 已实装并撞坑：
  - 双向同步逻辑在 3 个文件里有 3 个不同实现（PUT handler、session
    spawn、test fixture），任一处遗漏就漂移
  - Desktop 重启后两个面板 UI 状态必须分别从两个 endpoint 拉取 →
    `agentSetupConfigSync` 第一次实现就漏了 `skills_auto_load` 字段
    （用户报告的 bug）
  - 关联 PUT 的写入顺序竞争（"progressive=true & load_skill=false"
    并发 PUT，后写覆盖前写）
- **拒绝理由**：两个独立 Switch 等于邀请漂移。**单一字段 = 单一来源 = 零漂移**。
  既然必须联动（见 §4），两个字段就是冗余设计 —— 直接把 toggle 挂在
  `load_skill` 工具开关上更简单且更一致。

### 方案 D：list_my_skills + load_full_skills 双工具方案

- 优点：简单会话零成本
- 缺点：每多一轮 tool 调用 → prompt cache 命中边界要重算（tool 输出进入消息历史）
- **拒绝理由**：cache 友好性劣于 Progressive；增加 tool schema 永久占位 ~200 tokens

## 实施清单

| # | 改动 | 文件 |
|---|---|---|
| 1 | 删除 `AgentSkillsOverride` 死代码 | [core/acowork-runtime/src/cli.rs](../../core/acowork-runtime/src/cli.rs) |
| 2 | 重写 `resolve_skill_mode`（读 agent_tools.json::load_skill.enabled） | [core/acowork-runtime/src/cli.rs](../../core/acowork-runtime/src/cli.rs) |
| 3 | 新增 `LoadSkillTool` builtin 工具 | [core/acowork-runtime/src/tools/builtin/load_skill.rs](../../core/acowork-runtime/src/tools/builtin/load_skill.rs) |
| 4 | 注册 `load_skill` 到 `all_builtin_tools()` | [tools/builtin/mod.rs](../../core/acowork-runtime/src/tools/builtin/mod.rs) |
| 5 | `BUILTIN_TOOLS` 静态列表加 `load_skill` | [acowork-core/src/tools/mod.rs](../../core/acowork-core/src/tools/mod.rs) |
| 6 | 7 个示例 manifest `[[tools]]` 段加 `load_skill`（种子化） | [examples/](../../examples/) |
| 7 | `broadcast_builtin_tools_update` 内部额外 dispatch `UpdateSkillInstructions` | [http/server.rs](../../core/acowork-runtime/src/http/server.rs) |
| 8 | Frontend i18n `load_skill` 描述文案补充"开 = 自动注入 skills 摘要" | [i18n/](../../apps/acowork-desktop/src/i18n/locales/) |
| 9 | 测试 `test_compute_skill_instructions_payload_priority` 覆盖 `load_skill.enabled` 三个 cell | [http/prompts.rs](../../core/acowork-runtime/src/http/prompts.rs) |
| 10 | ~~`AgentConfig` 加 `skills_auto_load: Option<bool>` 字段~~（2026-10 删除） | n/a |
| 11 | ~~`UpdateAgentConfigRequest` 加 `skills_auto_load`~~（2026-10 删除） | n/a |
| 12 | ~~`ConfigField::SkillsAutoLoad` + dispatch + projection~~（2026-10 删除） | n/a |
| 13 | ~~`RuntimeConfigOverrides.skills_auto_load`~~（2026-10 删除） | n/a |
| 14 | ~~AgentSetupTab Session Parameters Switch + WiredField + i18n~~（2026-10 删除） | n/a |
| 15 | ~~`AgentProfileSettings.skillsAutoLoad`~~（2026-10 删除） | n/a |

## 风险与护栏

- **load_skill 在示例 manifest 中已种子化为 enabled=true**，与所有其他
  内置工具走相同的首次启动路径
- **session 进行中切 load_skill**：通过 fast channel 立即生效，下一次
  `build_chat_request` 前 `ContextBuilder` 完成重建 —— 既有 dispatch
  链路覆盖
- **用户关了 load_skill**（= 显式关 Progressive）：LLM 看不到 skill 摘要、
  看不到 `load_skill` 工具；这是预期行为，不再有"摘要还在但工具没了"
  的不一致态
- **单次 toggle 单次 dispatch**：把两条事件打包在
  `broadcast_builtin_tools_update` 一个 helper 里，避免后续在两个
  endpoint 上分别加补丁时再次引入漂移
