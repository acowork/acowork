# Skill System（技能系统）

> 版本：v3.2 | 更新日期：2026-04-14

---

Skill 是 Agent 行为模式的扩展机制。LLM 通过 Skill Instructions 获得特定领域的知识和操作流程，从而具备超出基础能力的专业行为。

Skill 系统采用**双层模型**：SKILL.md 作为静态定义层（发布态），SQLite 记忆层 作为动态经验层（运行态）。调试阶段的 Skill 在 SQLite 记忆层中迭代，完善后提交到 SKILL.md。

## 1. 架构概览

```
Skill System
├── Definition Layer（静态）    ← SKILL.md 文件，随 .agent 包分发
│   ├── YAML frontmatter        元数据（名称、触发词、工具依赖、模型兼容性快照）
│   └── Markdown body           指令正文（执行步骤、注意事项、输出格式）
│
├── Experience Layer（动态）    ← SQLite nodes 行，Agent 私有
│   ├── SkillDraft              草稿 Skill（调试阶段）
│   ├── SkillIteration          迭代版本（每次修改的快照）
│   ├── SkillExecution          执行记录（每次试运行的结果）
│   └── SkillExperience         已发布 Skill 的运行经验
│
└── Runtime Integration
    ├── Skill Loader            加载 SKILL.md + 查询 SQLite 经验层
    ├── Prompt Builder          合并静态定义 + 动态经验 + 模型适配
    └── Debug Controller        调试模式（创建/试运行/迭代/发布）
```

**双层模型的设计原则：**

| 层级 | 存储位置 | 生命周期 | 可分发 | 可审计 | 可版本控制 |
|------|---------|---------|--------|--------|-----------|
| Definition Layer | SKILL.md | 随 .agent 包版本管理 | 是 | 是（直接打开看） | 是 |
| Experience Layer | SQLite 记忆层 | 持久化到 Agent 工作区 | 否（私有数据） | 否（图数据库） | 否 |

类比：SKILL.md 是**教科书**（公共的、标准的、可分享的），SQLite 经验层是**个人笔记**（私有的、基于实践的、因人而异的）。两者结合才能形成完整的 Skill 行为。

## 2. SKILL.md 格式（静态定义层）

SKILL.md 采用 YAML frontmatter + Markdown body 格式，兼容 Agent Skills 开放标准（agentskills.io）。

### 2.1 文件位置

```
<agent_id>.agent
└── skills/
    └── <skill_name>/
        ├── SKILL.md           # 必需，Skill 定义
        └── references/        # 可选，补充文档、模板数据
            ├── template.md
            └── examples.json
```

### 2.2 完整格式

```yaml
---
# === 元数据 ===
name: weekly-report
description: 汇总本周工作内容生成结构化周报
version: "1.0.0"
author: agent                    # "agent" = Agent 自创建，"developer" = 开发者手写
source_draft: draft-abc123       # Agent 自创建时关联的草稿 ID（可追溯）

# === 触发条件 ===
triggers:
  - 周报
  - weekly report
  - 总结本周

# === 工具依赖 ===
tool_deps:
  # 记忆工具不再是内置工具，而是 Gateway 托管记忆服务的 MCP 工具（ADR-101）；
  # 注册名带 `mcp_<server>__` 前缀，工具级开关在 `agent_mcp_tools.json`（ADR-069）。
  - mcp_memory__memory_recall
  - file_write

# === 平台兼容性（可选）===
platforms:
  desktop: required              # 桌面端必需
  mobile: optional               # 移动端可选（行为可能降级）

# === 模型兼容性（发布时快照，运行时以 SQLite 记忆层 为准）===
tested_models:
  - provider: openai
    model: gpt-4o
    rating: excellent
  - provider: ollama
    model: qwen3:8b
    rating: good
    note: "需要扁平化指令适配"
---

# Weekly Report Skill

## 执行步骤

1. 使用 `mcp_memory__memory_recall` 检索本周的对话和工作记录
2. 按项目分类整理完成事项
3. 生成结构化周报：
   - 本周完成事项（含进展说明）
   - 进行中事项（含阻塞因素）
   - 下周计划
4. 使用 `file_write` 保存到用户指定路径

## 输出格式

使用 Markdown 格式，每个项目一个小节，控制在 500 字以内。

## 注意事项

- 如果本周无工作记录，回复"本周暂无工作记录"
- 优先使用 mcp_memory__memory_recall 获取数据，而非要求用户重复提供
- 输出风格默认简洁，除非用户明确要求详细版
```

### 2.3 字段说明

| 字段 | 必需 | 说明 |
|------|------|------|
| `name` | 是 | Skill 名称，在 Agent 内唯一 |
| `description` | 是 | 功能描述，用于 Skill 搜索和展示 |
| `version` | 否 | Skill 版本号（语义版本） |
| `author` | 否 | 创建者：`agent`（Agent 自学习创建）或 `developer`（开发者手写） |
| `source_draft` | 否 | Agent 自创建时的草稿 ID，用于追溯到调试历史 |
| `triggers` | 是 | 触发词列表，LLM 根据用户输入匹配 |
| `tool_deps` | 否 | 依赖的 Built-in / WASM 工具，Runtime 用于权限校验 |
| `platforms` | 否 | 平台支持声明，见 2.4 |
| `tested_models` | 否 | 模型兼容性快照，见第 5 节 |

### 2.4 平台兼容性声明

Skill 可以声明平台支持级别，与 Tools 的平台机制一致（见 12-tool-system.md 第 2.1 节）：

```yaml
platforms:
  desktop: required     # 桌面端必需，移动端安装被拒绝
  mobile: optional      # 移动端可选，行为可能降级
  # desktop: true       # 简写，等同于 required
  # mobile: false       # 简写，等同于不支持
```

| 值 | 含义 | 移动端安装 |
|---|------|-----------|
| `required` | 此 Skill 核心依赖桌面端能力 | 拒绝安装 |
| `optional` | 可在移动端降级运行 | 允许安装，但行为受限 |
| 不声明 | 默认全平台 | 允许安装 |

**降级场景示例：** 一个依赖 `shell` 工具的 "DevOps Deploy" Skill 声明 `desktop: required`，因为 `shell` 在移动端不可用。一个依赖 `web_fetch` 的 "News Digest" Skill 默认全平台，因为 `web_fetch` 全平台可用。

## 3. SQLite 记忆层经验层（动态运行态）

Agent 在使用 Skill 的过程中积累的经验数据存储在 SQLite 中，作为 SKILL.md 静态定义之上的增强层。

### 3.1 节点类型概览

```
SQLite 记忆层（`nodes` 表）
│
├─ SkillDraft            草稿 Skill（调试阶段，未发布）
├─ SkillIteration        迭代版本（每次修改草稿时的快照）
├─ SkillExecution        执行记录（每次试运行的结果）
└─ SkillExperience       已发布 Skill 的运行经验（成功模式、失败教训、用户偏好、模型兼容性）
```

### 3.2 SkillDraft（草稿 Skill）

调试阶段，Agent 自创建或用户引导创建的 Skill 存储为 SkillDraft 节点。

```rust
struct SkillDraft {
    draft_id: String,              // 唯一 ID（自动生成，如 "draft-abc123"）
    skill_name: String,            // Skill 名称
    description: String,           // 描述
    instructions: String,          // 指令正文（Markdown）
    triggers: Vec<String>,         // 触发词
    tool_deps: Vec<String>,        // 依赖的工具
    created_at: DateTime,
    updated_at: DateTime,
    status: DraftStatus,           // 草稿状态
}

enum DraftStatus {
    Draft,       // 刚创建，尚未测试
    Testing,     // 正在调试中
    Ready,       // 调试完成，等待用户确认发布
    Published,   // 已发布到 SKILL.md
}
```

### 3.3 SkillIteration（迭代版本）

每次修改草稿时，保存当前版本的完整快照，形成完整的迭代历史。

```rust
struct SkillIteration {
    iteration_id: String,
    draft_id: String,              // 关联的草稿
    version: u32,                  // 第几轮迭代（从 1 开始）
    instructions: String,          // 本轮的指令内容（完整快照）
    triggers: Vec<String>,
    tool_deps: Vec<String>,
    change_summary: String,        // 修改说明（"增加了错误处理步骤"）
    trigger_reason: String,        // 什么触发了这次修改（"运行失败：缺少数据收集"）
    created_at: DateTime,
}
```

### 3.4 SkillExecution（执行记录）

每次试运行的完整记录，关联到草稿和迭代版本。

```rust
struct SkillExecution {
    execution_id: String,
    draft_id: String,              // 关联的草稿
    iteration_id: String,          // 在哪个迭代版本执行的
    outcome: ExecutionOutcome,     // 执行结果
    user_feedback: Option<String>, // 用户反馈（"输出太长了"）
    error_detail: Option<String>,  // 失败详情
    duration_ms: u64,              // 执行耗时

    // 模型信息
    model_provider: String,        // "openai" / "claude" / "ollama"
    model_name: String,            // "gpt-4o" / "claude-sonnet-4-20250514" / "qwen3:8b"
    model_params: Option<ModelParams>,
    created_at: DateTime,
}

struct ModelParams {
    temperature: f32,
    max_tokens: u32,
}

enum ExecutionOutcome {
    Success,      // 完全成功
    Partial,      // 部分成功（完成但有瑕疵）
    Failure,      // 执行失败
    Skipped,      // 跳过（如权限不足）
}
```

### 3.5 SkillExperience（已发布 Skill 的运行经验）

Skill 发布后，运行时继续积累经验。这些经验在后续执行时作为补充注入。

```rust
struct SkillExperience {
    skill_id: String,              // 对应 SKILL.md 的 name
    usage_count: u64,              // 总使用次数
    success_count: u64,            // 成功次数
    last_used: DateTime,

    // 从实践中学习到的模式
    learned_patterns: Vec<LearnedPattern>,
    failure_cases: Vec<FailureCase>,
    user_preferences: HashMap<String, String>,

    // 模型兼容性
    // 联动：当某模型上某类任务成功率低于 60% 时，触发 AutobiographicalNode Limitation 节点自动更新
    // 详见 05-memory.md Phase 2「自我评估驱动的 AutobiographicalNode 更新」
    model_compatibility: HashMap<ModelKey, ModelCompatibility>,
}

struct LearnedPattern {
    pattern: String,               // "用户只说'天气'时，默认查当前城市"
    context: String,               // 什么场景下学到的
    confirmed_count: u32,          // 被验证了多少次
}

struct FailureCase {
    case: String,                  // "api.weather.com 偶尔返回 503"
    workaround: Option<String>,    // 解决方案（"重试一次通常能成功"）
    occurrence_count: u32,         // 发生次数
}

struct ModelKey {
    provider: String,
    model: String,
}

struct ModelCompatibility {
    tested: bool,
    test_count: u32,
    success_count: u32,
    last_tested: DateTime,
    rating: CompatibilityRating,
    adaptations: Vec<ModelAdaptation>,
    known_issues: Vec<String>,
}

enum CompatibilityRating {
    Excellent,   // 成功率 > 90%
    Good,        // 成功率 > 70%，有少量已知问题
    Limited,     // 成功率 > 50%，需要模型专属适配
    Untested,    // 未测试
}

struct ModelAdaptation {
    adaptation: String,            // "使用更简短的指令，避免复杂嵌套"
    reason: String,                // "qwen3:8b 对长指令遵循率低"
    created_at: DateTime,
}
```

### 3.6 节点关系图

```
SkillDraft (当前草稿)
  │
  ├─ [HAS_ITERATION] → SkillIteration #1（初始版本）
  │                       │
  │                       ├─ [EXECUTED_AS] → SkillExecution #1（Failure）
  │                       │                     model: openai/gpt-4o
  │                       │                     feedback: "缺少数据收集步骤"
  │                       │
  │                       ├─ [EXECUTED_AS] → SkillExecution #2（Success）
  │                       │                     model: openai/gpt-4o
  │                       │
  │                       └─ [NEXT_ITERATION] → SkillIteration #2
  │                                               │
  │                                               ├─ [EXECUTED_AS] → SkillExecution #3（Partial）
  │                                               │                     model: ollama/qwen3:8b
  │                                               │                     feedback: "输出格式混乱"
  │                                               │
  │                                               └─ [NEXT_ITERATION] → SkillIteration #3（最终版）
  │
  └─ [PUBLISHED_AS] → SKILL.md (skills/weekly-report/SKILL.md)
                          │
                          └─ [HAS_EXPERIENCE] → SkillExperience
                                                  │
                                                  ├─ learned_patterns: [...]
                                                  ├─ model_compatibility:
                                                  │   "openai/gpt-4o": Excellent
                                                  │   "ollama/qwen3:8b": Good
                                                  │       └─ adaptations: ["扁平化指令"]
                                                  └─ user_preferences:
                                                      output_style: "concise"
```

## 4. Skill 生命周期

Skill 的完整生命周期分为三个阶段：创建与调试、发布、运行与进化。

### 4.1 Phase 1：创建与调试（纯 SQLite 记忆层）

```
用户：学一下怎么帮我做周报总结
       │
       ▼
① Agent 在 SQLite 记录 SkillDraft
   status = Draft
       │
       ▼
② 进入 Debug 模式，试运行
       │
       ├─ 运行 1 → SkillExecution (Failure)
       │   → Agent 修改草稿 → SkillIteration #2
       │   → SQLite 记录 failure_case + change_summary
       │
       ├─ 运行 2 → SkillExecution (Partial)
       │   → 用户反馈："输出太长了"
       │   → Agent 修改草稿 → SkillIteration #3
       │
       └─ 运行 3 → SkillExecution (Success) ✓
       │
       ▼
③ Agent 标记草稿为 Ready
   status = Ready
       │
       ▼
④ 用户审阅（可选）
   ├─ "看看调试历史" → 展示所有 Iteration 和 Execution 记录
   ├─ "回退到版本 2" → 恢复 SkillIteration #2 的指令
   ├─ "保存草稿，下次继续" → status 保持 Ready
   └─ "发布吧" → 进入 Phase 2
```

**Debug 模式的关键能力：**

| 能力 | 说明 |
|------|------|
| 试运行 | 在 Debug 模式下执行 Skill，结果不写入生产记忆 |
| 迭代修改 | Agent 根据执行结果自动修改草稿指令 |
| 历史回溯 | 查看任意迭代版本和当时的执行记录 |
| 用户反馈 | 用户可以对每次执行结果给出反馈 |
| 草稿保存 | 中断后下次继续，草稿状态完整保留 |
| 模型切换 | 可在不同模型上试运行，验证跨模型兼容性 |

### 4.2 Phase 2：发布（SQLite 记忆层 → SKILL.md）

用户确认 Skill 调试完成后，Runtime 执行发布操作：

```
① 从 SQLite 读取 SkillDraft 最终状态
   （最新 SkillIteration 的 instructions + 元数据）
       │
       ▼
② 生成 YAML frontmatter
   name / description / triggers / tool_deps
   从 SkillIteration 提取
       │
       ▼
③ 生成 tested_models 快照
   从 model_compatibility 提取已测试的模型及其 rating
       │
       ▼
④ 写入 skills/<skill_name>/SKILL.md
       │
       ▼
⑤ 更新 SQLite
   ├─ SkillDraft.status = Published
   ├─ 创建 SkillExperience 节点
   │   （从调试期间的 learned_patterns / model_compatibility 迁移）
   └─ 关联 SkillDraft → SKILL.md
       │
       ▼
⑥ 通知用户发布完成
```

**发布后的 SKILL.md 示例（自动生成）：**

```yaml
---
name: weekly-report
description: 汇总本周工作内容生成结构化周报
version: "1.0.0"
author: agent
source_draft: draft-abc123
triggers:
  - 周报
  - weekly report
  - 总结本周
tool_deps:
  - mcp_memory__memory_recall
  - file_write
tested_models:
  - provider: openai
    model: gpt-4o
    rating: excellent
  - provider: ollama
    model: qwen3:8b
    rating: good
    note: "需要扁平化指令适配"
---

# Weekly Report Skill

（Markdown body，来自最终迭代版本的 instructions）
```

### 4.3 Phase 3：运行与进化（SKILL.md + SQLite 经验层）

发布后的 Skill 在每次执行时，Runtime 组装完整的上下文：

```
Skill Loader 加载 SKILL.md（静态定义）
       │
       ▼
SQLite 记忆层 查询 SkillExperience 节点（动态经验）
       │
       ├─ 无经验节点（首次发布后第一次使用）
       │   → 直接使用 SKILL.md 原始指令
       │
       └─ 有经验节点 → 合并为增强版 Skill 指令：
            │
            ├─ 基础指令来自 SKILL.md
            ├─ 追加 learned_patterns 作为补充提示
            ├─ 追加 user_preferences 作为约束
            ├─ 追加 failure_cases 作为注意事项
            └─ 注入当前模型的 adaptations（如有）
       │
       ▼
执行结果写入 SQLite
       │
       ├─ 成功 → 情景记忆 + 更新 SkillExperience.success_count
       ├─ 失败 → 更新 SkillExperience.failure_cases
       └─ 用户反馈 → 更新 SkillExperience.user_preferences
```

**自学习闭环：**

```
执行 Skill → 记录结果 → 积累经验 → 增强下次执行
     ↑                                        │
     └────────────────────────────────────────┘
```

当经验积累到一定程度（如 learned_patterns 超过 5 条，或 model_compatibility 新增了低评分模型），Runtime 可以提示用户：

> "weekly-report Skill 已积累 12 条新经验（3 条成功模式、1 个新模型的适配），建议进入调试模式更新 Skill 定义。"

同时，failure_cases 积累超过 3 条同类失败时，会触发 Skill ↔ ProceduralNode 联动提取（详见 05-memory.md Phase 2），生成跨 Skill 的通用行为模式。

用户确认后，将经验层合并回 SKILL.md，形成新一轮的发布。

## 5. 模型兼容性

Skill 的执行效果与 LLM 强相关。同一个 Skill 在不同模型上可能表现差异巨大。

### 5.1 模型兼容性记录

每个 Skill 的模型兼容性通过两层记录：

| 层级 | 存储 | 内容 | 用途 |
|------|------|------|------|
| 调试阶段 | SkillExecution 节点 | 每次试运行的模型 + 结果 | 精确回溯每次测试 |
| 经验阶段 | SkillExperience.model_compatibility | 按模型聚合的兼容性数据 | 运行时决策依据 |
| 发布态 | SKILL.md tested_models | 兼容性快照 | 分发时参考 |

### 5.2 运行时模型检查

Runtime 在组装 Skill 上下文时，检查当前模型兼容性：

```
检查当前模型 (model_provider / model_name)
       │
       ├─ Excellent / Good
       │   → 正常执行
       │
       ├─ Limited
       │   → 注入 model adaptations 作为补充提示
       │   → 例："注意：当前模型需要使用简短直接的指令格式。"
       │
       └─ Untested（首次在此模型上执行）
           ├─ 不阻塞执行
           ├─ 自动记录执行结果到 model_compatibility
           └─ 如果连续失败 3 次，通知用户：
              "此 Skill 在当前模型上效果不佳，
               建议切换到已验证模型或进入调试模式适配"
```

### 5.3 跨模型调试

用户可以在 Debug 模式下切换模型试运行，验证 Skill 的跨模型兼容性：

```
在 GPT-4o 上调试完成 → 发布
       │
       ▼
切到 qwen3:8b → 发现指令遵循率低
       │
       ▼
在 qwen3:8b 上调试适配 → 精简指令格式
       │
       ▼
发布更新 → SKILL.md 的 tested_models 增加两条记录
```

## 6. Runtime 集成

### 6.1 Skill Loader

Skill Loader 负责加载 SKILL.md 并查询 SQLite 经验层：

```rust
struct SkillLoader {
    memory_store: Arc<SqliteStore>,
}

struct LoadedSkill {
    name: String,
    definition: SkillDefinition,      // 来自 SKILL.md
    experience: Option<SkillExperience>, // 来自 SQLite 记忆层（可能为空）
    model_adaptations: Vec<String>,   // 当前模型的适配指令
}

impl SkillLoader {
    /// 加载已发布的 Skill
    fn load_published(&self, skill_name: &str, current_model: &ModelKey)
        -> Result<LoadedSkill>
    {
        // 1. 读取 skills/<skill_name>/SKILL.md
        let definition = self.parse_skill_md(skill_name)?;

        // 2. 查询 SQLite 记忆层 的 SkillExperience
        let experience = self.memory_store.get_skill_experience(skill_name)?;

        // 3. 提取当前模型的适配指令
        let model_adaptations = experience
            .as_ref()
            .and_then(|exp| exp.model_compatibility.get(current_model))
            .map(|mc| mc.adaptations.iter().map(|a| a.adaptation.clone()).collect())
            .unwrap_or_default();

        Ok(LoadedSkill { name: skill_name.to_string(), definition, experience, model_adaptations })
    }

    /// 加载草稿 Skill（调试模式）
    fn load_draft(&self, draft_id: &str) -> Result<SkillDraft> { ... }
}
```

### 6.2 Prompt Builder 中的 Skill 注入

Prompt Builder 将 Skill 注入到 System Prompt 时，合并静态定义和动态经验：

```
System Prompt 组装（步骤 5：Skill Instructions）
       │
       ▼
对每个已加载的 Skill：
       │
       ├─ 基础指令
       │   来自 SKILL.md 的 Markdown body
       │
       ├─ 经验补充（如有 SkillExperience）
       │   ## 从经验中学到的
       │   - {learned_pattern_1}
       │   - {learned_pattern_2}
       │
       ├─ 用户偏好（如有）
       │   ## 用户偏好
       │   - 输出风格：concise
       │   - 跳过穿衣建议
       │
       └─ 模型适配（如有 model_adaptations）
       ## 当前模型适配
       - 使用简短直接的指令格式，避免嵌套列表
```

### 6.3 上下文裁剪优先级

当 Token 预算不足时，Skill 内容的裁剪顺序（见 03-agent-runtime.md）：

| 优先级 | 内容 | 裁剪顺序 |
|--------|------|---------|
| 最高 | SKILL.md 基础指令 | 最后裁剪 |
| 中 | 模型适配指令 | 优先裁剪（Loss of quality 但不致命） |
| 中 | 经验补充（learned_patterns） | 次优先裁剪 |
| 低 | 用户偏好 | 优先裁剪 |
| 最低 | 失败教训（failure_cases） | 最先裁剪 |

## 7. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| 双层模型 | SKILL.md（静态）+ SQLite 经验层（动态） | SKILL.md 保证可分发、可审计、可版本控制；SQLite 记忆层 支持自学习和经验积累 |
| 调试在 SQLite 记忆层 | 不直接修改 SKILL.md | 调试是探索性过程，需要迭代历史、回滚、A/B 测试，图数据库天然支持 |
| 单向提交 | SQLite 记忆层 → SKILL.md | 类似 git 工作流：工作区迭代 → commit 到仓库 |
| 模型兼容性记录 | SkillExecution + SkillExperience | Skill 效果与 LLM 强相关，不同模型需要不同适配，必须有记录 |
| SKILL.md 格式 | YAML frontmatter + Markdown | 兼容 Agent Skills 开放标准（agentskills.io），六大主流平台事实标准 |
| 经验注入而非替换 | 运行时合并，不修改 SKILL.md | 保证 SKILL.md 作为稳定基准，经验作为动态增强层叠加 |
| 上下文裁剪 | 经验层优先裁剪 | 基础指令是 Skill 的核心逻辑，经验是锦上添花 |
| 草稿不进包 | SkillDraft 仅存 SQLite 记忆层 | 未发布的草稿不应该作为包的一部分分发 |

## 8. 未来扩展

### 8.1 Skill 级联降级（Phase 4+）

当 Skill 依赖的工具在当前平台不可用时（如移动端 `shell` 不可用），Skill 应能自动降级而非整体失效。

**愿景：**

- SKILL.md 的 `tool_deps` 标记每个依赖为 `required` 或 `optional`（默认 optional）
- `required` 工具不可用时，Skill 整体跳过，不注入 System Prompt
- `optional` 工具不可用时，Skill 仍注入，但在指令中标注"此步骤在当前平台不可用，请跳过"
- Runtime 在组装 Skill 上下文时，对比 `tool_deps` 与当前平台可用工具列表，生成降级版本指令

**待解决的设计问题：**

- "跳过某步骤"对 LLM 来说是否足够清晰？是否需要生成替代步骤？
- 降级后的 Skill 是否需要记录到 SkillExperience？如何区分完整执行和降级执行？
- 是否需要在 SKILL.md 中显式声明降级策略（类似 `on_tool_unavailable: skip_step | skip_skill | fallback`）？

这些问题需要在 Skill 系统稳定运行后，通过实际场景积累答案后再做详细设计。

