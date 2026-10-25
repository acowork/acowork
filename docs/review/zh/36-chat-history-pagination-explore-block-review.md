# 聊天记录分页机制分析：explore 块 >100 steps 时首屏不满屏

任务：`t-083ae242` · 项目：`p-66e68d1a`

## 结论（先说结果）

用户的猜测**方向正确但不是根因**。真实原因不是"后端按消息条数分页、前端按块显示"这个维度不一致本身
（那只是**症状的放大器**），而是：

> **首屏走的是 `openSession` 的裸 tail 加载路径，它在 `ensureLatestInCache` 之前就把
> `tailCovered` 短路的条件满足了，导致 `expandWindowToTurnBoundary` 根本没跑。**
> 首屏因此停在一个**被截断的 explore run 中段**——窗口里 50 条原始 entry 全部属于同一个
> explore run，折叠后只剩 1 个 explore chip + 1 条 assistant，屏幕自然是空的。

复现数据（真实 store 代码 + 虚拟后端，vitest 探针，已删除）：

| 场景 | HTTP 次数 | offset | 原始条数 | 折叠后 blocks | 首条 |
|---|---|---|---|---|---|
| A 裸 tail（`openSession` 路径） | **1** | 252 | 50 | 2（1 chip + 1 assistant） | `tool_call` ← 截断 |
| B `ensureLatestInCache`（正确路径） | 7 | 0 | 302 | 3（user + 1 chip + assistant） | `user` ✅ |

同一个 302 条 session，两条路径差 6 次 HTTP 往返，差一整个 user 消息。

## 根因链

### 1. 后端按 raw entry 分页（设计如此，非 bug）

`core/acowork-runtime/src/conversation.rs:3339` `read_messages_paginated`：
`offset` / `limit` / `total` 单位都是 **raw JSONL entry**（一行一条：user / assistant /
thought / tool_call / tool_result）。

`core/acowork-runtime/src/conversation.rs` 的注释明确说明这是**刻意决策**：
"Group-aware pagination (slicing by display groups) would require every protocol hop to
recompute group boundaries — a hidden coupling that reintroduces the partial-cut bug family."

即：后端**故意不认识** display group。ADR-050 §正向索引也把 `offset` 语义冻结为 raw entry。

**所以"后端分页 vs 前端折叠"这个维度不一致是设计意图，不是 bug**，不能靠改后端分页语义来解决
（那会推翻 ADR-050 并把 display 概念泄漏进 Runtime）。

### 2. 前端折叠把一个 explore run 压成 1 个 block

`apps/acowork-desktop/src/components/chat/messageFolder.ts` `foldMessages`：
连续的 `tool_call` / `tool_result` / `thought` → 1 个 `explore_group` block。

一个 100+ steps 的 explore run ≈ **300+ 条 raw entry**（每 step 至少 tool_call + tool_result，
实测最大 617 条 / 205 steps）。

### 3. **真正的 bug：首屏路径绕过了 turn-boundary 展开**

`apps/acowork-desktop/src/stores/chatStore.ts:1849` `openSession` → `loadSessionMessages(agentId, sessionId)`
（`offset === undefined` → `?tail=true&limit=50`）。

`apps/acowork-desktop/src/stores/chatStore.ts:2879` `ensureLatestInCache` 里有一个 `tailCovered` 短路：

```ts
const tailCovered =
  !messagesStale &&
  messageTotal > 0 &&
  messageOffset + messageLimit >= messageTotal &&
  messages.length > 0;
if (tailCovered) return;   // ← 直接返回，跳过下面的 expandWindowToTurnBoundary
```

`openSession` 的裸 tail 加载已经把 `messageOffset + messageLimit` 推到 `>= messageTotal`，
所以当 ChatPanel 的 `loadInitialPage()` → `ensureLatestInCache` 随后跑时，**短路命中，
`expandWindowToTurnBoundary` 被跳过**。

探针 E 直接证实：openSession 的 tail load 落地后 offset=252 / limit=50 / total=302
（252+50=302 >= 302），`ensureLatestInCache` 立刻 return，**首条仍是 `tool_call`（截断）**。

### 4. 为什么"看起来满屏了"但其实没满

`apps/acowork-desktop/src/components/chat/useScrollController.ts:561` 的 viewport-fill 循环
（`MAX_FILL_PAGES = 10`）会在内容不足一屏时继续 `loadPrevPage`。它**能**救回来（探针 H：
再花 6 次 HTTP 拉到 offset=0 / 302 条 / 首条 user），但：

- 恢复过程是**逐页向前爬**，每页 50 条 raw entry，一个 300 条的 explore run 要 6~7 次往返；
- `expandWindowToTurnBoundary` 的 `MAX_EXPAND_PAGES = 20` 上限意味着单次 explore run
  超过 1000 条 raw entry 时**永远拉不回 turn 边界**（探针 C：1202 条 session 花了 21 次 HTTP，
  最终 offset=152 / 首条仍是 tool_call，**依然是截断的**）；
- 填充循环按"页"计数，而一"页"可能折叠成 1 个 block，**填充效率与视觉填充量脱钩**。

### 5. 还有一个更隐蔽的放大器

`apps/acowork-desktop/src/components/chat/blockHeightEstimator.ts:311`：
`explore_group` 的估算高度是 `EXPLORE_HEADER_HEIGHT(32) + 4 + EXPLORE_CONTENT_MAX_HEIGHT(144)`，
**与 item 数量无关**（内容区硬上限 144px，内部滚动）。
`ExploreBlock.tsx:413` 同样是 `maxHeight={144}`。

所以一个 100+ steps 的 explore 块**在屏幕上最多占 180px**。它不是"高度很大导致溢出"，
而是"steps 很多但视觉高度恒定"——这正是"下边其实只有一条，根本不满一屏"的物理原因。
同时折叠状态下只占 32px。

## 真实数据佐证

扫了本机 `~/.acowork/acowork-node/packages/*/workspace/conversations/*.jsonl`（222 个 session）：

- 最大的 explore run：**617 条 raw entry（205 steps）**，其次 492 / 456 / 410 / 351 / 318 / 308；
- 12 个 session 存在 **>100 steps** 的 explore run（即用户报告的场景）；
- 1 个 session 的 explore run 超过 500 条 raw entry —— **超出 10×50 的 fill 预算**；
- 9 个 session 的 tail-50 折叠后只剩 ≤2 个 block（1 个 explore chip + 1 条 assistant）
  —— **这就是"首屏不满屏"的精确形态**。

## 方案评估

约束：ADR-050（正向 raw-entry offset、历史/实时分离、Adapter 数据驱动）不可推翻；
Runtime 不认识 display group（display 概念不能泄漏进后端）。

### 方案 A：后端改成按 display group 分页 ❌
违反 ADR-050 + 把 UI 概念泄漏进 Runtime + 重开 ADR-050。**否决。**

### 方案 B：纯前端——首屏统一走 `ensureLatestInCache` ✅ 最小、最对症
把 `openSession` 的裸 tail 加载改成走 `ensureLatestInCache`（它已经内含
`expandWindowToTurnBoundary`），让**首屏只有一条加载路径**，`tailCovered` 短路不再有机会
抢跑。同时把 `expandWindowToTurnBoundary` 的 `MAX_EXPAND_PAGES` 上限从"页数"改成
**"直到 offset=0 或首条是 turn 边界"**，去掉 1000 条的隐式天花板。

- 优点：改动集中在 chatStore，不动协议、不动 ADR、不动后端。
- 缺点：一个巨型 explore run 仍要 6~7 次 HTTP 往返才能拉到边界（`ensureLatestInCache`
  路径实测就是 7 次）。

### 方案 C：后端提供 turn 边界游标（协议扩展）⚠️ 更彻底但要动协议
让 Runtime 在返回窗口时**额外**给出"这个窗口所在的 turn 的起始 raw offset"
（Runtime 其实知道每条 entry 的 role，判断 turn 边界是它能做的、不涉及 display group 概念——
turn 是**对话语义**，不是 UI 概念）。前端一次请求就能直接对齐边界，把 7 次往返降到 1 次。

- 优点：一次往返解决；`expandWindowToTurnBoundary` 的逐页爬行可以退役。
- 缺点：要动 Runtime + Gateway 代理 + ADR-050 补充条款 + 前后端协议。

### 方案 D：把 explore 块的视觉高度放开 ❌
`maxHeight={144}` 改成不限制，会让一个 205 steps 的块变成几万像素高的 DOM，
虚拟列表的测量/锚定全部受影响。**否决。**

## 建议

**先做方案 B（前端收敛首屏路径 + 去掉 1000 条隐式天花板）**——它直接命中报告的症状、
不动协议、不动 ADR-050，且能顺带消掉"同一 session 两条首屏路径结果不同"这个更根本的不一致。

方案 C（后端 turn 边界游标）作为后续优化：它把"巨型 explore run 要 7 次往返"降到 1 次，
但需要改协议，应单独立项/ADR。

配套建议：`MAX_FILL_PAGES` 的语义应从"页数"改为"**折叠后的 block 数 / 视觉高度**"，
否则填充预算和用户真正看到的画面仍然脱钩。

## 复现方法

（临时探针已清理；重建方式见下）
1. mock `GET /api/agents/{id}/sessions/{sid}/messages`，按 `offset`/`limit`/`tail` 返回
   构造的 raw entry 序列：`[user, tool_call×300, assistant]`（302 条）。
2. 调 `useChatStore.getState().loadSessionMessages(AGENT, SESSION)`（= openSession 的尾页路径）。
3. 观察：`messageOffset=252 / messageLimit=50 / messageTotal=302`，
   `messages[0].type === "tool_call"` —— **截断**。
4. 改调 `ensureLatestInCache` 对比：7 次 HTTP，offset=0，首条 `user` —— 正确。

## 实施记录（2026-10-25，t-083ae242）

**实际落地的不是上面的方案 B，而是"配套建议"那一句**——`MAX_FILL_PAGES` 的语义被彻底删掉，
改成**按游标是否有进展**来判定，文件：`apps/acowork-desktop/src/components/chat/useScrollController.ts`。

### 为什么不做方案 B

方案 B 让 `openSession` 在**渲染之前**串行跑完 `expandWindowToTurnBoundary`。对 617 条的
explore run 那是 13 次阻塞请求，首屏白等；而填充循环本来就能达到同一个终态，只是**边渲染
边加载**。所以首屏路径不一致不需要单独修——把填充循环的终止条件修对，它自然覆盖。

### 改了什么

删除 `MAX_FILL_PAGES`（页数预算），改为"每个不同的窗口状态最多发一次填充请求"：

```ts
const cursor = `${a.messageOffset}:${a.messageLimit}`;
if (direction && cursor !== fillWindowRef.current) { … }
```

三个后果：

1. **巨型 explore run 不再被页数掐断**。617 条 / 205 steps 的 run 需要 12 次翻页才能跨过
   turn 边界，旧预算 10 次就停了——回归测试里旧实现精确停在 `offset=67`，剩 67 条 / ~22 步
   永远加载不到，正是用户报的现象。
2. **预算耗尽不再顺手关掉整个面板的分页**。旧代码 `return` 在 near-top / near-bottom 两个
   分支**之前**，而内容不足两视口时 `scrollTop` 恒为 0，于是"用户滚到顶也加载不了"，且计数
   只在 `range >= clientHeight` 时归零——一个填不满屏的会话等于**永久禁用分页**。
3. **失控保护从"页数"变成"进展"**：请求发了但游标没动（空响应 / 失败 / 重复页）就不再重试，
   不会按 rAF 频率空转；游标动了就自动重新武装。

### 顺带得到的性质

"补齐 turn 边界"不用再单独实现。窗口头部是**被截断的** explore run 时，补它的内容零高度增长
（`ExploreBlock` 内容区 144px 硬上限），所以视口永远填不满 → 填充循环必然继续跑，直到跨过
turn 边界出现新 block（头部唯一能增加高度的事件）。判据不需要读 `blocks[0].type`，
也不需要后端给组边界。

### 仍然存在的边界（诚实记录）

- 若整个会话折叠后**本来就不足一屏**（例如一条 617 条的 run + 一条 user），加载到 `offset=0`
  后屏幕仍不满。这是内容事实，不是 bug；要填满只能放开 `maxHeight`（方案 A），代价是长
  explore 展开后几千行不可用——**已明确否决**。
- 中途恢复浏览位置（`loadPageForBlockId`）时窗口尾部可能被截断，此时 `hasNewer` 优先，
  不会去补头部。该路径本就不做边界对齐，维持现状。
- `expandWindowToTurnBoundary`（`MAX_EXPAND_PAGES = 20`）保留未动：它现在是 `ensureLatestInCache`
  / `scrollToBottom` 的**抢跑优化**，不再是唯一机制；超过 20 页的部分由修正后的填充循环兜住。

### 验证

`useScrollController.test.ts` 新增 2 例（`viewport fill` 组）：

- 617 条 / 单块恒 180px 的虚拟后端：断言翻到 `offset=0`、请求数 **> 10**（旧实现失败于
  `expected 67 to be +0`），且数据取尽后不发第 13 次。
- 适配器零进展：断言只发 1 次请求，边缘分支不得偷发第 2 次。

`npx tsc --noEmit` 干净；全量 `vitest run` 196 files / 1784 tests 通过。

