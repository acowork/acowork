# 聊天视图渲染性能治理计划（chat transcript render perf）

> 版本：v1.0 | 日期：2026-10-26
>
> **一句话**：聊天会话（Chat surface）打字卡顿 ~2s 的根因是「按键 → 整棵 transcript 全量重渲染 + react-markdown 同步全量重解析」；分三层治理——L1 memo 安全网（当天可发）、L2 通用虚拟化壳、L3 store 引用纪律。每层独立交付、独立回滚。
>
> 前置调查：本计划基于已完成的只读根因分析（证据均带 file:line，见 §1）。

---

## 1. 根因摘要（调查结论，非本计划实施内容）

**症状**：聊天界面消息输入框增删字符延迟 ~2s 才上屏；记忆面板/历史会话搜索框流畅；执行会话（exec surface）输入框流畅；chat surface 在 idle 与 working 都卡（working 更频繁触发）。

**根因链（代码事实）**：

1. Chat surface 按键路径 = 本地 `useState` → 宿主组件整树 re-render → `rows.map` 全量渲染：
   - `AgentChatView.tsx:880`（onChange）→ `:392`（useState）→ `:752`（rows.map，无虚拟化）
   - `GroupChatPanel.tsx:1110-1116` → `:946`（同构，**同一 bug 已复制两份**）
2. 行渲染器全部无 memo：`RowView`（AgentChatView.tsx:110）、`GroupRowView`（GroupChatPanel.tsx:208）、`ChatMarkdown`（ChatMarkdown.tsx:115）、`InlineInteractionCard`（:114）。
3. react-markdown v10 默认导出为同步 `Markdown`，每次渲染全量 `parse+runSync`，库内无 memo（`node_modules/react-markdown/lib/index.js:175-179`）。
4. 成本 ∝ 已加载行数且只增不减：PAGE=50（chatSessionStore.ts:198）+ 滚动自动加载（AgentChatView.tsx:715-723）。
5. working 态放大器：user service 每次 append 发 doorbell（`core/acowork-user/src/chat_sessions.rs:809,827`）→ `wake()` 全量重拉所有打开窗口（chatSessionStore.ts:855-867）→ `refresh()` 无内容比较、恒 `set` 新引用（:689-693）→ 每次 append 触发一次全量重渲染，**即使 exec 在后台**。
6. exec surface 不卡的原因：VirtualMessageList 虚拟化（VML.tsx:269-275）+ MessageBubble/StreamMarkdown memo（MessageBubble.tsx:53,88）。
7. 历史背景：design 27 v1 刻意不做流式，`chatSessionStore.ts:17-20` 留有 `ponytail:` ceiling 注释——假设「doorbell 低频 → 全量渲染无所谓」，漏算了 3×4×5 的乘积。

**已排除**：网络阻塞（记忆搜索每键发 HTTP 反而流畅；输入链路无请求）、debounce/定时器、hljs 重跑（CodeBlock.tsx:84 有 memo）。

**判断依据可复核**：devtools console 的 `[longtask]`（探针已装，main.tsx:12-13）+ React Profiler 按键 commit。

---

## 2. 目标与非目标

**目标**
- G1：chat surface 任意规模的已加载 transcript 下，按键到上屏 < 50ms。
- G2：working 态后台会话的 doorbell 不再造成主线程可感知占用。
- G3：把「列表渲染性能」从每个 surface 的自觉，变成共享件的结构保证——新 chat-like surface 默认走虚拟化路径。

**非目标**
- 不做 chat surface 流式渲染（design 27 的 v1 取舍维持；升级路径另立计划）。
- 不动 exec surface 现有行为（VML 重构仅限抽取，不改语义）。
- 记忆面板搜索 debounce：记录为同类观察项，本期不做（行轻、未爆）。

---

## 3. 分层方案

### L1 — 叶子 memo 安全网（~0.5d，当天可发）

> 定位：这是「膏药」也是**必须的安全网**——任何 surface 将来再忘优化，代价从「秒级卡死」降为「一次廉价 diff」。L2/L3 的落地不改变 L1 的价值。

**改动**
1. `ChatMarkdown` 包 `React.memo`，比较 `content` 字符串 + `components` 引用。
   - 前提核查：调用方 components 均为模块常量（GroupChatPanel.tsx:90 `chatBlockComponents` ✓）。
   - 文件：`src/components/chat/ChatMarkdown.tsx`。
2. `RowView` / `GroupRowView` 包 `React.memo`；父侧不稳定 props 收敛：
   - `handleAnswer`（AgentChatView.tsx:539）、`jumpToExec`（:676）→ `useCallback`；
   - GroupChatPanel 对应回调同法；`triggerBody`/`replyQuote` 已是稳定 Map 派生（:780-784），确认引用不变即可。
3. 回归测试（本计划的验收骨架，锁两个 surface）：
   - 新增 `AgentChatView.memo.test.tsx`：渲染 N 行 → 模拟一次输入 `setInput` → 断言 RowView 渲染次数为 0（memo 命中）；
   - `ChatMarkdown.test.tsx` 补一条：同 content 重渲染父级，ReactMarkdown 解析调用不增加（可用 spy `remark` 管道入口或渲染计数）。

**验收**
- 长会话（≥3 页历史，翻满后 idle）打字，console 无新增 `[longtask] ≥250ms`；主观 <50ms。
- 单测绿；`npm run test`（desktop vitest）无新告警。

**回滚**：单 commit revert，无数据/契约变更。

---

### L2 — 通用虚拟化壳 `VirtualRowList<T>`（~2-3d）

> 定位：举一反三的架构答案。问题类别 = 「N 个 surface 各自重新发明列表」；解药 = 壳复用。VML 不可直接照抄的原因（调查已证）：其 props 焊死在 exec 数据模型（adapter/MessageBlock/toolProgress/isThinking 等，VML.tsx:31-94），而 chat surface 是 seq 窗口 + ChatMessageRow。**抽的是「只渲染视口 + 测量 + prepend 锚定」这 150 行机制，不是行渲染器。**

**改动**
1. 从 VML 抽取泛型壳 `src/components/common/VirtualRowList.tsx`：
   - Props：`items: T[]`、`keyOf(item)`、`renderItem(item, index)`、`estimateSize?(item)`、`overscan`、`scrollContainerRef`、`prependSignal`、命令式 handle（first/last visible index/key）。
   - 内含：`useVirtualizer`（@tanstack/react-virtual，**零新依赖**）+ `measureElement`/ResizeObserver 纪律（参照 VML.tsx:315 起）+ 高度估算缓存（参照 blockHeightEstimator.ts，按 key 泛化）。
   - 不含：任何行类型语义、任何 store 订阅。
2. VML 改为壳上的第一个适配器（行为不变；exec 的滚动快照/恢复/locate 等留在 VML 层）。
3. `AgentChatView`、`GroupChatPanel` 的列表段改为壳上的第二、三个适配器：
   - 行高估算：chat 行按 body 长度粗估（壳的 `estimateSize` 缺省 = 固定值亦可，measure 后自动校正）；
   - `stickToBottom` 与壳的协作保持现状（prependSignal 语义已有，stickToBottom.ts:207-226）；
   - 高亮定位（`pendingLocateSeq` / `data-seq`）经 handle 查询实现，行为对齐现状。
4. 结构保证：壳的 README 注释 + AGENTS.md 加一行约定——「消息/长列表 surface 必须经 VirtualRowList；直接 `.map` 全量渲染需在 review 说明理由」。

**验收**
- exec surface 全部现有滚动/分页/locate 回归测试绿（scrollSnapshot、bottomAnchor、useScrollController、chatListAdapter 等既有用例）；
- chat surface 新增用例：加载 500 行窗口，DOM 中挂载行数 ≤ 视口行数 + overscan；滚动到顶触发 loadOlder 后阅读位置不漂移（prepend 锚定）；
- 长会话按键 <50ms（L1 已达成，此处验证不劣化）；
- 手动：exec 会话切回/前滚后滚/流式跟随与改前无肉眼差异。

**风险**：VML 抽取回归面最大（滚动快照、session 切换白屏等历史 bug 都在这条线上）。缓解：壳抽取为**纯搬移**（不改逻辑），单独 commit，先只接 chat surface，VML 切换放后一个 commit，可独立 revert。

---

### L3 — store 引用纪律（~1d）

> 定位：修「触发频率」维度，与 L2 正交。L2 之后 no-op 更新只造成廉价 diff，但省掉无谓的 virtualizer 重测与网络仍有价值；且该约定防住所有「identity 变更 → 订阅者重渲染」类问题。

**改动**
1. `chatSessionStore.refresh()`：拉到与当前窗口内容等价（`total` 不变 + 末条 `seq`/`id` 不变 + 非 force）时**不 set**（chatSessionStore.ts:681-693 现恒产生新 `windows` 引用）。
2. `wake()`（:855-867）：对连续 doorbell 合并去抖（~250ms 窗口），多 session 重拉并行化（现为串行 `await` 循环）。
3. 约定入文：AGENTS.md / 模块设计文档补「identity-preserving update：store 写路径内容未变不得产生新引用」。
4. 债务账本：更新 `chatSessionStore.ts:17-20` 的 `ponytail:` ceiling 注释——注明升级路径已执行为 L1+L2+L3（不引入流式，流式仍是未做项），避免下一个 surface 作者再按 v1 假设写全量渲染。

**验收**
- 单测：refresh 内容等价时 `windows` 引用不变；wake 连发 5 次只重拉一轮；
- working 态（exec 后台跑 turn）在 chat surface 打字，`[longtask]` 频率与 idle 基线一致；
- 既有 doorbell 相关测试（unread badge、locate）绿。

---

## 4. 顺序与工作量

| 阶段 | 内容 | 工期 | 独立发布 |
|---|---|---|---|
| L1 | memo 安全网 + render-count 测试 | 0.5d | ✅ 当天 |
| L2 | VirtualRowList 壳 + 三适配器 | 2-3d | ✅（再拆：chat 接入 / VML 切换两个 commit） |
| L3 | store 引用纪律 + 文档/债务登记 | 1d | ✅ |

总计 ~4-5 人日。L1 先发的理由：立即止血用户可感问题，且 L2 的验收断言（按键成本）依赖 L1 的 memo 语义先稳定。

## 5. 度量基线（实施前后各测一次）

- 场景 S1：chat 会话加载 5 页历史（~250 行）idle，输入 1 字符 → 上屏延迟（Performance 面板量 longtask）。
- 场景 S2：同会话，exec 后台跑一个含多 tool 调用的 turn（doorbell 连发）→ 同 S1 测量。
- 场景 S3：exec surface 流式输出中切到 chat tab → S1 测量（验证后台轰炸已止）。
- 通过线：S1/S2/S3 均 <50ms，console 无 ≥250ms longtask 与按键时刻重合。

## 6. 同类观察项（本期不做，留档）

- 记忆面板搜索每键发 HTTP 无 debounce（MemoryPanel.tsx:129-130）——行轻未爆，L3 纪律推广时的候选。
- ChatPanel.tsx:348 断连横幅 1s interval、ExploreBlock.tsx:621 interval——影响面小，暂不动。
- chat surface 流式（design 27 ceiling 的另一半）——独立需求，另立计划。

## 7. 实施记录（L1/L2/L3 已落地）

| 层 | commit | 结果 |
|---|---|---|
| L1 | `b0fa2876` | ChatMarkdown / RowView / GroupRowView 包 React.memo；handleAnswer/jumpToExec 收敛为 useCallback，调用点内联 lambda 经稳定引用（onJumpToExecStable）传入。新增 render-count 回归测试 3 个（keystroke 后行渲染次数为 0）。 |
| L2 | `fdef0a3b` + `cca57368` | 抽通用壳 components/common/VirtualRowList.tsx（virtualizer + measureElement/ResizeObserver 纪律 + 模块级 measured-height cache + bottom-pin convergence + imperative handle）；AgentChatView/GroupChatPanel 接入（session-qualified key、key 重挂载、useLocateMessage 加 scrollToKey 兜底）。 |
| L3 | `ab7f2118` | refresh 等价门控（append-only + 行不可变 → total+末条 seq 即内容指纹，非 force 等价则不 set）+ 行级结构共享 shareRowIdentities（按 seq 复用旧 row 对象）；wake 250ms 去抖合并 + 并行重拉。新增 chatSessionStore.refDiscipline.test.ts（引用不变断言）。 |

**spike 结论（L2）**：VML 不包含 prepend 锚定——scrollHeight-delta 锚定在 scroll-container 层，由 useStickToBottom 的 prependSignal（chat）与 useScrollController 的 prepend detection（exec）各自实现；虚拟化下 scrollHeight = virtualizer.totalSize，两者继续有效。故壳不含 prepend 锚定，exec 面迁移另立计划。

**实施中新发现的壳加固（cca57368）**：React 19 下父组件的 scroll-container ref 在子组件 layout effect 之后才挂载，virtualizer 首帧发现不到 scroll element 且不再复查，导致白屏。三处修复：late-ref 自愈（每元素身份一次被动重渲染）、viewport 测量 offsetHeight 为 0 时回退 clientHeight、已挂载但零高容器全量渲染（回退到非虚拟化行为，杜绝白屏）。

**约定入文**：AGENTS.md 补两条——「chat transcript 渲染已 windowed+memoized（L1-L3）」与「identity-preserving store 更新」。

**度量验收（§5 S1/S2/S3）待真机执行**：本环境为 jsdom，longtask/上屏延迟需桌面真机用 mainThreadProbe（lib/mainThreadProbe.ts）复测。
