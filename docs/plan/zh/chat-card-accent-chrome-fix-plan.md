# 聊天弹出卡片高亮色描边修复计划

> 版本：v1.0 | 日期：2026-10-26
>
> 关联提交（根因）：`75b411f7` fix(chat): dismiss exec card on chat answer, stop phantom wait line after completion
> 其中 `feat(ui): unify chat interaction card visual language across surfaces` 一段把交互卡片的**边框线**与**标题文字**改成了品牌高亮色
> 关联视觉原型：[`docs/prototypes/chat-card-styles.html`](../../prototypes/chat-card-styles.html)（`--accent-border` / `--accent-soft`，本次不改原型，仅记录其与全局语言冲突）
>
> **一句话**：全局设计语言只允许高亮色出现在**按钮背景**上，卡片边框必须用中性描边 token、标题必须用常规层级文字色；本计划把聊天界面里被改成「高亮边框 + 高亮标题」的弹出卡片与内联 chip 一次性归位，并用测试钉死，防止再次漂移。

---

## 1. 定位与边界

| 项 | 说明 |
|----|------|
| 范围 | 桌面端 `src/components/chat/` 的交互卡片（2 个组件、3 个状态变体）+ 内联 chip/badge（4 处）；移动端 `.chat-task-chip` 1 处 |
| 不在范围 | 组件结构、交互逻辑、i18n 文案、布局尺寸；原型 HTML；`docs/design` 文档 |
| 改动性质 | **纯样式类名替换**，只允许改 `border-*` / `text-*` / `bg-*` 三类 token 引用 |
| 硬约束 | 不杀 gateway / runtime / dev server 进程；注释一律英文；不回退 `9b071b66`（ChatMarkdown `prose` 外壳 + `GroupChatPanel.markdownTable.test.tsx`） |

### 1.1 两条已确认的边界裁决

| 问题 | 裁决 | 依据 |
|------|------|------|
| `InlineInteractionCard` 的 approval（emerald）/ pause（amber）边框要不要一起改中性？ | **保留**，只改 accent | 二者是**状态语义色**，不是品牌高亮色；仓库既有约定「状态语义色统一用 emerald/amber，不占用 `--color-accent`」 |
| 内联 chip / badge（TaskChip、技能徽章、上传中 DocumentChip、pending 标签）要不要一起改？ | **一起改** | 同一处根因、同一条规则（边框+文字用高亮色），分两次改反而容易漏 |

---

## 2. 现状事实（代码证据）

### 2.1 设计语言基线

| 角色 | 标准写法 | 位置 |
|------|----------|------|
| 卡片/弹层外描边 | `border-border-outer`（light `#E4E4E7` / dark `#212121`，自动翻转，无需 `dark:`） | [globals.css:87](../../../apps/acowork-desktop/src/styles/globals.css#L87)、[:642](../../../apps/acowork-desktop/src/styles/globals.css#L642) |
| 卡片内部分隔线 | `border-border-divider` | [globals.css:90](../../../apps/acowork-desktop/src/styles/globals.css#L90)、[:646](../../../apps/acowork-desktop/src/styles/globals.css#L646) |
| 标签 pill 底色 | `bg-chat-badge`（注释即写明 "sender-role label pill"） | [globals.css:94](../../../apps/acowork-desktop/src/styles/globals.css#L94)、[:597](../../../apps/acowork-desktop/src/styles/globals.css#L597) |
| 标题文字 | `text-text`（高对比层级）；弱化态 `text-text-tertiary` | [globals.css:11/16](../../../apps/acowork-desktop/src/styles/globals.css#L11) |
| 高亮色唯一合法用途 | 按钮填充 `.btn-accent` / `bg-accent-strong` + 白字 | [globals.css:1649-1670](../../../apps/acowork-desktop/src/styles/globals.css#L1649-L1670) |
| 高亮色**允许**的例外 | 交互态的**背景**洗色（hover / selected），见 `listRowHover = hover:bg-[var(--color-accent)]/10` | [ui-styles.ts:104-121](../../../apps/acowork-desktop/src/lib/ui-styles.ts#L104-L121) |

**已合规的同类卡片参照**（对齐目标，不自创类名）：

1. [ChatHistoryDialog.tsx:278-288](../../../apps/acowork-desktop/src/components/chat/ChatHistoryDialog.tsx#L278) — `border-border-outer bg-modal-surface text-text` + `border-border-divider` 头部分隔 + `text-sm font-semibold` 标题
2. [NewGroupDialog.tsx:145-151](../../../apps/acowork-desktop/src/components/chat/NewGroupDialog.tsx#L145) — 同上，标题 `text-sm font-semibold text-text`
3. [capsule.ts:28](../../../apps/acowork-desktop/src/components/common/capsule.ts#L28) — 共享卡片壳原语 `rounded-xl border border-border-outer`
4. **同文件内的正确写法**：[InlineInteractionCard.tsx:125-127](../../../apps/acowork-desktop/src/components/chat/InlineInteractionCard.tsx#L125-L127) 回执态 = `border-border-outer` + `bg-chat-title` + `text-text-tertiary`；[CodeBlock.tsx:123-125](../../../apps/acowork-desktop/src/components/chat/CodeBlock.tsx#L123-L125) 标题栏 = `bg-chat-title`

> 结论：卡片头部（header bar）的中性配方在仓库里已有明确先例 —— **`bg-chat-title` 底 + `text-text` 文字 + `border-border-divider` 分隔**。修复直接复用它。

### 2.2 违规清单（桌面端）

| # | 文件:行 | 违规写法 | 类别 |
|---|---------|----------|------|
| A1 | [AskQuestionCard.tsx:56](../../../apps/acowork-desktop/src/components/chat/AskQuestionCard.tsx#L56) | `border-[var(--color-accent)]/30 dark:border-[var(--color-accent)]/40` | 卡片边框 |
| A2 | [AskQuestionCard.tsx:58](../../../apps/acowork-desktop/src/components/chat/AskQuestionCard.tsx#L58) | `bg-[var(--color-accent)]/10 … text-[var(--color-accent-strong)]` | 卡片标题 |
| B1 | [InlineInteractionCard.tsx:82](../../../apps/acowork-desktop/src/components/chat/InlineInteractionCard.tsx#L82) | `borderCls: "border-[var(--color-accent)]/30 dark:…/40"` | 卡片边框（`kindConfig` 的 ask_question 分支） |
| B2 | [InlineInteractionCard.tsx:83](../../../apps/acowork-desktop/src/components/chat/InlineInteractionCard.tsx#L83) | `headerBgCls: "bg-[var(--color-accent)]/10 text-[var(--color-accent-strong)]"` | 卡片标题 |

B1/B2 是**单一改动点覆盖多视觉变体**：`cfg.borderCls` / `cfg.headerBgCls` 同时被**开放态**（[:161-163](../../../apps/acowork-desktop/src/components/chat/InlineInteractionCard.tsx#L161-L163)）和**只读态**（[:144-145](../../../apps/acowork-desktop/src/components/chat/InlineInteractionCard.tsx#L144-L145)）消费，且群聊与 1:1 两个入口共用该组件；浅/深主题由 token 自动翻转。回执态（:125）本就中性，不动。

| # | 文件:行 | 违规写法 | 类别 |
|---|---------|----------|------|
| C1 | [GroupChatPanel.tsx:242-243](../../../apps/acowork-desktop/src/components/chat/GroupChatPanel.tsx#L242-L243) | `border-accent/30 bg-accent/10 text-accent` + `hover:border-accent/50` | TaskChip |
| C2 | [ChatPanel.tsx:2764](../../../apps/acowork-desktop/src/components/chat/ChatPanel.tsx#L2764) | `border-accent/20` + `style={{color: accent}}` | 技能徽章 |
| C3 | [AgentChatView.tsx:888](../../../apps/acowork-desktop/src/components/chat/AgentChatView.tsx#L888) | 同 C2（复制粘贴的双胞胎） | 「回复中」徽章 |
| C4 | [DocumentChip.tsx:59](../../../apps/acowork-desktop/src/components/chat/DocumentChip.tsx#L59) | `border-[var(--color-accent)] animate-pulse` | 上传中态边框 |
| C5 | [ChatPanel.tsx:2703](../../../apps/acowork-desktop/src/components/chat/ChatPanel.tsx#L2703) | `bg-accent/10 text-accent`（非交互标签） | pending「来自聊天」pill |

### 2.3 违规清单（移动端）

| # | 文件:行 | 违规写法 |
|---|---------|----------|
| M1 | [app.css:1899-1910](../../../apps/acowork-mobile/src/styles/app.css#L1899-L1910) | `.chat-task-chip { border: 1px solid var(--color-tint-strong); color: var(--color-tint-strong) }` |

移动端聊天卡片本身**合规**：`.chat-card-line-btn` 用 `var(--color-border)`（[app.css:1934-1946](../../../apps/acowork-mobile/src/styles/app.css#L1934-L1946)）、`.sheet-title` 用 `--color-label-secondary` + `--color-separator`（[:795-802](../../../apps/acowork-mobile/src/styles/app.css#L795-L802)）、`.question-card` 无描边。`.chat-card-tag` / `.chat-sheet-option` 的 amber 属状态语义色，按 §1.1 保留。

### 2.4 明确不动的高亮色用法（避免过度整改）

| 位置 | 用法 | 保留理由 |
|------|------|----------|
| [SessionTabBar.tsx:813](../../../apps/acowork-desktop/src/components/chat/SessionTabBar.tsx#L813)、FileTreeNode.tsx:553 | 重命名输入框 `border-accent` | 与全局输入态约定一致（`focus:border-[var(--color-accent)]`，见 StyledInput / Dropdown / ui-styles `inputBase`） |
| [GroupMembersPanel.tsx:152](../../../apps/acowork-desktop/src/components/chat/GroupMembersPanel.tsx#L152)、UserGroupMembersPanel.tsx:279 | 加成员虚线按钮 `hover:border-accent/50` | 按钮 hover 态，非卡片描边；不在本次清单内 |
| NewGroupDialog.tsx:169 / UserGroupMembersPanel.tsx:399 | `focus:border-accent/50` | 输入 focus 态，全局约定 |
| NewGroupDialog.tsx:226、UserGroupMembersPanel.tsx:264 | 选中圆点 `bg-accent` + 白字 | 按钮/控件填充 |
| ChatHistoryDialog.tsx:79、ExploreBlock.tsx:753、ContextUsageIcon.tsx:300 | 命中高亮 / 结果底纹 / loading spinner | 背景洗色与 spinner，非「边框+标题」 |
| InlineInteractionCard.tsx:68-69, 75-76 | emerald / amber 边框+头部 | 状态语义色（§1.1 裁决） |
| 各文件 `btn-accent` / `bg-accent-strong` 按钮 | 按钮填充 | 规则明确允许 |

---

## 3. 修复方案

### 3.1 卡片（A1/A2/B1/B2）

| 元素 | 现在 | 改为 |
|------|------|------|
| 卡片外壳边框 | `border border-[var(--color-accent)]/30 dark:border-[var(--color-accent)]/40` | `border border-border-outer` |
| 头部栏 | `bg-[var(--color-accent)]/10 … text-[var(--color-accent-strong)]` | `bg-chat-title … text-text` |

- 头部保留 `border-b border-border-divider`（已合规）。
- 同步修正 [InlineInteractionCard.tsx:12](../../../apps/acowork-desktop/src/components/chat/InlineInteractionCard.tsx#L12) 的文档注释「Open: accent border + semantic header bg」→ 描述为中性描边 + 中性头部，并注明语义色只属于 approval/pause。
- `KindConfig` 的字段名 `borderCls` / `headerBgCls` 保持不变（不重构结构）。

### 3.2 内联 chip / badge（C1–C5）

统一配方：**边框中性、文字中性、高亮色只留在背景洗色**。

| # | 改为 |
|---|------|
| C1 TaskChip | `border-border-outer bg-[var(--color-accent)]/10 text-text-secondary` + `hover:bg-[var(--color-accent)]/20`（hover 从「边框变高亮」改为「背景加深」，与 `listRowHover` 家族一致） |
| C2/C3 徽章 | 去掉 `border-[var(--color-accent)]/20` 与内联 `style={{color}}` → `border-border-outer text-text-secondary`，保留 `bg-[var(--color-accent)]/10` |
| C4 上传中 | `border-[var(--color-accent)] animate-pulse` → `border-border-divider animate-pulse`（进度感由 pulse 承担；`border-border-outer` 与静置态同色会完全不可辨，故取更重一档的中性分隔色） |
| C5 pending pill | `bg-[var(--color-accent)]/10 text-[var(--color-accent)]` → `bg-chat-badge text-text-secondary`（它不是按钮也不是行，用专门的标签 pill token） |

### 3.3 移动端（M1）

```css
.chat-task-chip {
  border: 1px solid var(--color-border);
  color: var(--color-label-secondary);
}
```
与桌面 TaskChip 对齐（透明底 + 中性描边 + 次级文字），保留 `--color-tint-strong` 给按钮类控件。

---

## 4. 测试方案

### 4.1 新增桌面端测试

`apps/acowork-desktop/src/components/chat/chatCardChrome.test.tsx`

优先用**真实 DOM 断言**（渲染组件后读 shell/header 的 `className`），仅在渲染成本过高时退回源码扫描（先例：[capsule.test.tsx](../../../apps/acowork-desktop/src/components/common/capsule.test.tsx) 读源码、[markdownTable.roundedCorners.test.tsx](../../../apps/acowork-desktop/src/components/chat/markdownTable.roundedCorners.test.tsx) 读 CSS）：

| 用例 | 断言 |
|------|------|
| InlineInteractionCard ask_question 开放态 | 外壳 `className` 含 `border-border-outer`、不含 `--color-accent`；头部含 `bg-chat-title` + `text-text`、不含 `accent-strong` |
| InlineInteractionCard 只读态 | 同上（钉住「一个 cfg 覆盖多变体」的第二入口） |
| InlineInteractionCard 回执态 | 保持 `border-border-outer`（防回归基线） |
| InlineInteractionCard approval / pause | **仍**含 `emerald-500/30` / `amber-500/30` —— 把 §1.1 裁决钉成代码契约，防止后来者「顺手统一」掉语义色 |
| AskQuestionCard | 外壳 `border-border-outer`、头部 `bg-chat-title text-text`，均不含 accent |
| GroupChatPanel TaskChip | 复用既有 `GroupChatPanel.taskChip.test.tsx` 的渲染 harness，追加 className 断言 |
| DocumentChip 上传中 | `border-border-divider`，不含 accent |
| ChatPanel / AgentChatView 徽章、pending pill | 源码扫描（整面板渲染需 mock 大量 store，成本与脆弱度不划算） |
| 反向守卫 | 三个卡片文件里 `btn-accent` 仍存在 —— 证明修复没有把合法按钮高亮一起删掉 |

### 4.2 新增移动端测试

`apps/acowork-mobile/src/__tests__/task-chip-style.test.ts` — 读 `src/styles/app.css`，断言 `.chat-task-chip` 块内 `border`/`color` 不含 `--color-tint-strong` / `--color-accent`，且含 `--color-border`。

### 4.3 红→绿双向验证

1. 先跑新增测试 → 必须**全红**（此时修复未落地）；
2. 落地 §3 修复 → 全绿；
3. 再把 `InlineInteractionCard.tsx` 的 `borderCls` 临时改回 accent → 对应 2 条用例必须**重新变红**，然后还原。三步都留日志证据。

### 4.4 回归范围

- 桌面：`npx vitest run src/components/chat`
- 移动：`npx vitest run src/__tests__/task-chip` + 全量 `npm test`

---

## 5. 提交拆分

| # | 内容 | 说明 |
|---|------|------|
| 1 | `style(desktop): return chat interaction cards to neutral chrome` | A1/A2/B1/B2 + 注释修正 + 卡片测试 |
| 2 | `style(desktop): drop accent from inline chat chip borders and labels` | C1–C5 + chip 测试 |
| 3 | `style(mobile): neutralize the task chip border and label` | M1 + 移动端样式测试 |

三个 commit 各自独立可 review、可回滚，且每个 commit 只处理一类卡片（符合「一次只处理一类」的收敛要求）。

---

## 6. 验收标准

- [ ] 聊天界面所有弹出卡片：边框 = `border-border-outer`，标题 = `text-text`，accent 只剩在按钮背景上
- [ ] 群聊 / 1:1 两个入口、开放态 / 只读态 / 回执态三个变体、浅色 / 深色两套主题全部覆盖（由 token 自动翻转保证）
- [ ] approval / pause 的 emerald / amber 状态色**未被误伤**
- [ ] 新增测试全绿；`src/components/chat` 既有用例零回归；移动端 `npm test` 零回归
- [ ] 红→绿双向验证留痕
- [ ] `9b071b66`（ChatMarkdown `prose` 外壳 + `GroupChatPanel.markdownTable.test.tsx`）完好
- [ ] 无组件结构 / 交互逻辑 / 文案改动，无新增硬编码色值，无自创类名

---

## 7. 遗留（不在本次范围）

1. `docs/prototypes/chat-card-styles.html` 的 `--accent-border` / `--accent-soft` 与本规则冲突，是这次漂移的视觉源头；原型是讨论稿，是否改写待产品决定。
2. 桌面端 11 处 `text-text-primary` 引用的 `--color-text-primary` **在 globals.css 里并不存在**（`@theme` 只定义了 `--color-text` / `-secondary` / `-tertiary` / `-disabled`，见 [globals.css:11-18](../../../apps/acowork-desktop/src/styles/globals.css#L11-L18)），这些类名当前不产生任何颜色。属独立的 token 缺失缺陷，与本次「高亮色越界」根因不同，建议单独立项。
