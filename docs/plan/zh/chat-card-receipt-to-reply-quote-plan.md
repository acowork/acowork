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
2. **执行面（`AskQuestionCard`）未动**。它本来就没有回执，是本次对齐的参照物，不是待修对象。
3. **移动端**未动：移动端不渲染 `interaction_request` 卡片回执，无对应物。
4. **正文 markdown 渲染**：随回执卡删除，问题原文不再进 chat 流，原先暴露的「卡片正文不渲染 markdown」在已答/未答两条路径上同时消失。但**开放态与只读态仍用裸 `{question}`**（[InlineInteractionCard.tsx:172](../../../apps/acowork-desktop/src/components/chat/InlineInteractionCard.tsx#L172)、[:192](../../../apps/acowork-desktop/src/components/chat/InlineInteractionCard.tsx#L192)），模型若把 markdown 写进问题，开放卡仍会显示原始语法。这条仍然遗留，需要先把 `ChatMarkdown` 从 `AgentChatView` 局部组件提取为共享件，属结构改动。
