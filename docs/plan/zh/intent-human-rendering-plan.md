# Intent 人类可读渲染计划（display / machine envelope 双格式）

> 版本：v1.0 | 日期：2026-11-09
>
> **一句话**：agent↔agent 的 intent 与 intent response 在聊天列表里显示成 `[intent:<id>:<action>] {"...":"..."}` 这样的 JSON 源码，因为**模型提示材料**与**人类展示文本**长期以来是同一个字符串；本计划把两者拆成两条正交的产物——`content` 继续作为机器信封交给模型，新增 `display` 投影随 user 行的 `metadata.intent` 落库、经 `RoundProjection.user.display` 透传到桌面与移动端，让所有画像素的表面读 markdown，让程序继续读 JSON。
>
> 前置调查已完成，证据均带 file:line（见 §1）。拆成 M1–M5 五个可独立交付/测试的模块（§4），§6 记录实施进度。

---

## 1. 缺陷摘要（调查结论）

### 1.1 现象

- SSE.Cassie → SSE.Kevin 发 intent：Kevin 那一侧的执行块标题与正文是 `[intent:2f3c…:analyze] {"title":"…","workspace":"…"}`。
- Kevin 的 response 回到 Cassie：Cassie 侧同样是 JSON 源码，内层 markdown 的换行被转义成 `\n`，整段挤成一行。
- 两侧都是「程序化信息」，没有任何一处人类可读的句子。

### 1.2 链路逐段

1. **发送**：内置工具 `intent_send`（[intent_send.rs](../../../core/acowork-runtime/src/tools/builtin/intent_send.rs)）`POST /api/intents`，`params` 是 agent 自己构造的任意 JSON object。
2. **投递**：Gateway 的 [intent_api.rs](../../../core/acowork-gateway/src/http/intent_api.rs) → [delivery.rs](../../../core/acowork-gateway/src/intent/delivery.rs)（唤醒 + 信箱 + MQTT publish），信封原样传递。
3. **接收**：[inbound.rs:317](../../../core/acowork-runtime/src/agent/inbound.rs#L317) 得到 `InboundMessage::IntentMessage`，[gateway_loop.rs](../../../core/acowork-runtime/src/startup/gateway_loop.rs) 交给 `deliver_intent_turn`。
4. **渲染**：[intent_turn.rs:382](../../../core/acowork-runtime/src/agent/intent_turn.rs#L382) `render_intent_message()` 生成 `[intent:{from}:{action}] {params}` + 坐标行（message_id / conversation_id / source_chat_session_id / repo）。`{params}` 是 `serde_json::Value` 的 `Display`——**单行紧凑 JSON**。
5. **落库**：这一整个字符串作为 `SessionMessage::Intent.content` 进入本轮，既喂给模型，又被当作 **user 行的正文**写进执行会话 JSONL。
6. **读回**：ADR-106 之后聊天里只剩**空 body 的引用行**（`KIND_EXECUTION_ROUND`），正文完全由执行会话 JSONL 投影回来（[rounds.rs:799](../../../core/acowork-runtime/src/rounds.rs#L799) `RoundUserRow` → `RoundProjection.user.body`）。
7. **画像素**：[ChatExecutionBlock.tsx](../../../apps/acowork-desktop/src/components/chat/ChatExecutionBlock.tsx) 的 `questionOf()`、块头命名（`deriveSessionTitle`）、复制导出、`RoundBody` 面板全部只认 `user.body`——于是它们看到的就是第 4 步那行机器信封。
8. **应答方向同理**：[intent_turn.rs](../../../core/acowork-runtime/src/agent/intent_turn.rs) 的 turn 结束时 `post_intent_response` 写回 `POST /api/intents/{request_id}/response`，`params = {"message": <答复 markdown>}`；Cassie 侧 `kind=response` 走同一渲染器，markdown 被 JSON 转义吞掉。

### 1.3 根因（三条，缺一不可）

1. **一个字符串侍奉两个受众**。`content` 的坐标行是设计 27 §3.2 要求的：模型必须知道往哪儿回写。但它同时也是唯一被落库的正文。
2. **人类通道只有一个，且对 peer origin 为空**。`meta.assignment_brief` 由 [intent_turn.rs:336](../../../core/acowork-runtime/src/agent/intent_turn.rs#L336) `reference_name()` / [intent_turn.rs:351](../../../core/acowork-runtime/src/agent/intent_turn.rs#L351) `reference_brief()` 产出，但历史上只对 `pm_task` 和人类来源有值；`AgentRequest` / `AgentResponse` 两个 origin 返回空 → 所有表面回落到机器格式。
3. **ADR-102 §2 只修了一半**。它把 `pm_task` 与人类来源的 `render_envelope` 置 false（[intent.rs:103](../../../core/acowork-core/src/intent.rs#L103)、[intent.rs:112](../../../core/acowork-core/src/intent.rs#L112)），`AgentRequest` / `AgentResponse` 仍是 true（[intent.rs:118](../../../core/acowork-core/src/intent.rs#L118)、[intent.rs:124](../../../core/acowork-core/src/intent.rs#L124)）——这正是本次现象的另一半。

---

## 2. 原则：展示信息与程序化信息用不同格式

| | 程序化信封（`content` / user 行 body） | 展示投影（`display` / `metadata.intent`） |
|---|---|---|
| 受众 | 模型 | 人（桌面 / 移动 / 导出 / 引用） |
| 格式 | `[intent:…] <compact JSON>` + 坐标行 | markdown |
| 载体 | user 行 body（JSONL `content`） | user 行 `metadata.intent`（JSONL `metadata`） |
| 生成点 | 接收侧的投递边界（唯一持有信封处） | 同一处，同一函数调用点 |
| 语义 | 可被程序解析、可被模型回写 | 只被渲染，不参与任何契约 |

两条产物同源同点生成，因此不可能漂移；展示投影缺失时（人类 origin，或 params 里没有任何可打印内容）各表面行为与今天一致——诚实的降级，而不是空面板。

---

## 3. 三个已确认的决策

**D1 — 展示文本存 user 行的 `metadata.intent`，不改 body。**
body 是模型输入与（ADR-106 前的）会话契约，改它会污染 prompt 与所有下游解析器。metadata 是 JSONL 里本就存在的旁路字段，[conversation.rs](../../../core/acowork-runtime/src/conversation.rs) 的 `append_message_with_id` 已支持写入，旧行天然没有该字段 → **零迁移**。代价：展示文本与 body 各占一份空间，用 D2 的上限兜住。

**D2 — `display.body` 上限 16KB + `truncated` 标记。**
一个 response 的 `message` 可以是整篇答复。投影是给「一眼看懂」的，不是给全文归档的——全文永远在机器信封里。超限时截断并置 `truncated: true`，桌面/移动在正文末尾渲染一行提示（i18n key `displayTruncated`），用户知道「还有更多，且原始信封完整」。

**D3 — 请求侧没有 `message` 字段时，回落到 `params → markdown 列表`。**
`response` 的载荷有约定键（`message`），原样当 markdown 渲染。`request` 没有约定键，agent 会传 `{"title":…,"workspace":…,"scope":…,"requirements":…}` 这类结构；若整段塞成一行，就是「JSON 问题换了件衣服」。规则：`title` 类键作标题行，`message`/`content`/`body` 类键原样作正文（它通常已是 markdown），**其余键逐条渲染成 markdown 列表**。`summary` 取 title，没有 title 就取正文首句，再没有就用 action。

---

## 4. 模块

**M1 — 展示渲染器（core，纯函数）** ✅
[intent_turn.rs](../../../core/acowork-runtime/src/agent/intent_turn.rs) 新增 `IntentDisplay { origin, action, summary, body, truncated }` 与 `render_intent_display(kind, action, params) -> Option<IntentDisplay>`：`render_envelope == false` 的 origin 返回 `None`；`AgentResponse` 取 `params.message`；`AgentRequest` 按 D3。纯函数，无 IO，单测覆盖每个 origin、每种 params 形状与截断边界。

**M2 — 写路径（runtime）** ✅
`SessionMessage::Intent` 增 `display: Option<IntentDisplay>`；`AgentLoop` 增 `turn_user_row_meta: Option<Value>`，只在 user 行 `append_message_with_id` 时消费一次（[loop_.rs:1256](../../../core/acowork-runtime/src/agent/loop_.rs#L1256)）；投递边界计算 display 并同时喂给 `reference_name()`（块头命名）与 user 行 metadata。

**M3 — 读路径（runtime → Gateway）** ✅
[rounds.rs:667](../../../core/acowork-runtime/src/rounds.rs#L667) `RoundUserRow.display: Option<RoundUserDisplay>`，`from_entry` 解析、`project()` 透传；旧行 `None`，各表面回落。Gateway 侧不改：`chat_blocks` 只搬运投影，display 原样过。

**M4 — 桌面渲染** ✅
[chatBlocks.ts](../../../apps/acowork-desktop/src/lib/chatBlocks.ts) 的 `RoundProjection.user.display?`；[ChatExecutionBlock.tsx](../../../apps/acowork-desktop/src/components/chat/ChatExecutionBlock.tsx) 的 `questionOf()` 回落链改为 `display.summary → display.body → body`，块头命名与复制导出同改；`RoundBody` 展开面板用 `ChatMarkdown` 渲染 `display.body`，机器信封收进 `<details>`（`rawEnvelope`）。

**M5 — 移动端** ✅
[types.ts](../../../apps/acowork-mobile/src/lib/types.ts) / [chatBlocks.ts](../../../apps/acowork-mobile/src/lib/chatBlocks.ts) 加 display；[ChatExecutionBlock.tsx](../../../apps/acowork-mobile/src/components/ChatExecutionBlock.tsx) 同桌面逻辑（`questionOf` / 块头命名 / `RoundBody` 面板 / `<details class="chat-run-raw">`），样式见 [app.css](../../../apps/acowork-mobile/src/styles/app.css)。

---

## 5. 验证

- `cargo clippy -p acowork-runtime --all-targets -- -D warnings`：通过。`SessionMessage` 的 `clippy::large_enum_variant` 在枚举上显式 allow，理由写在属性上方（channel 消息、每次投递克隆一次、Intent 信封宽度是设计 27 §3.2 的固有形状；Box 只是把同样字节搬到堆上并让读者看不见信封）。
- `cargo test -p acowork-runtime --lib`：1960 passed / 0 failed。
- 桌面 `npx vitest run src/components/chat`：61 files / 588 tests passed；`npm run check:i18n`：1869 调用点全部解析。
- 移动端 `npx tsc --noEmit`：通过。
- 桌面 `npx tsc --noEmit` 仅剩 editor/* 的 9 条「缺少可选依赖（pdfjs-dist / jszip / xlsx）」既有环境错误，与本改动无关。

---

## 6. 实施进度

| 模块 | 状态 | 备注 |
|---|---|---|
| M1 | 已完成 | 单测 31 条 |
| M2 | 已完成 | `turn_user_row_meta` 一次性消费 |
| M3 | 已完成 | 旧行零迁移 |
| M4 | 已完成 | `ChatExecutionBlock.test.tsx` 覆盖 display 回落与 raw envelope 折叠 |
| M5 | 已完成 | 与桌面同一回落链 |

---

## 7. 非目标与后续

1. **不改 intent 的 wire 契约**。`params` 仍是 agent 自定的 JSON；本计划只在接收侧做投影，不要求发送方遵守任何新 schema——否则协议变更会波及所有 agent 包。
2. **不做发送侧预览确认**（「你的 agent 将要发出这段文字」）。属于产品决策，与本缺陷无关。
3. **展示投影不参与任何契约**：没有它就回落，有它也不进 prompt。任何让它影响模型输入的改动都视为回归。
