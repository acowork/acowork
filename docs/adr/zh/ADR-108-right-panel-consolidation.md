# ADR-108：右侧栏面板合并 —— 工具面板下线，卡片并入设置面板

- **编号**：ADR-108
- **状态**：已实施（2026-11-06）
- **日期**：2026-11-06
- **决策者**：Nicholas（方案确认）
- **关联**：ADR-029（内置工具持久化与使能）、ADR-034 §7.6.5（工具数据的合并端点）、ADR-048（DevMode 调试）、ADR-063 §3.7（包内 `prompts/` / `skills/` 编辑）、ADR-069（MCP 工具级 opt-in）、ADR-090（MCP 安装归属与安装状态 UI）、ADR-094（`load_skill` 内置工具）、ADR-101（记忆服务与记忆面板门禁）、ADR-104（推荐 MCP 声明与推荐分组）
- **英文 counterpart**：[docs/adr/en/ADR-108-right-panel-consolidation.md](../en/ADR-108-right-panel-consolidation.md)（同 commit 维护）

---

## 摘要

**一句话**：右侧栏从 6 个 Tab 收敛为 **5 个**（Workspace / Status / Memory / Debug / Setup）。「工具」Tab 下线，它的两张卡片——内置工具、MCP 服务器（含推荐分组）——并入「设置」Tab；Debug Tab 的「提示词与技能」卡片同样迁入设置 Tab，Debug Tab 只保留调试开关（含迭代/阶段/Token 读数）、上下文快照与压缩历史。设置 Tab 因此从 4 张卡片变成 7 张，**默认只展开前 3 张**，其余默认折叠。

**一句话（为什么）**：一个 Tab 的存在理由必须和**数据的归属**一致。工具 Tab 里那两张卡改的是**这个 Agent 自己的配置**（`agent_tools.json` / `agent_mcp.json`），与设置 Tab 里改 `agent_config.json` 的卡片是同一对象、同一套权限、同一条写入路径；把它们拆在两个 Tab，用户必须额外记住"哪个开关在哪个 Tab"，而这份记忆负担换来的"视觉分组"并不成立——那个 Tab 里只有两张卡。同理，提示词与技能（ADR-063 §3.7）编辑的是包内文件，与调试能力毫无关系，它出现在 Debug Tab 顶部只是因为当年 Debug Tab 是空面板。

---

## 背景（代码取证）

1. **工具 Tab 的全部内容就是两张卡。** `ToolsTab` 渲染「内置工具」折叠卡、「MCP 服务器」折叠卡，以及夹在两者之间的推荐分组（ADR-104：安装一条推荐会把一行**移进**下面的开关列表，因果必须同屏）。没有第三块内容，也没有 Tab 级工具栏。
2. **设置 Tab 用的是同一套卡片语法。** 两张工具卡与设置 Tab 的四张卡都是 `ListBox` + `ExpandableRow` 的 level-1 折叠卡（同一个 `min-h-[36px]` 头部、同一条 hairline 分隔）。ADR-094 的 `load_skill` 位置描述（"工具面板 → Builtin Tools 列表"）与设置 Tab 里那批卡片属于同一张表。
3. **已有过一次"拆走一份配置"的负债。** ADR-094 为了把技能模式开关从设置面板挪走，把设置 Tab 的 `skillsAutoLoad` 开关删除、改由工具面板的 `load_skill` 行承担。于是"这个 Agent 的配置"被两个 Tab 分走，注释里到处要写"另一处在哪个面板"——本 ADR 把这类跨面板指路一次抹平。
4. **提示词与技能卡片的可见性本来就与调试无关。** ADR-063 §3.7 的要求是"无论是否进入 DevMode、无论 Agent 是否运行，都能浏览与准备覆盖"；把它放在 Debug Tab 顶部只是顺序上的便利，Debug Tab 自己的内容（DevMode 分支、快照、压缩历史）反被它挤到下面。
5. **设置 Tab 的"全展开"是 4 张卡时的性质。** 4 张卡全开是一屏总览；7 张卡全开是一条滚动墙，用户看到的第一个卡片之外全是滚动区。

---

## 决策

### D1 — 右侧栏是 5 个 Tab，`tools` 从类型层面消失

`PanelTab` 收缩为 `workspace | status | memory | debug | setup`。随之：

- 面板注册项（图标、顺序、显示条件）删除 tools 一项，`RightNavBar` 不再需要通用的"lucide 图标插槽"——那是 tools 唯一的用法，其余 Tab 都用 Outline/Filled 遮罩图标对；
- `AppLayout` 的 agent 停止回弹条件同步去掉 `tools`；
- `rightPanel.tools` 文案删除（en / zh-CN）。

不留"隐藏的 tools Tab"：一个仍然可以成为 `activePanelTab` 但没有入口的值，只会让"入口和面板不一致"重新长出来。

### D2 — 卡片归属按"改的是谁的数据"划分，顺序由注册表给出

内置工具、MCP 服务器（含推荐分组）、提示词与技能 → **设置 Tab**，与 Agent Info / Session Params / Model Params / Chat Router 同列。

卡片顺序即 `AGENT_SETUP_CARDS`（[agentSetupCards.ts](../../../apps/acowork-desktop/src/components/right-panel/agentSetupCards.ts)）的顺序，且**该表就是渲染顺序的唯一来源**：

`agentInfo → sessionParams → modelParams → chatRouter → promptsSkills → builtinTools → mcpServers`

搬入的三张卡排在原有四张之后，避免打断既有用户的肌肉记忆。

### D3 — 默认只展开前 3 张，规则单点定义、不持久化

`DEFAULT_EXPANDED_CARD_COUNT = 3`，`defaultOpenForCard(id)` 由卡片在注册表中的**位置**推导。面板用一份 `Record<cardId, boolean>` 驱动全部 `ExpandableRow`，搬入的卡片通过 `defaultOpen` 入参接收初值。

两个刻意的取舍：

- **规则必须在注册表里，不在卡片里。** 七处散落的 `useState(true)` 看起来等价，但插入一张卡片就会静默改变"前三张"的语义，而没有任何地方会报错。
- **不做持久化。** 折叠状态与合并前一样是组件本地状态：Tab 切走即卸载、切回按规则重新初值。为七个布尔值引入一层存储，会让"默认展开前 3 张"这条唯一规则退化成"用户上次碰过什么"。

### D4 — 宿主管滚动，卡片不自带滚动壳

搬入的卡片不再拥有自己的滚动根、Tab 级 padding 与"未选择 Agent"空态——这三件事由设置面板提供（单一 `OverlayScrollbar` 滚动列 + 它自己的空态）。由此：

- **内置工具列表取消九行高度上限。** 那个 `maxHeight` 是它独占一个 Tab 时的产物（当时卡片必须自己限制高度，否则快照/压缩历史会被顶出可视区）；在设置面板里它只是单列滚动中的一块，嵌套滚动只会把行藏到**第二个**滚动位置后面，多一次滚动动作、少一次一眼看完。
- 卡片之间的分隔线沿用宿主同款 `-mx-3 my-2 border-t` hairline，使搬入的三张卡读作同一列的一段，而不是外来的容器。

### D5 — Debug Tab 收窄为"调试",不再承载配置编辑

Debug Tab 保留：调试开关块（Enter/Exit Debug + 会话控制 + 迭代/阶段/Token/会话状态读数）、上下文快照卡片、压缩历史卡片。

**ADR-063 §3.7 的"Debug Tab 顶部"位置不再成立**（卡片内容、编辑交互、reload 语义均不变，只换宿主）。Debug Tab 存在的意义是"观察一次运行"，它不该同时是"编辑下一轮要用的提示词"的地方——后者的写入对象是包文件，与运行态无关。

### D6 — 不变量

- **端点 / Store / 权限 / 写入路径全部不变**：`GET/PUT /tools`、`/mcp-tools`、`/prompts/reload`、`agent_mcp.json` 的使能路径、ADR-087 的只读判定，均未改动；本次只换渲染宿主与入口。
- **ADR-104 的因果相邻不变**：推荐分组仍紧贴 MCP 开关列表，仍在同一张卡片格里。
- **ADR-029 / ADR-094 的开关语义不变**：内置工具行的开关仍是 `PUT /agents/{id}/builtin-tools`；`load_skill` 仍是技能模式的唯一开关。
- **ADR-101 的记忆门禁不变**：Memory Tab 仍需要"Agent 运行中 + 记忆服务就绪"两条事实。

---

## 后果

- **用户**：找控件从 6 个 Tab 变成 5 个；设置 Tab 首屏只展开前 3 张，其余一眼可见标题、一次点击展开。
- **代码**：`ToolsTab.tsx` → `ToolsCards.tsx`（无面板壳、无空态）；`PromptsSkillsCard` 的初始展开由宿主注入（`defaultOpen`）；`RightNavBar` 的通用图标插槽删除。
- **测试**：`agentSetupCards`（规则本身）、`AgentSetupTab.cards`（7 张卡的顺序 + 前 3 张展开 + 内置工具列表无内部滚动宿主）。
- **文档**：PRD §4.4 Tab 表更新并新增 §4.4.3（设置 Tab 的 7 张卡）；ADR-029 / ADR-034 §7.6.5 / ADR-063 §3.7 / ADR-069 / ADR-090 / ADR-094 / ADR-104 中"在工具面板…""在 Debug Tab 顶部…"的位置表述加注指向本 ADR（不改写各 ADR 的历史结论）。

---

## 不做什么

- **不做卡片顺序的用户自定义 / 持久化**（D3 的推论）。
- **不把设置 Tab 再拆成子 Tab**：那只是把"六个面板"换成"一个面板里的六个子面板"，负担没变、还多一层。
- **不删除任何卡片能力**：状态读数与压缩历史留在 Debug Tab；本次没有以"面板更短"为理由裁掉任何功能。
- **不改 MCP 推荐分组的排序/因果**：`promptsSkills → builtinTools` 之间插入推荐分组会破坏 ADR-104 的"安装后立刻看到它进入下面的列表"。

---

## 验证

- `apps/acowork-desktop/src/components/right-panel/agentSetupCards.test.ts`：注册表顺序、前 3 张展开、其余折叠、状态表与规则一致。
- `apps/acowork-desktop/src/components/right-panel/AgentSetupTab.cards.test.tsx`：真实渲染 7 张卡（顺序 + `aria-expanded` = 前三为真）、提示词与技能卡只存在一份、12 个内置工具全部渲染且卡片与行解析到**同一个**面板滚动视口（重新套一层 `OverlayScrollbar` 该用例会失败）。
- `npx tsc --noEmit`、`npm run check:i18n`、`apps/acowork-desktop` 的相关 vitest 套件全绿。
