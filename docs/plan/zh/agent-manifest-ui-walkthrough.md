# Agent manifest 改造 —— UI 走查稿

> 配套方案：[agent-manifest-declaration-plan.md](agent-manifest-declaration-plan.md)（§7 推荐 MCP 交互、§8 Create Agent 5 步）
> 本文只描述**屏幕、状态、文案、交互**，不含实现细节。文案以 zh-CN 为准，5 个 locale 同步。

---

## 0. 复用与新增

| 用途 | 复用 | 新增 |
|---|---|---|
| 折叠分组 / 行 | `ListBox`、`ExpandableRow`、`ListRow`、`EmptyState`（`components/common/list`） | — |
| 开关 | `Switch` | — |
| 输入 | `StyledInput`、`StyledTextarea` | — |
| 状态胶囊 | `Badge` / capsule 语法 | — |
| 卡片角标 | `sidebarCountBadge` 语法 | — |
| **标签输入** | — | `components/common/TagInput.tsx`（含 `TagInput.test.tsx`） |
| **多选预设** | `ListRow` + `Switch` | 向导内的 `McpPickList`（纯展示） |

不引入新的视觉语言：所有卡片、分隔线、字号（text-10/11/12）沿用现有面板。

---

## 屏 A：右侧面板 → 工具（推荐 MCP 分组）

### A.0 位置

```
工具                                        ← 面板标题
┌─────────────────────────────────────────┐
│ ▸ Agent 包建议的 MCP (2)                 │ ← 新增分组，置顶
├─────────────────────────────────────────┤
│ ▸ 内置工具 (21)                          │
├─────────────────────────────────────────┤
│ ▾ MCP 服务器 (3)                         │ ← 现有
└─────────────────────────────────────────┘
```

置顶理由：它需要用户先做出决定（装 / 不装），决策完就沉下去，不再占用注意力。

`recommended.length === 0` → **整组不渲染**（不写"暂无建议"的空文案，避免占位噪音）。

### A.1 行结构

```
▸ [●] brave-search                        [安装并启用]
      联网检索最新资料 · 需要 BRAVE_API_KEY · 3 个工具
```

- `[●]`：`required = true` 时的实心圆点（必需），`required = false` 时省略。
- 标题：catalog 的 `display_name` ?? `recommended.name`。
- 第二行小字（text-10，tertiary）：`why`（作者写的理由）· 密钥需求 · 工具数；缺失项自动省略，用 `·` 连接。
- 右侧：状态动作（见 A.2）。

展开体（点击行标题）：

```
为什么：联网检索最新资料
将启用的工具：brave_web_search、brave_news_search
（工具名来自 Agent 包声明；启用后可在本面板逐个调整）
                          [忽略此建议]
```

### A.2 状态 × 文案 × 动作

| state | 第二行 | 右侧控件 | 点击后 |
|---|---|---|---|
| `unavailable` | `why · 需要 BRAVE_API_KEY · 3 个工具` | 按钮「安装并启用」 | 走预设安装管线 → 成功后自动激活 → 行变 `active` |
| `installing` | 同上 | 按钮 spinner「安装中…」，同组其它行禁用 | — |
| `install_failed` | 红色小字「安装失败：npm 不可用」 | 按钮「重试」 | 重跑安装 |
| `secrets_missing` | 「缺少密钥：BRAVE_API_KEY」 | 按钮「填写密钥」 | 打开密钥表单（复用 add-server 的 secret 表单），保存后自动继续安装 |
| `installed_inactive` | 加前缀「已安装，未启用」 | 按钮「启用」 | `PUT /mcp-servers` 追加 → 行变 `active` |
| `enabling` | — | spinner「启用中…」 | — |
| `active` | 加前缀「已启用」 | 胶囊「已启用」 | 无（可展开看工具） |
| `active` + `missing_tools` | 黄色「该 MCP 未提供：brave_web_search」 | 同上 | — |
| `dismissed` | 收进「已忽略 (1)」折叠行 | 展开后每行「恢复」 | 恢复为该行原状态 |

`hosted`（`pm` / `doc` / `memory`）：catalog 由平台自动注入，所以只会出现 `installed_inactive` / `active`，第二行加后缀「· 由平台托管」。

### A.3 已忽略折叠

```
▸ 已忽略 (1)
    corp-kb                                     [恢复]
```

默认折叠。忽略是一次写入（`PUT /mcp-servers` 带 `dismissed_recommendations`），不是本地隐藏 —— 换机器也一致。

### A.4 顶部横幅（只在有 `required` 未满足时出现）

```
缺少必需 MCP：brave-search、corp-kb
```

- 位置：工具面板最上方，`Badge` 警告样式；不遮挡分组。
- **不阻断**聊天与启动。

### A.5 骨架 / 加载

面板首屏 `recommended` 尚未返回时，分组标题显示为「Agent 包建议的 MCP …」，行不渲染（不用骨架屏——返回通常在 100ms 内，闪骨架更吵）。

### A.6 安装去向提示（必须写进 UI）

从本面板安装的服务器进入**本机共享的 MCP 目录**，因此：

- 安装成功后，该行右侧出现一行 text-9 提示：「已加入本机 MCP 目录；其他 Agent 也能看到，但只在本 Agent 激活。」
- 分组说明（标题下方一行）：「由 Agent 包声明、需要你确认的 MCP 服务器。安装后进入本机 MCP 目录。」

不写这条提示，用户会在别的 Agent 工具面板里发现"凭空多出来的服务器"。

---

## 屏 B：Agent 列表卡片

```
┌──────────────────────────┐
│ 🤖  PM              ●①   │  ← 右上角角标：1 个必需 MCP 未满足
│     Project Manager      │     hover tooltip：「缺少 1 个必需 MCP」
└──────────────────────────┘
```

- 条件：该实例存在 `required = true` 且 state ∉ {`active`, `dismissed`}。
- `dismissed` 不算未满足（用户已明确拒绝，不再提醒）。
- 全部满足 → 不显示任何角标（不要"健康"噪音）。

---

## 屏 C：创建成功后的落点

`onCreated` 回调里：右侧面板切到「工具」标签；若该 Agent 的 `recommended` 非空，展开该分组并滚动到可见区。否则保持默认标签（对话）。

一次性动作，不写 localStorage —— 用户自己换了标签页就不再干预。

---

## 屏 D：Create Agent 向导（5 步）

### D.0 步骤条

```
① 基础   ② 模型与参数   ③ 工具与 MCP   ④ 技能与能力   ⑤ 预览
```

图标沿用现有：`Bot` / `SlidersHorizontal` / `Wrench` / `PackagePlus` / `Check`。

### D.1 步骤一：基础

在现有字段上补 3 个：

```
Agent ID *            com.example.myagent
显示名 *              我的 Agent
角色                  产品经理                 ← 可选
版本 / 作者           0.1.0 / 你的名字
描述                  ...
标签                                 3/12      ← 新增
  [ project-management × ] [ agile × ] [_______]
  回车或逗号分隔；小写字母、数字、连字符；最多 12 个
```

标签交互：
- 输入框内 Enter 或 `,` 提交一个 chip；chip 右侧 `×` 删除；Backspace 在空输入时删最后一个。
- 非法输入（大写 / 下划线 / 超长 / 重复 / 超 12 个）：输入框红边 + 下方红字「标签只能包含小写字母、数字和连字符（2–32 字符）」，Enter 不提交。
- 计数在 label 右侧 `3/12`。

`下一步` 可用条件：`agent_id` 反向域名格式、`name` 非空、所有 chip 合法。

### D.2 步骤二：模型与参数

```
以下参数会作为该 Agent 的出厂默认值写入 manifest；
留空则使用平台默认值，用户之后可在 Agent 设置里覆盖。

温度             [────●────]  0.5                        （空 = 平台默认 0.3）
最大输出 Token   [         ]  默认 200000（模型上限）
上下文窗口       [         ]  默认 200000
最大迭代次数     [         ]  默认 500
压缩比例阈值     [         ]  默认 90%
```

- 每一行右侧显示平台兜底值（灰字），让"空"有明确含义。
- 温度用滑杆（与 Agent 设置面板一致），其余为数字输入。
- 每个输入下方一行 text-9 说明（复用 `agentSetup.*Desc` 文案）。
- 允许全部留空 → 预览里不出现 `[config]` 段。

`下一步` 可用条件：所有输入在范围内（空合法）。

### D.3 步骤三：工具与 MCP

两个分区，同一对话框内滚动：

```
内置工具
  ▸ bash                    [on]
  ▸ file_edit               [off]
  ...
────────────
推荐 MCP
  声明该 Agent 建议使用的 MCP 服务器。安装后会出现在 Agent 的工具面板，
  由用户确认安装与启用。
  平台托管
    [ ] pm        任务与项目管理数据
    [ ] doc       在线文档库
    [ ] memory    长期记忆
  预设
    [ ] Brave Search    隐私优先的网页检索 · 需要 BRAVE_API_KEY
    [ ] Context7        文档检索
    ...
  ── 已选（2）──
    pm
      说明（展示给用户）  [任务与项目管理数据                    ]
      必需                [off]
      （具体工具在启用后于工具面板勾选）
```

- 平台托管的三个固定出现（`hosted`），预设列表来自 `GET /api/mcp-presets`。
- 选中项展开两个字段：`why`（可选，占位如上）、`必需` 开关（**默认关**）。
- 工具子集在本步**不收集**（服务器未连接，拿不到 `tools/list`），文案明示。

`下一步`：总是可用（都可选）。

### D.4 步骤四：技能与能力

```
技能
  [ 点击或拖拽 .zip 文件（需包含 SKILL.md） ]
  requirements-analysis   Collect, clarify, prioritize…   [×]
    依赖工具：file_read · mcp_memory__memory_store
  risk-assessment         …                                [×]
    依赖工具：brave_web_search      ⚠ 未在工具/MCP 中声明
────────────
对外能力（capability）
  声明其他 Agent 可以通过 intent 寻址的动作。
  与技能不同：技能是内部流程，能力是对外契约。
                                [ + 添加动作 ]
  requirements_analysis
    描述     Analyze and document project requirements into a …
    关联技能 [ requirements-analysis × ]
```

- 技能行新增一行小字「依赖工具：…」；依赖未被第 3 步覆盖时追加黄色「未在工具/MCP 中声明（不阻断）」。
- capability 行三个字段：动作名（`[a-z0-9_]` 校验）、描述、关联技能（多选：仅本步已上传的技能）。
- 空态：「未声明对外动作。仅在希望其他 Agent 调用它时才需要。」
- 高级折叠「输入/输出 schema（JSON）」→ 两个 textarea，内容必须是合法 JSON。

`下一步` 可用条件：技能全部解析成功、动作名合法且唯一、引用的技能存在、JSON（若填）合法。

### D.5 步骤五：预览

```
manifest.toml                              [复制]
┌──────────────────────────────────────────┐
│ agent_id = "com.example.myagent"         │
│ labels = ["project-management", "agile"] │
│ ...                                      │
│ [config]                                 │
│ temperature = 0.5                        │
│ [[mcp_servers]]                          │
│ hosted = "pm"                            │
└──────────────────────────────────────────┘

⚠ 3 条提示（可继续）
  · 技能 risk-assessment 依赖 brave_web_search，但未声明对应 MCP

[ 创建 Agent ]
```

- TOML 由后端 `preview_manifest` 返回（与创建用同一构造函数），前端不再手拼。
- 校验结果分两级：**错误**（红，禁用「创建 Agent」）；**提示**（黄，可继续）。

---

## 5. 文案清单（zh-CN）

| key | 文案 |
|---|---|
| `agentSetup.recommendedMcp` | Agent 包建议的 MCP（{{count}}） |
| `agentSetup.recommendedMcpHint` | 由 Agent 包声明、需要你确认的 MCP 服务器。安装后进入本机 MCP 目录 |
| `agentSetup.recommendedInstalledShared` | 已加入本机 MCP 目录；其他 Agent 也能看到，但只在本 Agent 激活 |
| `agentSetup.recommendedInstallAndEnable` | 安装并启用 |
| `agentSetup.recommendedInstalling` | 安装中… |
| `agentSetup.recommendedRetry` | 重试 |
| `agentSetup.recommendedFillSecrets` | 填写密钥 |
| `agentSetup.recommendedEnable` | 启用 |
| `agentSetup.recommendedEnabling` | 启用中… |
| `agentSetup.recommendedActive` | 已启用 |
| `agentSetup.recommendedInstalledInactive` | 已安装，未启用 |
| `agentSetup.recommendedHosted` | 由平台托管 |
| `agentSetup.recommendedWhy` | 为什么：{{why}} |
| `agentSetup.recommendedTools` | 将启用的工具：{{tools}} |
| `agentSetup.recommendedNoToolSubset` | 未声明工具子集，启用后可在本面板勾选 |
| `agentSetup.recommendedMissingTools` | 该 MCP 未提供：{{tools}} |
| `agentSetup.recommendedDismiss` | 忽略此建议 |
| `agentSetup.recommendedDismissed` | 已忽略（{{count}}） |
| `agentSetup.recommendedRestore` | 恢复 |
| `agentSetup.recommendedMissingBanner` | 缺少必需 MCP：{{names}} |
| `agentSetup.recommendedInstallFailed` | 安装失败：{{error}} |
| `createWizard.stepConfig` | 模型与参数 |
| `createWizard.stepMcp` | 工具与 MCP |
| `createWizard.stepCapability` | 技能与能力 |
| `createWizard.labelsLabel` | 标签 |
| `createWizard.labelsHint` | 回车或逗号分隔；小写字母、数字、连字符；最多 12 个 |
| `createWizard.labelsInvalid` | 标签只能包含小写字母、数字和连字符（2–32 字符） |
| `createWizard.configHint` | 以下参数会作为出厂默认值写入 manifest；留空则使用平台默认值 |
| `createWizard.defaultValue` | 默认 {{value}} |
| `createWizard.mcpHint` | 声明该 Agent 建议使用的 MCP 服务器。安装后会出现在工具面板，由用户确认 |
| `createWizard.mcpHosted` | 平台托管 |
| `createWizard.mcpPresets` | 预设 |
| `createWizard.mcpWhyLabel` | 说明（展示给用户） |
| `createWizard.mcpRequired` | 必需 |
| `createWizard.mcpToolsLater` | 具体工具在启用后于工具面板勾选 |
| `createWizard.capabilityHint` | 声明其他 Agent 可通过 intent 寻址的动作；技能是内部流程，能力是对外契约 |
| `createWizard.capabilityAdd` | 添加动作 |
| `createWizard.capabilityEmpty` | 未声明对外动作。仅在希望其他 Agent 调用它时才需要 |
| `createWizard.skillToolDeps` | 依赖工具：{{tools}} |
| `createWizard.skillDepsUndeclared` | 未在工具/MCP 中声明（不阻断） |
| `createWizard.previewErrors` | {{count}} 个错误（无法创建） |
| `createWizard.previewWarnings` | {{count}} 条提示（可继续） |

需要**改写**的旧文案：
- `createWizard.skillsHint`：现为「每个 SKILL.md 会成为 manifest 的一个 capability」→ 改为「拖入技能 ZIP 包加入此 Agent。技能是内部流程，不对外暴露；需要对外暴露的动作请在下一步声明。」

---

## 6. 走查待拍板

1. **`必需` 开关在向导里默认关**（推荐）还是默认开？默认开会让新 Agent 一装上就带红角标。
2. 创建成功后**自动切到工具标签**并展开建议分组（推荐是）；还是只弹一条 toast「该 Agent 建议配置 1 个 MCP」？
3. 推荐全部处理完后，这个分组是**保持显示**（变成全部「已启用」的静态行），还是自动收起只留标题？推荐：保持显示，但默认折叠，标题显示「已全部启用」。

确认后按 Phase 4a / 4b 实施。