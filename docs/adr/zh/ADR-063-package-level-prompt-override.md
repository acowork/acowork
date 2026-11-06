# ADR-063：包级 LLM 提示词覆盖机制 - `prompts/` 特殊文件名约定扩展

> **面板位置修订（2026-11-06，[ADR-108](./ADR-108-right-panel-consolidation.md)）**：本文中「位于 Debug 面板顶部」的 Prompts & Skills 卡片现已迁入**设置面板**（`AgentSetupTab`）的 Prompts & Skills 卡片；Debug 面板只保留调试开关与上下文快照。卡片的读取/覆盖逻辑、来源标注与失败分支语义均未改变；改动范围见 ADR-108。


**状态**：已定案
**日期**：2026-09-20
**决策者**：大鱼
**前置**：
- [ADR-053](./ADR-053-agent-specific-compaction-prompt.md)（`prompts/summary.md` 覆盖 `COMPACTION_SYSTEM_PROMPT`）
- [ADR-060](./ADR-060-prompt-cache-friendly-context-block-reorg.md)（稳定前缀 + 末尾追加，限制本 ADR 的可覆盖范围）
- [ADR-061](./ADR-061-context-compression-byte-budget.md)（压缩协议的 `<summary>` / `<user_intent>` 输出格式约束）

---

## 1. 决策摘要

`prompts/summary.md`（ADR-053）已经证明"包级文件名覆盖内置常量"是简单、零成本、对称于 system prompt 的模式。但该机制**仅适用于压缩/蒸馏这一条指令**，`core/acowork-runtime/src/prompt.rs` 与下游 `acowork-grafeo` / `acowork-memory` 中仍有 **8 个 LLM 指令性 prompt 常量**沿用硬编码内置值，无法被 `.agent` 包作者按需改写。

本 ADR 沿用 ADR-053 的同构约定，把这 8 个常量统一接入 `prompts/` 文件名覆盖机制：

1. **`prompt.rs` 4 个独立常量**：`PROMPT_BUILDER_FALLBACK` / `SEARCH_SYSTEM_PROMPT` / `COMPACT_PROMPT` / `TITLE_PROMPT`——分别约定为 `fallback.md` / `search.md` / `compact-template.md` / `title.md`。
2. **grafeo / memory 4 个常量**：`EXTRACTION_SYSTEM_PROMPT` / `CONFLICT_CLASSIFICATION_PROMPT` / `GENERALIZATION_PROMPT` / `DEFAULT_ABSTENTION_PROMPT`——分别约定为 `extraction.md` / `conflict-classification.md` / `generalization.md` / `abstention.md`。
3. **优先级链**（每条独立、各自回退）：
   ```
   prompts/<file>.md（包声明）  >  const PROMPT: &str in prompt.rs（内置兜底）
   ```
4. **加载收敛到 Phase A**：与 `compaction_prompt` 同路径——`agent_init.rs` 一次性加载，结果存入 `AgentBootContext`，Phase B 注入 `AgentCore` 新字段；Gateway 与 Standalone 两种模式行为一致。
5. **明确**不覆盖的三类（runtime 注入块 / 工具 description / 协议格式），并在 prompt-audit 文档中标注，理由见 §3.4。
6. **Debug 面板编辑入口**（§3.7）：配套 L1 文件读写 + L2 DevMode 重载两层机制——Runtime 新增 4 个 HTTP 端点（`/api/agents/{id}/prompts[/{name}]`）+ 1 个 Debug RPC（`POST /api/debug/prompts/reload`，沿 ADR-048 `DebugService` trait 的 late-bind slot）；Desktop DebugPanel 新增常驻头部"提示词列表"，点击走 `fileEditorStore.openFileWithContent` 在 FileEditor 打开，按 §3.7.2 标签分组（🟦 普通段 / ⚙️ 任务指令），始终显示不限 DevMode 状态。

> **关于 `build_compaction_system_prompt()`**：该函数在 `COMPACTION_SYSTEM_PROMPT`（已被 `summary.md` 覆盖，ADR-053）之后追加 identity 上下文；其追加块结构本身是协议边界（`<user_intent>` 语言规则的下游契约），不在本 ADR 覆盖范围。

---

## 2. 背景与动机

### 2.1 现状：8 个指令性 prompt 仍硬编码

按 [`docs/prompt-audit/zh/runtime-prompts-summary.md`](../../prompt-audit/zh/runtime-prompts-summary.md) 盘点，`core/acowork-runtime/src/prompt.rs` 集中了 5 个生产 prompt 常量 + `build_compaction_system_prompt` 拼接函数，`acowork-grafeo` 与 `acowork-memory` 各有 1–3 个独立 `const PROMPT`，合计 9 条（去重后 8 条独立内容）LLM 指令性 prompt 全部内置。

| 常量 | 典型调用点 | 为什么不通用 |
|---|---|---|
| `PROMPT_BUILDER_FALLBACK` | `prompt_builder.rs` 无 prompts/*.md 时 | 包作者想给"空包"留一个非默认身份（如"我是一个空包占位符"） |
| `SEARCH_SYSTEM_PROMPT` | perplexity.rs 后端 | 工程 agent 想要"返回结果附 commit hash"，客服 agent 想要"返回结果附 FAQ 链接" |
| `COMPACT_PROMPT` | `episode_distill.rs` | user prompt 包裹格式（`<conversation>`）对部分模型来说过于啰嗦，需要更简洁的指令 |
| `TITLE_PROMPT` | `compact_session_title_with_llm` | 不同 agent 的标题风格偏好（如"动词开头" vs "名词短语"） |
| `EXTRACTION_SYSTEM_PROMPT` | grafeo 三元组抽取 | 不同 agent 的知识模式（工程：函数/API；客服：用户诉求/处理结论） |
| `CONFLICT_CLASSIFICATION_PROMPT` | grafeo 冲突分类 | 领域歧义边界不同 |
| `GENERALIZATION_PROMPT` | grafeo 行为模式 | 抽象层次偏好不同 |
| `DEFAULT_ABSTENTION_PROMPT` | memory 弃权 | 语气 / 兜底措辞 / 是否承认"我不知道" |

这些指令的差异**完全取决于包作者对该 agent 类型的领域理解**，与"运行时配置"（如温度、context window）正交，理应归 `.agent` 包所有。

### 2.2 与 `system_prompt_override` 的边界

`system_prompt_override`（`agent_config.json` 字段）覆盖的是**主对话 system prompt**，是用户级的运行时调优入口。包级 `prompts/<file>.md` 覆盖的是**任务指令性 prompt**，是包作者声明的 agent 行为定义。两者维度正交：

| 维度 | 入口 | 归属 | 适用对象 |
|---|---|---|---|
| 主对话 system prompt | `prompts/system.md` + `prompts/*.md` + `system_prompt_override` | 包 / 用户 | 主对话身份 |
| 任务指令（compaction / search / title / extraction / ...） | `prompts/<special-name>.md` | 包 | 各类隐式 LLM 调用 |

`system_prompt_override` 不再被覆盖路径借用（ADR-053 §3.2 已修正）；本 ADR 延续该原则。

### 2.3 为什么现在做

- 项目未上线，无外部 `.agent` 包兼容负担。
- `summary.md` 已落地完整基础设施（`load_compaction_prompt` + 主 prompt 排除 + Phase A 收敛），横向扩展是同构成本的零增量。
- prompt-audit 文档已建立清晰的硬编码清单，扩展时不会"漏过"任何一条。

---

## 3. 设计

### 3.1 文件约定

```
examples/<agent-name>/
├── prompts/
│   ├── system.md                     # 主对话身份
│   ├── constraints.md                # 行为约束
│   ├── summary.md                    # 压缩/蒸馏指令（ADR-053）
│   ├── fallback.md                   # 新：无 prompts/*.md 时的兜底 system prompt
│   ├── search.md                     # 新：搜索系统提示
│   ├── compact-template.md           # 新：压缩 user prompt 模板
│   ├── title.md                      # 新：标题生成 prompt
│   ├── extraction.md                 # 新：三元组抽取
│   ├── conflict-classification.md    # 新：冲突分类
│   ├── generalization.md             # 新：行为模式抽取
│   └── abstention.md                 # 新：记忆弃权
├── skills/
└── manifest.toml
```

**命名规则**：

- 全部小写 + 短横线分隔（除 `compact-template` 这种复合词）。
- 主对话 system prompt 走 `prompts/*.md` 全量拼接（**保持原行为**，不引入新文件名）。
- 每个被覆盖的硬编码常量对应**唯一**文件名；命名冲突（如 `compact.md` 易与 `summary.md` 混淆）一律避开。
- 加载与排除使用**同一精确文件名匹配**判据（与 ADR-053 §3.3 一致），保证"被加载为 X 指令"与"被排除出主 prompt"指向同一文件。
- 这 9 个文件名同时构成 §3.7 Debug 面板列表的展示范围——普通段 `system.md` / `constraints.md` 作为分组代表，不在 `OVERRIDABLE_PROMPTS` 清单内但与列表有视觉对照关系。

### 3.2 加载与解析链

`prompt_builder.rs` 新增通用加载器与文件名清单：

```rust
/// 约定文件名清单：精确文件名 → 含义的映射
/// 加载与主 prompt 排除均基于此清单的"文件名"列。
pub const OVERRIDABLE_PROMPTS: &[(&str, &str)] = &[
    ("summary.md", "compaction/distillation system prompt (ADR-053)"),
    ("fallback.md", "PROMPT_BUILDER_FALLBACK"),
    ("search.md", "SEARCH_SYSTEM_PROMPT"),
    ("compact-template.md", "COMPACT_PROMPT"),
    ("title.md", "TITLE_PROMPT"),
    ("extraction.md", "EXTRACTION_SYSTEM_PROMPT (grafeo)"),
    ("conflict-classification.md", "CONFLICT_CLASSIFICATION_PROMPT (grafeo)"),
    ("generalization.md", "GENERALIZATION_PROMPT (grafeo)"),
    ("abstention.md", "DEFAULT_ABSTENTION_PROMPT (memory)"),
];

/// 通用加载器：精确文件名 → Option<String>
/// 缺失 / 纯空白 / 权限或编码错误的行为与 load_compaction_prompt 一致。
pub fn load_optional_prompt(package_dir: &Path, filename: &str) -> Option<String>;
```

`AgentBootContext`（`startup/context.rs`）新增对应字段（独立字段形态，与 ADR-053 严格同构）：

```rust
pub struct AgentBootContext {
    // ... 已有字段
    pub compaction_prompt: Option<String>,            // ADR-053
    pub fallback_prompt: Option<String>,              // 新
    pub search_prompt: Option<String>,                // 新
    pub compact_template: Option<String>,             // 新
    pub title_prompt: Option<String>,                 // 新
    pub extraction_prompt: Option<String>,            // 新
    pub conflict_classification_prompt: Option<String>,// 新
    pub generalization_prompt: Option<String>,        // 新
    pub abstention_prompt: Option<String>,            // 新
}
```

**`AgentCore`**（`agent/agent_core.rs`）新增对应字段（与 `compaction_prompt` 同模式），调用方解析链：

```rust
// 各 LLM 调用点
let prompt = core.title_prompt
    .as_deref()
    .unwrap_or(crate::prompt::TITLE_PROMPT);
```

### 3.3 主 prompt 排除

`build_system_prompt_with_mode` 把排除列表从仅 `summary.md` 扩展为 `OVERRIDABLE_PROMPTS.map(|(f, _)| f)`（精确文件名集合），任何被该集合命中的 `.md` / `.txt` 文件在主 system prompt 拼接时跳过。语义对称：

> "被加载为任务指令"与"被排除出主 prompt"始终指向同一文件；任何其他命名（如 `SUMMARY.md`、`title.txt`）一律按普通 prompt 段处理。

### 3.4 明确不覆盖的三类

**a. 运行时注入块（`context.rs` 7 个 §Section 模板）**

理由：
- 模板含**结构性占位符**（`{identity}` / `{memory}` / `{todos}` 等），位置与顺序影响 prompt cache 命中率（ADR-060 的 Block A 稳定前缀）。
- 模板边界被下游解析逻辑依赖（如 `<conversation>` 包裹、`## Environment` 段落被压缩规则识别）。
- "改指令内容"与"改骨架"必须分离：前者由本 ADR 接入包级覆盖，后者必须由平台统一控制。

**b. 工具 description（22 个 ToolSpec.description）**

理由：
- 22 个工具的 description 多为"功能说明"而非"任务指令"，与 ADR-053 同构度低。
- 部分工具的 description 嵌入了跨字段引用（如 file_read 强调"先 content_search 定位行号"），改写风险大于收益。
- 留作未来扩展点：若出现明确的"agent 自定义工具行为"需求，再单独 ADR（候选文件命名如 `tools/<tool_name>.md`，但需重新评估）。

**c. 协议格式（`episode_distill.rs::format_messages` 行模板、`output.rs` TRUNCATED marker）**

理由：LLM 反向解析这些标记作为压缩/截断的语义边界，灵活化会直接破坏协议。

### 3.5 保留不变

- 内置常量全部保留为兜底，行为零回归。
- `summary.md` 加载器（`load_compaction_prompt`）可被通用 `load_optional_prompt` 替换或保留为薄包装，二者选其一（实现期决定）。
- `build_compaction_system_prompt(base, identity_context)` 的拼接结构不变，仅 `base` 可来自包声明。
- 加载收敛点（Phase A → `AgentBootContext` → Phase B 注入）与 ADR-053 完全一致。

### 3.6 限制与预留

- **启动时静态加载**：与 system prompt / compaction prompt 一致，包升级 / 热更新后需重启 Runtime 生效。
- **grafeo / memory 的常量当前为进程级单例**（非 per-AgentCore），需评估是否升级为 `MemoryProvider` trait 的可注入参数（ADR-051 解耦基础上的延伸）；本次 ADR 仅做"加载器 + 注入点"，实现期评估是否需要 trait 改造。
- **`compact-template.md` 的特殊语义**：覆盖的是 user prompt 模板（含 `<conversation>` 占位符），包作者必须保留 `{messages_text}` 占位符，否则运行时拼装失败。文档中明确标注。

### 3.7 Debug 面板编辑入口（L1 文件读写 + L2 DevMode 重载）

包级覆盖机制落地后，包作者有了"声明意图"通道，但**调试与迭代**仍是空白——`.agent` 包作者修改 `prompts/summary.md` 后必须重启 Runtime 才能看到效果。本节定义一个最小化的 Debug 面板编辑入口，覆盖以下三个场景：

1. **包作者**：在 Debug 模式下快速对比不同 `summary.md` 措辞对压缩输出的影响。
2. **运维**：发现某次 compaction 输出走样时，直接打开对应 `prompts/*.md` 查看/微调。
3. **教学**：新人 onboarding 时可视化展示"哪些提示词被运行时引用、哪些被排除"。

#### 3.7.1 整体策略：L1 必做 + L2 建议 + L3 不做

| 层 | 范围 | 实施 | ADR 关系 |
|---|---|---|---|
| **L1 必做** | 文件读写（UI + REST） | 4 个新 HTTP 端点 + Debug 面板列表 + FileEditor 打开/保存；保存后提示"重启 Runtime 生效"或"重新进入 DevMode 生效" | 与 §3.6 "启动时静态加载"兼容 |
| **L2 建议做** | DevMode 启动时一次性重载 | `POST /api/debug/prompts/reload` 在 DevMode 启用后由 `DebugService` trait 触发；只 reload 一次，不做实时传播 | 复用 ADR-048 §4.1 已有的 `reloadSkills` 占位 RPC 模式 |
| **L3 不做** | 实时 lock-free 热加载（每次 LLM 调用读最新值） | 需要把 `AgentCore` 的 9 个 prompt 字段改为 `Arc<arc_swap::ArcSwap<...>>`，所有 prompt 调用点都要改写 | 与 §3.6 "启动时静态加载" + §5.3 "热更新否决"直接冲突 |

**为什么 L2 足够覆盖大多数"热加载"诉求**：Debug 模式下用户的典型工作流是"编辑 prompt → 退出 Debug → 重新进入 Debug 看效果"——这是 IDE 调试的本能反应，不是"保存后必须秒生效"。DevMode 启动时 reload 把这个工作流压缩到"保存 → 重新进 Debug"，已经够用；强行做 L3 实时热加载会让代码复杂度倍增，且与 ADR-053 / §3.6 的简洁设计冲突。

**L1 + L2 与 shell_risk 热加载的关系**：[`core/acowork-runtime/src/security/shell_risk.rs:236-241`](../../acowork-runtime/src/security/shell_risk.rs#L236-L241) 已实现 PUT 后调 `reload_from_disk()` + `Arc<RwLock>` 全局缓存替换的模式；本节借鉴其"PUT 写盘 → 触发 reload"的写法，但 prompt 是 per-`AgentCore`（非全局单例），所以 L2 选"DevMode 启动时 reload"而不是"PUT 时 reload"——避免正在运行的 session 持有旧 `Arc<AgentCore>` 与新 reload 结果分裂。

#### 3.7.2 UI 设计

**位置**：Debug 面板顶部（紧贴 tab 标签下方），作为 Debug tab 5 个状态分支（[ResultsPanel.tsx:281-292](../../acowork-desktop/src/components/results/ResultsPanel.tsx#L281-L292)）的**常驻头部**。在 state 1/2（agent 未运行 / DevMode 未启用）时自然占据"上方空白处"；在 state 5（已连接 DebugPanel）时作为 DebugPanel 顶部的可折叠区域。

**语义**：**始终显示**（不限 DevMode 状态），理由：
- 与 shell_risk 的"编辑风险规则"按钮一致——后者在 AgentSetupTab 是常驻 UI，不受任何状态门控。
- Debug 模式是**编辑 prompt 的主要场景**，若 DevMode 启动后列表消失，等于把"调试 prompt"从 Debug 面板抽走。
- 5 个状态的渲染区域已有大块留白（图标 + 单行文字 + 单按钮），塞入一个轻量列表不破坏布局。

**列表内容**：`OVERRIDABLE_PROMPTS` 9 个文件名按 §3.2 顺序排列，每行带分组标签：

| UI 标签 | 含义 | 视觉示例 |
|---|---|---|
| 🟦 普通段 | 进入主 system prompt（如 `system.md` / `constraints.md`） | 列在分组 "Main dialog" 下 |
| ⚙️ 任务指令 | 运行时按需引用，覆盖 `prompt.rs` 内置常量 | 列在分组 "Task directives" 下 |

**文件范围**：仅展示 `OVERRIDABLE_PROMPTS` 中的 9 个文件名 + `system.md` / `constraints.md`（作为普通段代表）。`prompts/` 目录下的其他随机 `.md`（如 `examples/notes.md`）**不展示**——保持聚焦"被运行时引用的提示词"。

**点击行为**：调用 `fileEditorStore.openFileWithContent(agentId, "__agent_home__", "prompts/<name>.md", fetchedContent, "markdown")` 在 FileEditor 打开（参考 [AgentSetupTab.tsx:907-933](../../acowork-desktop/src/components/results/AgentSetupTab.tsx#L907-L933) 的 shell_risk 打开模式）。

**保存后行为**：

| 场景 | 行为 |
|---|---|
| DevMode 未启用 | Toast: "Saved. Restart Runtime to apply."（与 §3.6 静态加载一致） |
| DevMode 已启用 | Toast: "Saved. Re-enter DevMode to apply."（L2 在 DevMode 启动时 reload 一次） |
| 外部触发 reload（DevMode 重入 / Runtime 重启） | MQTT `debug/events/onStateChange` 事件携带 "prompts reloaded" 信号，Desktop 自动 refresh 列表内容（复用 ADR-058 的 `diskConflict` 跟踪机制） |

#### 3.7.3 HTTP 端点

新增 4 个 Runtime HTTP 端点 + 1 个 Debug RPC，镜像 shell_risk 的路由模式（[server.rs:631-634](../../acowork-runtime/src/http/server.rs#L631-L634)）：

| 方法 + 路径 | Runtime 端点 | Gateway 反代 | 用途 |
|---|---|---|---|
| `GET /api/agents/{id}/prompts` | `GET /agents/{id}/prompts` | `/api/agents/{id}/prompts` | 列出该 agent `prompts/` 目录下 `OVERRIDABLE_PROMPTS` 交集的文件清单（含每文件 size + mtime，给 ADR-058 冲突检测用） |
| `GET /api/agents/{id}/prompts/{name}` | `GET /agents/{id}/prompts/{name}` | 同路径 | 读取单个文件内容（UTF-8，缺文件返回 404） |
| `PUT /api/agents/{id}/prompts/{name}` | `PUT /agents/{id}/prompts/{name}` | 同路径 | 写入文件；name 必须 ∈ `OVERRIDABLE_PROMPTS`（防止任意文件写入）；空内容写入视为删除（保留文件但内容清空，行为与 ADR-053 §`load_compaction_prompt` "纯空白 → None" 一致） |
| `POST /api/agents/{id}/debug/prompts/reload` | `POST /agents/{id}/debug/prompts/reload` | 同路径 | L2 触发点；DevMode 启用后由 `DebugService` trait 调用；执行"重新加载 `OVERRIDABLE_PROMPTS` → 替换 `AgentBootContext` 对应字段 → 通过已 clone 的 `Arc<AgentCore>` 写入新值（详见 §3.7.5 写入策略）" |

**响应格式**（与 shell_risk 的 GET 响应风格一致）：

```jsonc
// GET /api/agents/{id}/prompts
{
  "agent_id": "com.acowork.senior-engineer",
  "prompts": [
    { "name": "summary.md",          "size": 1234, "modified": "2026-09-20T10:30:00Z", "kind": "task" },
    { "name": "system.md",           "size": 567,  "modified": "2026-09-19T08:00:00Z", "kind": "main" },
    { "name": "constraints.md",      "size": 234,  "modified": "2026-09-19T08:00:00Z", "kind": "main" }
  ]
}
```

**写入路由**：PUT 直接走文件系统（`fs::write(package_dir.join("prompts").join(name), content)`），**不**走 Workspace file API（`/workspaces/file`）——后者只能访问 `work_dir/`，而 `prompts/` 位于包安装目录。

#### 3.7.4 Debug RPC 接入

复用 [ADR-048 §4.2](../../adr/zh/ADR-048-debug-protocol-mqtt-http.md) `DebugService` trait 的 late-bind slot 模式，新增一个方法：

```rust
// core/acowork-runtime/src/usecases/debug_service.rs（追加）
#[async_trait]
pub trait DebugService: Send + Sync {
    // ... 已有 10 个方法（ADR-048）

    /// L2: DevMode 启动后触发一次 prompt 重载。
    /// 重新读取 prompts/<OVERRIDABLE_PROMPTS> → 替换 AgentBootContext 字段 →
    /// 写入已 clone 的 Arc<AgentCore>（详见 §3.7.5 写入策略）。
    /// 失败不阻塞 DevMode 启动；返回 Ok(ReloadReport) 让调用方 toast 报告。
    async fn reload_prompts(&self) -> Result<ReloadReport, DebugError>;
}

pub struct ReloadReport {
    pub loaded: Vec<String>,    // 成功重载的文件名
    pub missing: Vec<String>,   // 包目录中不存在的文件（保留旧值或 None）
    pub failed: Vec<(String, String)>, // (文件名, 错误信息)
}
```

**调用时机**：[`core/acowork-runtime/src/startup/subsystems.rs`](../../acowork-runtime/src/startup/subsystems.rs) Phase C 的 `enable_debug_mode()` 末尾追加 `debug_service.reload_prompts().await`，结果写入 MQTT `debug/events/onStateChange` payload 供 Desktop toast。

**Desktop 侧**：通过 ADR-048 已有的 `debug_rpc` 通用命令调用 `reload_prompts`，无需新增 transport 代码——D7 "文档同步" 已声明"未来补齐走 ADR-053，传输接线零改动"。

#### 3.7.5 写入策略：避免 Arc<AgentCore> 持有旧值

**问题**：`AgentCore` 已被 clone 成多个 `Arc<AgentCore>` 分散到各 session / AgentLoop。简单 `Arc::get_mut(&mut arc)` 只在引用计数为 1 时成功——DevMode 启动时所有 session 已持有克隆，**get_mut 必然失败**。

**解决**：在 `AgentCore` 内 9 个 prompt 字段外包一层 `Arc<std::sync::RwLock<Option<String>>>`：

```rust
pub struct AgentCore {
    // ... 已有字段

    /// §3.7.5: 用 RwLock 包装使 L2 reload 可写穿。
    /// 与 §3.2 的独立 Option<String> 字段语义不变，区别只是多了
    /// reload 写入通道；读取点一律 .read().unwrap().as_deref()。
    pub(crate) compaction_prompt: Arc<std::sync::RwLock<Option<String>>>,
    pub(crate) fallback_prompt: Arc<std::sync::RwLock<Option<String>>>,
    // ... 其余 7 个
}
```

**Clone 语义**：`Clone for AgentCore` 共享 `Arc`（引用 +1），不是深拷贝——与原 `Option<String>` 字段的 Clone（深拷贝 String）**行为有差**，需在 doc-comment 明确标注：

> `AgentCore::clone()` 对这 9 个字段共享 `Arc<RwLock<...>>`（引用 +1）；其余字段仍按值 Clone。L2 reload 写入一个 clone 即可被所有克隆看到。

**读取点改造**：把 §3.2 的 `core.title_prompt.as_deref().unwrap_or(...)` 改为 `core.title_prompt.read().unwrap().as_deref().unwrap_or(...)`——lock guard 持有仅在 `.read()` 调用期间，开销可忽略（与原 `Option<String>` 的 `as_deref()` 是同一量级）。

**写入点**（L2 reload）：

```rust
async fn reload_prompts(&self) -> Result<ReloadReport, DebugError> {
    let ctx = self.boot_context.lock().await;
    let new = load_all_overridable_prompts(&ctx.package_dir);
    *ctx.compaction_prompt.write().unwrap() = new.summary.clone();
    *ctx.fallback_prompt.write().unwrap() = new.fallback.clone();
    // ... 其余 7 个
    Ok(new.into_report())
}
```

**锁粒度**：每个字段独立 `RwLock`，不互锁——L2 reload 并发与 LLM 调用读取可完全并行；只有 reload 写入与 LLM 读取对**同一个**字段互斥。

**性能**：单次 LLM 调用的 prompt 解析链增加 9 次 `RwLock::read().try_lock()` 调用（实测 `RwLock::read()` 在无写竞争时 ≈ 25ns；与原 `Option<String>.as_deref()` ≈ 5ns 差距在 LLM 调用的毫秒级延迟中可忽略）。如需进一步优化，可在 `AgentCore` 内缓存 `Option<&'static str>` + OnceCell 初始化，避免每次 LLM 调用读 lock——但 LLM 调用频次远低于 LLM 自身耗时，缓存收益微小，**本期不优化**。

#### 3.7.6 安全边界

- **写入路径校验**：PUT handler 强制 `name` ∈ `OVERRIDABLE_PROMPTS`，且不含 `/` `\` `..`（防御性，与 shell_risk 写入 handler 一致）。
- **Debug RPC 鉴权**：L2 端点 `/api/agents/{id}/debug/prompts/reload` 必须经 DevMode 启用——Runtime 启动时该路径不存在；`enable_debug_mode` 注册后才生效。沿用 ADR-048 §4.6 的 late-bind slot 模式。
- **Gateway 反代**：5 个端点全部走 Gateway 反代 → Runtime localhost HTTP（[core/acowork-gateway/src/http/proxy.rs](../../acowork-gateway/src/http/proxy.rs)），与 shell_risk 的反代规则同模式，新增 ~10 行。
- **Grafeo / memory 覆盖的提示**：grafeo / memory 4 个常量当前为进程级单例（§3.6 已标注），L2 reload 对它们的生效路径不同——`reload_prompts` 内对 grafeo / memory 的 reload 需评估 trait 改造（沿 ADR-051 解耦路径）。**本期不实现** grafeo / memory 的 L2 reload；grafeo / memory 文件 PUT 后仍提示"重启 Runtime"。

---

## 4. 影响

### 4.1 代码改动清单

**核心加载 + AgentCore 字段（§3.1-§3.6）**：

| 文件 | 改动 |
|---|---|
| `core/acowork-runtime/src/package/prompt_builder.rs` | 新增 `OVERRIDABLE_PROMPTS` 常量 + `load_optional_prompt` 通用加载器；主 prompt 排除列表扩展为完整精确文件名集合；新增 N 个单元测试 |
| `core/acowork-runtime/src/prompt.rs` | 顶部 `//!` 注释新增"包级覆盖文件名约定"段落，列出所有可覆盖常量与对应文件名 |
| `core/acowork-runtime/src/agent/agent_core.rs` | **CHANGE**：原 §3.2 的 9 个 `Option<String>` 字段改为 `Arc<std::sync::RwLock<Option<String>>>`（详见 §3.7.5）；`Clone` impl 改为共享 `Arc`（引用 +1）；9 处读取点改为 `.read().unwrap().as_deref().unwrap_or(const)` |
| `core/acowork-runtime/src/startup/context.rs` | `AgentBootContext` 新增对应 9 个 `Option<String>` 字段（**保持原类型**——Phase A 一次性加载，不需要 lock） |
| `core/acowork-runtime/src/startup/agent_init.rs` | Phase A 加载所有可覆盖 prompt，结果存入 `AgentBootContext` |
| `core/acowork-runtime/src/startup/session_init.rs` | Phase B 从 ctx 注入到 `AgentCore`；注入时用 `Arc::new(RwLock::new(ctx.field.clone()))` 包装 |
| `core/acowork-runtime/src/cli.rs` | Standalone 分支从 ctx 注入（消除模式分裂） |
| 各 LLM 调用点（`episode_distill.rs` / perplexity.rs / grafeo / memory） | 解析链改为 `core.<field>.read().unwrap().as_deref().unwrap_or(const)` |

**Debug 面板编辑入口（§3.7）**：

| 文件 | 改动 |
|---|---|
| `core/acowork-runtime/src/http/prompts.rs`（**新增**） | 4 个 axum handler：`list_prompts` / `get_prompt` / `put_prompt` / `reload_prompts`；PUT 路径校验强制 `name ∈ OVERRIDABLE_PROMPTS` 且不含 `/`/`\`/`..` |
| `core/acowork-runtime/src/http/server.rs` | 新增路由：`GET /agents/{id}/prompts` / `GET /agents/{id}/prompts/{name}` / `PUT /agents/{id}/prompts/{name}` / `POST /agents/{id}/debug/prompts/reload`；最后一个走 ADR-048 `DebugService` late-bind slot |
| `core/acowork-runtime/src/usecases/debug_service.rs` | `DebugService` trait 新增 `async fn reload_prompts(&self) -> Result<ReloadReport, DebugError>` 方法 + `ReloadReport` DTO |
| `core/acowork-runtime/src/usecases/debug_service_impl.rs` | `RuntimeDebugService::reload_prompts` 实现：调 `load_optional_prompt` 重读 9 个文件 → 通过 `Arc::get_mut` 失败回退到 `boot_context` 字段的 `RwLock` 写入 → 构造 `ReloadReport` |
| `core/acowork-runtime/src/startup/subsystems.rs` | Phase C `enable_debug_mode()` 末尾追加 `debug_service.reload_prompts().await`，结果通过 MQTT `debug/events/onStateChange` payload 推送 |
| `core/acowork-gateway/src/http/proxy.rs` | 新增 5 条反代规则：`/api/agents/{id}/prompts`（GET）+ `/api/agents/{id}/prompts/{name}`（GET+PUT）+ `/api/agents/{id}/debug/prompts/reload`（POST），转发到 Runtime localhost HTTP |
| `apps/acowork-desktop/src/components/debug/PromptList.tsx`（**新增**） | 列表组件，两组布局（🟦 普通段 / ⚙️ 任务指令），点击调 `fileEditorStore.openFileWithContent` |
| `apps/acowork-desktop/src/components/debug/DebugPanel.tsx` | 在 DebugPanel 顶部接入 `<PromptList agentId={agentId} />`；始终渲染，不受 5 状态门控 |
| `apps/acowork-desktop/src/components/results/ResultsPanel.tsx` | Debug tab state 1/2 分支（agent 未运行 / DevMode 未启用）渲染 `<PromptList />` 作为主内容填充"上方空白处" |
| `apps/acowork-desktop/src/stores/debugStore.ts` 或 `commands/debug.rs` | 新增 `reloadPrompts()` 客户端方法，走 ADR-048 `debug_rpc` 通用命令 |
| `apps/acowork-desktop/src/i18n/locales/{zh-CN,en,...}.json` | 新增 i18n keys：`debug.promptList.title` / `debug.promptList.mainGroup` / `debug.promptList.taskGroup` / `debug.promptList.savedRestartHint` / `debug.promptList.savedReenterHint` |

**文档 + 协议**：

| 文件 | 改动 |
|---|---|
| `examples/*/prompts/*.md` | 示范文件：按 agent 类型补充对应覆盖文件（`fallback.md` / `search.md` / `title.md` / `extraction.md` / `abstention.md` 等） |
| `docs/prompt-audit/zh/runtime-prompts-summary.md` | 在 §1 / §3 表格中新增"支持覆盖文件名"列；新增"明确不覆盖"章节引用本 ADR；§1 顶部注释新增"Debug 编辑入口"链接到 §3.7 |
| `docs/protocols/zh/http.md` | 4 个新 HTTP 端点 + 1 个 Debug RPC 路由的 API 参考 |
| `docs/design/zh/03-agent-runtime.md` §7.2 | 同步说明 prompt 重载触点（DevMode 启动时一次） |
| `docs/adr/zh/ADR-048-debug-protocol-mqtt-http.md` | §4.1 / §4.2 标注 `reload_prompts` 是 ADR-063 §3.7 新增的 D8 占位 RPC 实现 |

### 4.2 行为变化

| 场景 | 之前 | 之后 |
|---|---|---|
| 包无 `prompts/<file>.md` | 用内置默认 | 用内置默认（不变） |
| 包有 `prompts/<file>.md` | （无此概念） | 该任务 LLM 调用使用包声明指令 |
| 包有 `prompts/<file>.md` 但文件出现在主 system prompt 拼接 | — | 自动按精确文件名跳过（与 summary.md 对称） |
| 用户设置 `system_prompt_override` | 仅影响主对话 | 不变（与本 ADR 正交） |
| 用户在 Debug 面板编辑 `prompts/<file>.md` 并保存（DevMode 未启用） | — | 文件落盘，toast 提示"重启 Runtime 生效" |
| 用户在 Debug 面板编辑 `prompts/<file>.md` 并保存（DevMode 已启用） | — | 文件落盘，toast 提示"重新进入 DevMode 生效"；re-enter 时自动 reload（§3.7.4） |

### 4.3 验证

- `cargo build -p acowork-runtime`：通过。
- `cargo test -p acowork-runtime --lib`：覆盖 8 个新加载器 + 主 prompt 排除扩展的全部分支。
- `cargo test -p acowork-grafeo --lib` / `cargo test -p acowork-memory --lib`：覆盖注入点改造后的回退路径。
- `cargo clippy --all-targets -- -D warnings`：零警告。
- 集成测试：`mqtt_e2e_full` / `conversation_session_tokens` / `builtin_tools_mutation` 全绿。
- 文档同步：`prompt-audit` 表格更新 + ADR 链接生效。

---

## 5. 备选方案

### 5.1 单一"覆盖层"配置文件（如 `prompts/overrides.json`）（否决）

把所有可覆盖常量集中到一个 JSON 文件：键为常量名，值为字符串。看起来比 8 个 `.md` 文件更紧凑，但：
- 失去 Markdown 的代码块/列表/标题语法，编辑体验下降；
- 与 `summary.md` 不对称，破坏"所有 prompt 都用 .md"的组织原则；
- JSON 字段名是结构化标识，对包作者不友好。

### 5.2 运行时配置入口（`agent_config.json` 增加字段）（否决）

8 个新字段会让 `agent_config.json` 膨胀，且混淆"包声明"与"用户调优"两个维度。运行时配置改写后会让所有使用该 `.agent` 包的实例行为分裂（与 `system_prompt_override` 已暴露的语义混乱同源）。包级声明是 single source of truth。

### 5.3 运行时热更新（watch prompts/ 目录，重载 prompt 内容）（**部分否决**——仅 L3 否决，L1 + L2 见 §3.7）

本 ADR 落地后 §3.7 已采纳 **L1 文件读写 + L2 DevMode 启动时重载**——前者覆盖"打开/编辑/保存 UI"，后者覆盖"保存后何时生效"。**否决的是 L3（实时 lock-free 热加载）**，理由：
- 与 ADR-053 的"启动时静态加载"原则冲突；
- `AgentCore` 已被 clone 成多个 `Arc<AgentCore>` 分散到各 session / AgentLoop，做 lock-free 需要 `Arc<arc_swap::ArcSwap<...>>`，9 个 prompt 字段的每个调用点都要改写；
- 包升级的语义应统一在 Runtime 重启，不应在 prompt 层分裂。

详见 §3.7.1 三层策略表 + §3.7.5 `Arc<RwLock<...>>` 写入策略。

### 5.4 覆盖范围扩到 runtime 注入块 / 工具 description（否决）

§3.4 已详述理由。核心：注入块是结构骨架（cache 锚点 + 协议边界），工具 description 是工具元数据，二者与"任务指令性 prompt"的同构度低，强行扩展会引入远超收益的复杂性。留作未来按需扩展。

### 5.5 加载器改用统一 HashMap（`<文件名, 内容>`）（否决）

`AgentCore.extra_prompts: HashMap<&'static str, String>` 形态在"扩展新覆盖项时 AgentCore / AgentBootContext 零改动"上确有优势，但失去类型安全（typo 的 key 编译期不可见），且与 ADR-053 严格同构的承诺相违。独立字段的代码重复可接受（9 个 `Option<String>` + 一一对应的 `unwrap_or` 解析链），换来读代码时一眼可辨每个 agent 持有哪些 prompt 覆盖。

### 5.6 实时热加载（`Arc<arc_swap::ArcSwap<...>>` 每次 LLM 调用读最新值）（否决）

§3.7.5 末尾讨论的"性能优化选项"——把所有 prompt 字段改为 `ArcSwap` + `OnceCell` 缓存，避免每次 LLM 调用读 lock。**否决**：LLM 调用的毫秒级延迟远超 lock 开销（25ns vs 5ns），缓存收益与代码复杂度不匹配。如未来 LLM 调用延迟降到微秒级（如本地小模型推理），可单独 ADR 评估重审。

---

## 6. 后续跟进

- **grafeo / memory 注入点评估**：本次实现需评估是否需要把 4 个常量的使用方改为 trait 注入（沿 ADR-051 的 MemoryProvider 解耦路径），避免进程级单例被多个 AgentCore 共享时覆盖失效。
- **工具 description 覆盖**：观察未来 .agent 包作者需求，若高频出现"agent 自定义工具行为"诉求，单独 ADR 评估 `prompts/tools/<tool_name>.md` 命名方案。
- **prompt-audit 文档维护**：本 ADR 落地后该文档新增"支持覆盖"列；任何新增硬编码 prompt 必须在 prompt.rs 注释中标注对应文件名（或明确标注"不支持覆盖 + 理由"）。
- **Debug 面板编辑入口实施路径（§3.7）**：建议分两阶段落地——
  1. **阶段 A（必做）**：§3.7.3 的 4 个 HTTP 端点 + §3.7.2 的 Desktop `PromptList` 组件 + `AgentCore` 字段类型改为 `Arc<RwLock<...>>`（§3.7.5）；无 L2 重载能力，保存后提示"重启 Runtime"或"重新进入 DevMode"。
  2. **阶段 B（建议做）**：`DebugService::reload_prompts` 实现 + `enable_debug_mode` 末尾调用（§3.7.4）；grafeo / memory 4 个常量的 L2 重载**不在本期范围**——它们的注入点需要 trait 改造（§6.1 跟进项），先保持"重启 Runtime 生效"。
- **`AgentCore` RwLock 改造的回归测试**：所有 9 个 prompt 字段的读取点（`episode_distill.rs` / perplexity.rs / grafeo / memory）改为 `.read().unwrap()` 后，需补 9 个并发读取 + 1 个写入的单元测试，确保锁粒度正确、不引发死锁。
- **MQTT `debug/events/onStateChange` payload 扩展**：当前 ADR-048 §4.1 列出的 5 个 payload 字段需追加 `prompts_reloaded: Option<ReloadReport>`，仅在 `reload_prompts()` 调用后填充；Desktop 侧 toast 与列表刷新逻辑依赖此字段。
