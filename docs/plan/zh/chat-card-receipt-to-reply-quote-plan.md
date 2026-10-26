# 聊天卡片：以答案引用行取代「已回答回执卡」

- 状态：已实施（`37056c19` + `75801a35`）
- 前置：[chat-card-accent-chrome-fix-plan.md](chat-card-accent-chrome-fix-plan.md)（高亮色越界修复）。本文件处理该修复暴露出的下一层问题。
- 范围：`apps/acowork-desktop` 的 1:1 chat 面与项目群面。执行面（`ChatPanel` / `AskQuestionCard`）与移动端不在本次之内。

---

## 1. 问题

accent 越界修完之后，聊天里的交互卡片在视觉上已经统一，但**已答态回执卡本身是冗余的**：

| 观察 | 证据 |
|------|------|
| 回执卡不记录答案，也不记录答题人 | [InlineInteractionCard.tsx](../../../apps/acowork-desktop/src/components/chat/InlineInteractionCard.tsx) 旧回执分支：头部只有图标 + 「已回答/已结束」，正文只有 `{question}` |
| 答案与链接另有其行 | 答案落一条普通 `user_message`，`meta.in_reply_to` = 卡片 message id（[chatSessionStore.ts:458-465](../../../apps/acowork-desktop/src/stores/chatSessionStore.ts#L458)） |
| 两个面对同一事件表现不一致 | 执行面答完调 `resolveQuestion` 直接移除问题，不留任何痕（[ChatPanel.tsx:2073](../../../apps/acowork-desktop/src/components/chat/ChatPanel.tsx#L2073)） |
| 回执卡把正文当纯文本渲染 | 旧分支 `{question}` 未走 markdown，反引号与 `[text](url)` 原样出现在聊天流里 |

即：一张带标题栏、描边、内边距的完整卡片，唯一作用是把刚读过的问题再抄一遍。

## 2. 规格依据

不是自创方案，而是回到 [design 27 §5.5.4](../../design/zh/27-intent-messaging-and-group-chat.md)：

> 卡片落一行；它的状态由后续推导——**不存在 `in_reply_to` 指向它的行 = 未决**。

记录职责本来就在答案行上，卡片只是渲染。而 §5.5.5（v2.4）规定历史「只记对话与**未获回答的事实**」——这两句合起来正好是本次的形状：**已答 → 不留卡片；未答 → 必须留痕**。

## 3. 改动

### 3.1 答案引用行（`ReplyQuote`）

新增 [ReplyQuote.tsx](../../../apps/acowork-desktop/src/components/chat/ReplyQuote.tsx)：一行、`border-border-divider` 左条 + `text-text-tertiary`，问题原文折叠为单行并截断至 60 字，完整文本挂 `title`。

两个面各自解析引用文本，且**只索引 `interaction_request` 行**：

```ts
if (r.kind !== "interaction_request" || !r.meta?.message_id) continue;
m.set(r.meta.message_id, r.meta.interaction?.question || r.body);
```

三条非显然的选择：

1. **放在气泡上方，不放气泡内部**。自己的气泡底色是 `--color-chat-user`（= accent-strong 90%）配白字，tertiary 灰放进去不可读；而高亮底上再描一条高亮线正是上一轮定下的红线。附件 chip 与 TaskChip 本来也在气泡上方，语法一致。
2. **只索引卡片行**。`in_reply_to` 也指向普通消息，照单全收会把「回复了一句普通话」画成「回答了一个决策」。
3. **问题优先于 body**。行 `body` 只是摘要（审批卡是 `"Tool approval: bash"`，问题正文是 `"Approve \`bash\`? <风险说明>"`），引用应当是读者当时在卡上看到的那句话——与卡片自身的 `it?.question || row.body` 同一条解析规则。

### 3.2 卡片状态收敛

| 状态 | 之前 | 之后 |
|------|------|------|
| 已答 | 完整回执卡（重复问题） | **不渲染** |
| 未答而关闭（轮次结束 / 超时 / 停止） | 完整回执卡 | 卡片槽位内一行安静注记，含被截断的问题 |
| 开放 / 只读 | 中性描边卡 | 不变 |

这行注记属于**事件文本**的行级档：留在该行内容列（`ml-12`）并带 kind 图标，与轮次级（居中、无图标）区分。两档的完整判据已固化为 transcript 的落点语法，见 [design 27 §5.5.5](../../design/zh/27-intent-messaging-and-group-chat.md) 与共享原语 `ChatEventText`。

i18n：`cardAnswered` / `cardClosed` 失去渲染方，五语删除；新增 `cardUnanswered`（`未获应答：{{question}}`）。

## 4. 测试

| 文件 | 钉住什么 |
|------|----------|
| `ReplyQuote.test.tsx`（新，7 条） | 折叠与截断、空白不渲染、中性描边（不含 accent、也不用 `border-border-outer`）、左右对齐、`title` 保留全文 |
| `GroupChatPanel.replyQuote.test.tsx`（新，3 条） | 群面：已答无卡 + 答案带引用；`in_reply_to` 指向非卡片行时**不**出引用；未答关闭留一行 |
| `InlineInteractionCard.test.tsx`（改 2 条） | 已答 → `container.firstChild` 为 null；未答 → 单行且无 `border` / 无 `bg-` / 无按钮 |
| `AgentChatView.test.tsx`（改 1 条） | 1:1 面同上两条轴 |
| `chatCardChrome.test.tsx`（改 1 条） | 已答不画任何壳（壳不存在，也就无从再刷成 accent） |

红→绿验证：把已答早退改成恒假 → 两个面共 4 条用例变红；把群面 `replyQuote` 置空 → 群面引用用例变红。两处均已还原。

> 第一轮红测时发现一个**假绿**：只断言「没有 `cardAsk` 标题」不足以防住「已答卡落到未答分支」——那会打出「未获应答：<问题>」，比原回执更糟。因此两个面的用例都补了 `queryByText(/chatView.cardUnanswered/)` 为 null。

回归：桌面端全量 196 文件 / **1774 通过**（较改动前 +10 条），`tsc --noEmit` exit 0。

## 5. 未做

1. **引用行不可点击跳转**。卡片就在同一窗口内，滚动即见；加跳转要引入锚点与滚动定位，收益不抵复杂度。
2. **执行面（`AskQuestionCard`）的回执结构未动**。它本来就没有回执，是本次对齐的参照物；正文 markdown 渲染见 §6。
3. **移动端**未动：移动端不渲染 `interaction_request` 卡片回执，无对应物。
4. **卡片正文 markdown 渲染**：原列为遗留，已在同一批次内完成，见 §6。

## 6. 追加：卡片正文走 markdown + `ChatMarkdown` 提取为共享件

回执删除后，用户在同一处截图指出：开放卡正文把 markdown 语法原样画出来（`` `bash` `` 带反引号、`[标题](路径)` 带方括号）。问题文本是模型输出，带 markdown 是常态，而紧邻的聊天气泡会渲染它——卡片是唯一不渲染的 chat 面。

### 6.1 结构前提

`ChatMarkdown` 在 `AgentChatView`、`GroupChatPanel`、`MessageBubble` 各有一份局部实现。三份的**外壳 class 字符串完全相同**，元素覆盖表也同为 `pre`/`a`/`table` 三个键；差异只在群聊版多了 `p`/`code`/`ul`/`ol`/`li` 的间距覆盖、1:1 与执行面版 `pre` 带 `dedent`、群聊版 `a` 写死 accent 类名。已漂移的三份副本正是 `9b071b66` 表格缺陷的成因，因此先提取再复用：

| 文件 | 改动 |
|---|---|
| `chat/ChatMarkdown.tsx`（新增） | 合并三份副本：外壳取共同字面量，覆盖表取并集（`dedent` + 走 `.prose a` 的链接 + 表格滚动壳），导出 `CHAT_MARKDOWN_SHELL_CLASS` |
| `AgentChatView.tsx` / `GroupChatPanel.tsx` | 删本地副本，改 import 共享件（−67 / −82 行） |
| `MessageBubble.tsx` | 覆盖表改用共享件；外壳 div 保留（它经 `StreamMarkdown` 流式渲染），但引用同一常量 |

行为保持：外壳字面量逐字取自原三份（含执行面的 heading 字号与 `[&_th]`/`[&_td]` 单元格底色——三份原本一致，所以是共享默认值而非按面传参），覆盖表键集合与原来相同，无独有 handler 丢失。

### 6.2 卡片接入

`InlineInteractionCard` 开放态 [:199](../../../apps/acowork-desktop/src/components/chat/InlineInteractionCard.tsx#L199)、只读态 [:178](../../../apps/acowork-desktop/src/components/chat/InlineInteractionCard.tsx#L178)、`AskQuestionCard` [:85](../../../apps/acowork-desktop/src/components/chat/AskQuestionCard.tsx#L85) 三处裸 `{question}` 改为 `<ChatMarkdown content={question} />`。**未答关闭态的单行标记保持纯文本**——它是状态行不是正文，且 `title` 属性承载不了 markup。

### 6.3 测试与验证

`ChatMarkdown.test.tsx`（5 条）+ `cardMarkdown.test.tsx`（4 条）。卡片用例同时断言**渲染结果出现**（`<code>`、`<a>`）与**原始语法消失**（文本中不再含 `` `bash` ``、`[the plan](`），否则「退回裸文本节点」这种回归仍可能靠树里别处的 `code` 蒙过。

红→绿双向：临时回退开放态为裸文本、删掉 `prose` 作用域、去掉 `dedent` —— 三条各自转红；还原后桌面全量 1783 通过、`tsc` exit 0。

### 6.4 自检发现的两处自伤

1. **假绿断言**：首版 `ChatMarkdown.test.tsx` 写的是 `toContain("prose")`（被 `prose-sm` 满足）与 `toBe(CHAT_MARKDOWN_SHELL_CLASS)`（拿组件自己读的常量和该常量比，恒真）。红→绿验证时删掉 `prose` 却没有转红才暴露，改为与字面量比较 + 断言 `prose` 独立成词。
2. **误改共享外壳**：提取时一度把默认外壳写成 `prose … dark:prose-invert … [&>*:first-child]:mt-0`，并给执行面留了第二个常量。`git diff` 核对原副本后发现三个面原本用的是**同一条**（含 `[&_th]:bg-chat-title`），照我误写的默认值会把两个 chat 面的表格单元格底色改掉、并凭空引入 `dark:prose-invert`。已回退为单一共享默认值。教训：合并副本时「更合理的写法」不是依据，逐字比对才是。
