# ADR-120 Agent 系统提示词拆分：`system.md`（角色身份）与 `rules.md`（业务场景规则）

- **状态**：已定案（运行时 + 6 个官方 agent 包已实现）
- **日期**：2026-11-23
- **决策者**：大鱼
- **前置**：[ADR-063](./ADR-063-package-level-prompt-override.md)（`prompts/` 特殊文件名覆盖 + 提示词编辑/热重载）、[ADR-094](./ADR-094-progressive-skill-auto-load-and-load-skill-tool.md)（对话基座与技能摘要分离）、[ADR-104](./ADR-104-agent-manifest-declaration-surface.md)（`.agent` 包是构建产物）

---

## 1. 背景

### 1.1 事实：一个 `system.md` 混了两类内容

拆分之前，每个 agent 包的 `prompts/` 只有一个 `system.md`，里面同时写着两类东西：

| 类别 | 例子（senior-engineer） | 换业务场景时是否需要变 |
|---|---|---|
| 角色通用属性 | 身份、领域专长、工程原则、代码评审清单、调试方法、沟通与输出格式 | 不变——变了就是换了角色 |
| 业务场景规则 | 项目参与闭环、系统记录、交付目录 `{doc_path}/技术方案/`、审批门、状态机、非目标 | 变——不同业务场景的交付规范、工具面、审批链不同 |

这就是问题本身：**同一个角色落到不同业务场景时，改动只能整篇重写 `system.md`**。于是

- 一次改动里同时混着"角色变了"和"场景变了"，评审无法分辨；
- 同角色的不同部署无法各自维护规则集，只能各复制一份整篇提示词，然后任其漂移；
- 规则里的事实（哪个服务是系统记录、交付物落在哪个目录）被埋在身份描述中，读的人容易当成角色偏好而不是硬约束。

### 1.2 事实：运行时如何拼接 `prompts/`

`build_system_prompt_with_mode`（`core/acowork-runtime/src/package/prompt_builder.rs`）：

1. `collect_markdown_files` 收集 `prompts/` 下所有 `.md` / `.txt`；
2. **按文件名排序**；
3. 跳过 `OVERRIDABLE_PROMPTS` 名单内的任务指令性文件（`summary.md`、`title.md`、`compact-template.md`、`abstention.md`、`chat.md`）；
4. 每段以 `## {文件名词干}\n\n{正文}` 形式拼接，段间以 `\n\n---\n\n` 分隔。

因此新增一个 `prompts/rules.md` **会被自动纳入主系统提示词**，但按字母序会排在 `system.md` **之前**——契约跑到它所约束的身份前面。

### 1.3 事实：实例级定制通道已经存在

- 每个 agent 实例的安装目录带一份完整的 `prompts/` 与 `skills/`；
- Setup 面板的「提示词与技能」卡片通过 `__agent_package__` 工作区读写这两个子树（ADR-063 §3.7）；
- 保存后 `POST /api/agents/{id}/prompts/reload` 会重建主系统提示词并推送到所有活跃会话。

结论：**"落地不同业务场景之后的定制"不需要新机制**，缺的只是把可定制的部分从角色身份里切出来。

## 2. 决策

### 决策 1：两个文件的职责边界，判据是"换掉它是否换了角色"

- `prompts/system.md` —— **角色身份**：是谁、擅长什么、坚持什么原则、怎么表达。场景无关；换掉它等于换了一个 agent（应当是新包，不是新配置）。
- `prompts/rules.md` —— **业务场景契约**：谁是系统记录、允许用哪些工具、交付物落在哪里、有哪些审批门、按什么工作流推进、什么不做。换掉它角色不变，做事方式变。

判据是可操作的：**把文件换掉，如果团队需要重新认识"这个 agent 是谁"，它属于 `system.md`；如果只是"它在这个场景里必须这么干"，它属于 `rules.md`。**

两个容易混淆的边界，按同一条判据裁决：

- **角色方法论留在 `system.md`**（senior-engineer 的调试五步、代码评审清单；architect 的设计评审清单与反模式）——它是这位工程师的做事方式，换场景不变。
- **平台/项目工作流归 `rules.md`**（`Architecture Workflow`、`Default Workflow`、`Quality Workflow`、`Your Loop`）——它由项目怎么运转决定，换场景就换。

### 决策 2：运行时固定段序 `system.md` → `rules.md` → 其余按文件名

新增 `PROMPT_SECTION_ORDER = [SYSTEM_PROMPT_FILE, RULES_PROMPT_FILE]` 与 `prompt_section_rank()`：名单内的文件按名单顺序在前，其余按文件名升序在后。

- 理由：身份必须先于约束它的契约，两者都先于零散补充段；
- 匹配**大小写敏感、整名精确**，与 `is_overridable_prompt` 的匹配语义一致——否则在不区分大小写的文件系统上 `Rules.md` 会悄悄抢占 `rules.md` 的位置，而别处仍把它当普通段。

### 决策 3：`rules.md` 是普通对话段，不是任务指令覆盖项

它**不**进入 `OVERRIDABLE_PROMPTS`：

- 它要出现在主系统提示词里（否则读不到）；
- 它不是"替换某个内置常量"，`load_optional_prompt` 的覆盖链对它没有意义；
- 它必须能在 Setup 面板的提示词树里被直接编辑——普通段天然满足。

### 决策 4：单文件包保持向后兼容

只有 `system.md`、没有 `rules.md` 的包行为与拆分前完全一致（有单测 `test_build_system_prompt_without_rules_is_unchanged` 锁定）。因此这次拆分不需要数据迁移，也不需要包格式版本号变更。

### 决策 5：场景定制的落点是实例内 `prompts/rules.md`

包内的 `rules.md` 是**默认值**，不是唯一定义。同一角色部署到不同业务场景时：

1. 在该实例的 Setup 面板「提示词与技能」里编辑 `prompts/rules.md`；
2. 保存后 reload 即对下一轮生效，无需重启、无需改包、无需新 ADR；
3. 场景规则的变化不再触碰角色身份，评审时一眼能看出"这次改的是场景"。

### 决策 6：官方 agent 包本批完成源码迁移，打包随合并统一进行

`project-manager`、`document-manager`、`product-manager`、`quality-assurance`、`senior-engineer`、`software-architect` 六个包按决策 1 拆分，版本号各 +1 patch。`agents/agent-packages/*.agent` 的重新打包**本批不做**：另一个分支上还有 agent 未合并，打包与发布统一到合并之后一起做，避免出现"一部分包已签名、一部分还是旧内容"的中间状态。

代价是明确的：在统一重新打包之前，`core/acowork-sign/tests/example_packages.rs`（要求包与源树逐字节一致）会保持失败——这是**预期状态**，不是缺陷；重新打包后自然转绿。

第三方包 `github.ponytail` 不迁移：它的提示词整体就是一套"懒"的工程人格，没有平台业务场景约定可切，硬切只会制造一个空文件。

## 3. 后果

### 正面

- "角色变了"与"场景变了"成为两种可见、可分别评审的改动。
- 同角色的多场景部署共享同一份 `system.md`，规则各自演进，不再整篇复制。
- 场景规则（系统记录、交付目录、审批门）集中在一处，新成员读一个文件就知道"在这个场景里必须怎么干"。
- 段序由代码固定，不依赖文件名排序的巧合。

### 负面 / 取舍

- 包升级会以包内 `rules.md` 覆盖实例内的场景定制——**实例定制目前不随包升级保留**。需要长期保留的场景规则应回到包源（或另行设计覆盖层，不在本 ADR 范围内）。
- 两个文件意味着作者要做一次归属判断；边界模糊的段落（例如某角色的方法论）需要显式决定落在哪边，本 ADR 给出的判据是唯一标准。
- 拼接后的字符数与拆分前基本一致，不增加上下文成本。

### 验证

- `cargo test -p acowork-runtime --lib`：2117 passed（含 5 个新增用例）。
- `cargo test -p acowork-core --test agent_participation_contract`：3 passed。
- `./dev/ci.sh redlines`：全绿。
- 打包验证（`cargo test -p acowork-sign --test example_packages`）在**统一重新打包之后**执行——见决策 6，当前保持失败是预期状态。

## 4. 相关文件

- [core/acowork-runtime/src/package/prompt_builder.rs](../../../core/acowork-runtime/src/package/prompt_builder.rs)（`SYSTEM_PROMPT_FILE`、`RULES_PROMPT_FILE`、`PROMPT_SECTION_ORDER`、`prompt_section_rank`）
- [core/acowork-runtime/src/http/prompts.rs](../../../core/acowork-runtime/src/http/prompts.rs)（保存后 reload 链路，未改动）
- [agents/senior-engineer-agent/prompts/system.md](../../../agents/senior-engineer-agent/prompts/system.md) / [rules.md](../../../agents/senior-engineer-agent/prompts/rules.md)
- [docs/design/zh/02-agent-package.md](../design/zh/02-agent-package.md)（包结构）
- [docs/design/zh/03-agent-runtime.md](../design/zh/03-agent-runtime.md) §3.1（上下文构建规则第 1 层）
