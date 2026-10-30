# chat session 附件一等公民计划（context + presentation parity）

> 版本：v1.0 | 日期：2026-10-28
>
> **一句话**：chat session 的附件在链路上是二等公民——上下文侧 router 看不见本轮附件（元数据只进 Block B 历史投影，不进 Block C），展示侧 desktop 是灰色文字注释、mobile 是 `📎 filename` 纯文本，两侧都不可点击。本计划把「附件元数据」提升为本轮上下文的一等字段，并把 chat 附件胶囊化、打通「打开文件」，拆成 7 个可独立交付的模块：M1–M4 为后端上下文，P1–P2 为 desktop 展示，P3（mobile）单列。
>
> 前置调查已完成，证据均带 file:line（见 §1）。本计划**只做设计与排期**，代码未改动。

---

## 1. 缺陷摘要（调查结论）

### 1.1 上下文侧：router 看不见本轮附件

Chat Router 一轮请求由三块组成：Block A（system prompt）、Block B（历史投影）、Block C（本轮视图）。附件元数据只走通了 Block B。

1. **Block C 只装文本**：[runner.rs:403](core/acowork-runtime/src/agent/chat_router/runner.rs#L403) `user_message: text`；`text` 来自 [runner.rs:208-210](core/acowork-runtime/src/agent/chat_router/runner.rs#L208) 的 `params["message"]`。
2. **`TurnContext` 根本没有附件字段**：[context.rs:480](core/acowork-runtime/src/agent/chat_router/context.rs#L480) 只有 surface / speaker / now / draft / candidates / task / history_available / user_message，`render()`（[:499](core/acowork-runtime/src/agent/chat_router/context.rs#L499)）逐行拼接这些字段。
3. **Block B 才有附件行**：[context.rs:359](core/acowork-runtime/src/agent/chat_router/context.rs#L359) `prefix_body` → [context.rs:374](core/acowork-runtime/src/agent/chat_router/context.rs#L374) `attachments_line`，格式 `[attachments] name (mime, size)`（design 29 §4.1）。
4. **而本轮消息被排除在 Block B 之外**：[runner.rs:360-363](core/acowork-runtime/src/agent/chat_router/runner.rs#L360) 按 `message_id` / `in_reply_to` 排除。于是本轮附件在整个上下文里**没有任何落脚点**。
5. **提示词也没提附件**：[chat_prompt.rs:28](core/acowork-core/src/chat_prompt.rs#L28) 的 `CHAT_ROUTER_BASE_PROMPT` 与两个 scenario 段里 `attach` 出现 0 次。
6. **设计文档先漏的**：design 29 §5.1 的 Block C 字段表、§5.3 的渲染示例都无附件行；§4.1 只规定了历史投影的附件格式。所以这不是实现时漏写，而是设计层缺口——代码严格照文档实现，必然如此。
7. **信封只带 id**：[intent_api.rs:1639](core/acowork-gateway/src/http/intent_api.rs#L1639) / [:1660](core/acowork-gateway/src/http/intent_api.rs#L1660)、[delivery.rs:150](core/acowork-gateway/src/intent/delivery.rs#L150) —— 元数据和字节都不上 MQTT，这是 ADR-046 / design 27 §4.2.1 的有意设计，不是缺陷。

**三种可见后果**：

- 图文混合：判断依据退化为纯文本，「图里是什么」（报错截图、UI 问题）不参与路由。
- 纯附件：`[user message]` 是空串（[runner.rs:214](core/acowork-runtime/src/agent/chat_router/runner.rs#L214) 明确允许 text 为空，且无占位文案），模型极易当成空消息/闲聊而不路由。
- 不对称偏差：历史轮次的附件有 `[attachments] …` 行，本轮没有——router 会引用旧截图，却看不见刚附上的那张。

**同根因的副作用（降级路径）**：[gateway_loop.rs:1397-1415](core/acowork-runtime/src/startup/gateway_loop.rs#L1397) 仅在 `text` 非空时拼接 chat excerpt，且 `chat_tail_excerpt` 跳过空 body（[runner.rs:1519](core/acowork-runtime/src/agent/chat_router/runner.rs#L1519) 有对应测试）——纯附件轮降级后，执行会话收到的是空消息（附件本身仍通过 `chat_attachment_ids` 送达，但没有任何说明）。

### 1.2 展示侧：不可点击、风格不统一

**Desktop**：[ChatAttachmentChip.tsx](apps/acowork-desktop/src/components/chat/ChatAttachmentChip.tsx) 是 chat surface 唯一的附件渲染件，被 [AgentChatView.tsx:318/443/1172](apps/acowork-desktop/src/components/chat/AgentChatView.tsx#L318) 与 [GroupChatPanel.tsx:612-625](apps/acowork-desktop/src/components/chat/GroupChatPanel.tsx#L612) 共用：

- 文档分支是 `<span class="rounded bg-panel-inset px-2 py-0.5 text-xs">`——无边框、无图标、无 hover、无点击，视觉上就是个文字注释。
- 图片分支是 56×56 缩略图，同样没有点击。
- props 只声明 `{ id, filename, mime }`（没有 `size`），连"能否预览"都判断不了。

**Mobile**：[AgentChatScreen.tsx:703-709](apps/acowork-mobile/src/screens/chat/AgentChatScreen.tsx#L703) 更彻底——`<span className="chat-attachment-chip">📎 {a.name}</span>`（样式 [app.css:1888](apps/acowork-mobile/src/styles/app.css#L1888)），纯文本，不取字节，无图片预览，也没有任何查看器组件。

**执行 session 的对照事实（必须说清）**：[AttachmentChipRow.tsx:98](apps/acowork-desktop/src/components/chat/AttachmentChipRow.tsx#L98) `WorkspaceRefChip` 才是可点胶囊（`button` + `border-border-outer` + `FileText` + hover），点击 → [MessageBubble.tsx:137](apps/acowork-desktop/src/components/chat/MessageBubble.tsx#L137) `openAttachedRef` 在编辑器打开。但它**只对工作区引用**成立：`file_upload` / `image_upload` 在 [openWorkspaceRef.ts:120-125](apps/acowork-desktop/src/lib/openWorkspaceRef.ts#L120) 是显式 warn + 返回。所以「和执行 session 一样」只能对到**样式**，点击语义在 chat 侧是新能力。

**已有但零调用的基础设施**（这是它停在文字注释状态的直接原因）：

| 能力 | 位置 | 现状 |
|---|---|---|
| `downloadChatAttachment`（保存框 + Rust 流式传输） | [user-chat-api.ts:230](apps/acowork-desktop/src/lib/user-chat-api.ts#L230) | 只有单测 |
| `canPreviewAttachment` / `fetchAttachmentText` | [user-chat-api.ts:272](apps/acowork-desktop/src/lib/user-chat-api.ts#L272) | 只有单测 |
| `openAttachmentPreview`（只读 Monaco 标签） | [fileEditorStore.ts:246](apps/acowork-desktop/src/stores/fileEditorStore.ts#L246) | **src 内零调用方** |
| `chatAttachmentObjectUrl`（含 `?thumb=1`） | [chatSessionStore.ts:401](apps/acowork-desktop/src/stores/chatSessionStore.ts#L401) | 仅用于缩略图 |

### 1.3 实证（本机真实数据）

- chat `218dff2a` 第 135 行，`message_id = cc83829e-428e-4aa9-8448-5d0493483edf`，正文为文字 + `screenshot-1793088253750.png (image/png, 134906B)`。
- 同一 message_id 在该 agent 的 `logs/chat-router-decisions.jsonl` 中：`decision=dispatch:new`、`history_len=32`、`latency_ms=19464`、`model=MiniMax-M3.1-Flash`。
- 结论：那一轮 router 拿到的是纯文本，图片的存在、文件名、类型全都不在上下文里。
- 另：本机库中存在 body 为空的附件行（迁移行 4 条 + 若干空正文上传），证明「纯附件轮」是真实存在的输入形态。

### 1.4 已确认无误的部分（避免误修）

- **执行侧是完好的**：[session_task.rs:1520-1640](core/acowork-runtime/src/agent/session/session_task.rs#L1520) 用 `chat_attachment_ids` 经 Gateway 拉字节、materialize 到 agent-home `files/`、图片走多模态 `ContentPart`、其余走 `build_attachment_hint`。
- **id 传递两条路都通**：[runner.rs:655](core/acowork-runtime/src/agent/chat_router/runner.rs#L655)（dispatch）与 [gateway_loop.rs:1615](core/acowork-runtime/src/startup/gateway_loop.rs#L1615)（fallback）。
- 缺陷只在「谁来决定这个活归谁」这一环，以及「用户看到/点到的那个胶囊」。

---

## 2. 目标与非目标

**目标**

- G1：任一路由轮，router 的上下文里包含本轮附件的 `filename / mime / size / kind`，且与历史投影**同格式**。
- G2：纯附件轮不再是空消息：上下文里既有占位文案，也有附件清单。
- G3：chat 附件在 desktop 呈现为可点胶囊，视觉与执行 session 的工作区引用胶囊同族。
- G4：点击有明确语义：图片预览、文本只读预览、其余下载；失败有可见降级。
- G5：本轮与历史对同一行消息的附件投影**逐字节一致**（结构性保证，不靠约定）。
- G6：这类问题在决策日志里可观测。

**非目标**

- NG1：不把附件内容（图片像素、PDF 正文）送进 router——注入面与 token 预算另立计划（见 §5 D5）。
- NG2：不改执行 session 的上传件点击行为（ADR-046 的 provenance 语义维持），仅统一**样式**。
- NG3：不改 `exec_attachments`（ADR-100 D12）的惰性（见 §3 B5）。
- NG4：不在本批做 mobile（P3 单列，见 §4 P3）。

---

## 3. 架构约束（不可违反）

- **B1** chat 附件字节的唯一所有者是 user service 的 `chat-sessions/{sid}/files/`；Gateway 与 Runtime 都不得直读该目录。
- **B2** 展示侧取字节只走既有公共路由 [chat_sessions_api.rs:130](core/acowork-user/src/http/chat_sessions_api.rs#L130) `GET /api/chat-sessions/{sid}/files/{aid}`（参与者校验 + `?thumb=1`）。前端不得自建下载通道。
- **B3** Runtime 拿 chat 数据只经 Gateway HTTP，不得直连 user service。
- **B4** ADR-046 §2.5 的「上传件不可点」适用范围见 §5 D1——本计划不修改 ADR，但要澄清实现注释。
- **B5** `exec_attachments` 是「属于执行 run、不属于本条 chat」的标签，**保持惰性**（[ExecAttachmentLabels](apps/acowork-desktop/src/components/chat/ExecAttachmentLabels.tsx)）。本期任何改动都不得把它变成可点。

---

## 4. 模块分解

每个模块可独立 review、独立测试、独立回滚。M* 与 P* 之间无强依赖（P1 可先发）。

### M1 — 共享数据契约：`ChatAttachmentMeta`（core，~0.5d）

**定位**：先定数据，再定渲染。当前附件元数据的形状有两份：Runtime 的 `RowAttachment`（[context.rs:98](core/acowork-runtime/src/agent/chat_router/context.rs#L98)）与 user service 的 `Attachment`（[chat.rs:224](core/acowork-user/src/chat.rs#L224)）。让 Gateway 生产、Runtime 消费、Block B/C 渲染共用一份。

**改动**

1. `core/acowork-core/src/protocol.rs` 新增：

   ```rust
   pub struct ChatAttachmentMeta {
       pub filename: String,
       pub mime: String,
       pub size: u64,
   }
   ```

   **实施修正（原计划含 `kind`，已删除）**：原设计打算加一个由 mime 派生的 `kind`（text / image / document）。实际落码时去掉，理由有三：其一，user service 的 `Attachment::kind()` 只派生 image / document 两值，再引入一个含 text 的三值词汇等于在同一描述上发明第二套分类；其二，router 拿到的是原始 `mime`，`kind` 没有任何消费方（Block B/C 只渲染 filename / mime / size，路由判断由 LLM 读 mime 自行理解）；其三，去掉后结构恰好等于 Block B 已渲染的三项，Block B 与 Block C 的 parity 由"同一个结构体 + 同一个渲染函数"直接保证，而不是靠"两处派生逻辑必须一致"这种需要测试看护的约定。

   生产端字段名是 `filename`（不是 `name`）——user service 的 `Attachment.filename`（[chat.rs:226](core/acowork-user/src/chat.rs#L226)）原样出现在 append 响应里；三字段各带 `#[serde(default)]`，形状不符的元素会退化成"空描述"，由消费侧丢弃（见 M3）。
2. `acowork-runtime` 的 `RowAttachment` 删除，改用 `ChatAttachmentMeta`；`attachments_line` / `human_size` / `sanitize_inline` 保持为**唯一**渲染实现，Block B 与 Block C 都调它。
3. 上限维持 `ATTACHMENT_META_MAX_BYTES = 512`（[context.rs:42](core/acowork-runtime/src/agent/chat_router/context.rs#L42)）；超限整条丢弃，不得截断到半个条目。

**测试边界**

- 反序列化：`{filename,mime,size}` 正常、空数组 → 不产出 `[attachments]` 行；三字段缺失退化为空描述时由消费侧过滤（`turn_attachments` 丢弃 filename 与 mime 同时为空的条目）。
- 生产端投影：落库行的 `attachments`（含 `id` / `thumb`）→ 只保留三字段、顺序不变；缺失 / `null` / 非数组 → 空列表（`intent::chat_client::tests`）。
- `attachments_line`：>512B 时整条丢弃；filename 含 `\n` 不能伪造第二行（现有 `sanitize_inline` 的回归）。

**回滚**：单 crate 内类型替换，回滚即 revert。

### M2 — Gateway 生产端：把落库后的元数据带进 delivery params（~0.5d）

**定位**：零额外往返。用户服务的 append 接口**已经返回**落库后的 `SessionMessage`，附件由 `resolve_attachments` 从磁盘重读（[chat_sessions.rs:987-1005](core/acowork-user/src/chat_sessions.rs#L987)，不采信客户端），但 Gateway 把它丢掉了。

**改动**

1. `chat_client::append_message`（[chat_client.rs:169](core/acowork-gateway/src/intent/chat_client.rs#L169)）返回值由 `Result<(), String>` 改为 `Result<Vec<ChatAttachmentMeta>, String>`，从 201 响应体解析 `attachments`；解析失败只 warn 并返回空列表（**不得**让投递因元数据解析失败而失败）。
2. 两个投递入口把结果放进 delivery params：

   - 1:1：[intent_api.rs:2177](core/acowork-gateway/src/http/intent_api.rs#L2177) append → 下方 dreq 的 `params_json` 增加 `"chat_attachments"`。
   - 群聊 @：[intent_api.rs:1564](core/acowork-gateway/src/http/intent_api.rs#L1564) 同理，且该入口是**先落库后 fan-out**，元数据天然对齐共享行。

   其余 6 处调用点（PM dispatch、card 回写、sweeper 等）附件恒空，只做签名适配。
3. 信封字段 `chat_attachment_ids` 的语义不变（id 仍然只走信封）。

**测试边界**

- 单测：append 返回带 2 个附件 → params 里 `chat_attachments` 与之逐字段相等；返回体畸形 → params 里为空数组且投递成功。
- 集成：原计划在 `tests/intent_delivery.rs` 断言两条入口的 delivery 载荷与落库行一致，**实际未做**——该 fixture 起真实 Gateway 进程，而取附件元数据要经 `acowork-user` 落库往返，性价比不足。改为两层等价的守卫：
  - `intent_api::chat_params_tests`（3 支）：附件键名等于 `CHAT_ATTACHMENTS_PARAM`、无附件时仍写出空数组、共享 builder 写出的 key 集合覆盖 router 读取的全部 key。
  - `runner::tests::the_gateways_delivery_params_reach_the_router_prompt`：把 Gateway 侧同一份 params 结构喂给 router，断言 Block C 渲染出附件行 —— 守的是"两侧写/读的是同一组 key"。
  - 代价：**没有**测试覆盖"params 与落库行逐字段相等"。元数据本身由 `acowork-user` 单测覆盖（见 M1），此处只做截面搬运；若后续要补，最省的做法是不起 Gateway，直接调 `chat_delivery_params` 与 user 侧 append 返回值比对。

**回滚**：params 多一个键，旧 Runtime 完全忽略 → 天然向前兼容，可单独回滚。

### M3 — Runtime：Block C 渲染本轮附件（~1d）

**定位**：把 G1/G2 落到唯一一处。

**改动**

1. `runner.rs` 解析 `params["chat_attachments"]` → `Vec<ChatAttachmentMeta>`；缺失/类型错/超长一律降级为空，**绝不**因此失败一轮（附件缺失的代价是一轮判断质量，不是整轮丢失）。
2. `TurnContext` 增加 `attachments: &'a [ChatAttachmentMeta]`；`render()` 在 `[user message]` 之后输出与 Block B **同一个** `attachments_line` 结果。
3. 纯附件轮（`user_message` 为空且附件非空）在 `[user message]` 位置写固定占位 `(no text — the message is its attachments)`，可被 i18n 之外的因素影响：必须**逐字节固定**。
4. Block A 增加固定字节的一段：router 无法读取文件内容、附件通常意味着「有活要干」、纯附件轮要路由、交接时点一句文件名。注意两点：
   - 这是一次性的缓存前缀变更（可接受，设计 29 §6.4 的 `prefix_hash` 会变一次）。
   - 群聊 `chat.md` 覆盖体**不会**继承这段（seed 来自旧字节）——需要在文档中注明。
   - 生效范围（`prompt_builder.rs::seed_chat_prompt`）：seed **只在文件不存在时写盘**。因此这段新指令只对**新装 agent** 生效；存量安装（本机已装 agent 的 `prompts/chat.md` 均为改动前副本）不会自动获得。要生效只有两条路：删掉该文件让 seed 重跑，或由用户/agent 自己改写覆盖体。这不是漂移，是"升级不重写用户可编辑文件"的既有语义 —— 但必须写清楚，否则会被误读成"改常量即全局生效"。

**测试边界**

- **parity（G5 的结构保证）**：同一行 chat 消息，经 `build_block_b` 投影 与 经 `TurnContext::render` 渲染，附件行**逐字节相同**。
- 纯附件轮：render 含占位文案 + 附件行；`dispatch_content` 仍转发原文（现有 [runner.rs:1375](core/acowork-runtime/src/agent/chat_router/runner.rs#L1375) 附近测试的扩展）。
- params 容错矩阵：缺键 / `null` / 字符串 / 超 32 条 / 超 512B。
- 回归：`only_human_chat_messages_are_router_turns` 必须仍通过（纯附件轮仍是合法轮次）。

**回滚**：Block C 多渲染一行 + Block A 一段常量；回滚即 revert，无持久化影响。

### M4 — 降级路径与可观测性（~0.5d）

**改动**

1. [gateway_loop.rs:1397](core/acowork-runtime/src/startup/gateway_loop.rs#L1397) 拼接 excerpt 的条件从「text 非空」改为「text 非空 **或** 本轮附件非空」；纯附件轮也带上尾部说明。
2. `chat-router-decisions.jsonl` 增加 `attachments: N` 与 `attachment_only: bool`（`decision.rs` 的 `TurnTrace`）。当前该日志不记录任何消息内容与附件计数——这正是本缺陷潜伏至今的原因之一。
3. 不记录文件名：日志只记数量，避免把用户内容落到长期日志。

**测试边界**：decision 行字段快照；attachment-only 轮 fallback 后 params.message 非空。

### P1 — desktop：`AttachmentCapsule` 原语 + chat 胶囊化（~0.5d）

**定位**：样式统一。这是用户感知最强、成本最低的一块。

**改动**

1. 新增 `components/chat/AttachmentCapsule.tsx`：`Tag = onClick ? "button" : "span"`，统一 `inline-flex items-center gap-1.5 rounded-md border border-border-outer bg-zinc-50 px-2.5 py-1 text-xs` + icon slot + `max-w-[200px] truncate` + hover。
2. `AttachmentChipRow` 的 `WorkspaceRefChip` / `SelectionChip` / `FolderChip` 改用它（纯抽取，不改语义）。
3. `ChatAttachmentChip` 文档分支改用它（`FileText` + filename + 尺寸），图片分支保留 56×56 但补 `cursor-pointer` + hover ring；props 由 `{id,filename,mime}` widening 为 `ChatAttachment`（[types.ts:2028](apps/acowork-desktop/src/lib/types.ts#L2028)，已含 `size` 与 `thumb`）。
4. `chatSessionStore` 行类型的 `attachments` 补 `thumb?`（数据已在线上，仅类型缺失，[chatSessionStore.ts:113](apps/acowork-desktop/src/stores/chatSessionStore.ts#L113)）。

**测试边界**

- `AttachmentCapsule`：button / span 两态、hover class、truncate。
- `ChatAttachmentChip` 既有测试（[ChatAttachmentChip.test.tsx](apps/acowork-desktop/src/components/chat/ChatAttachmentChip.test.tsx)）扩展：文档显示尺寸文本、图片有可点态。
- 两 surface 一致性：1:1 与 group 的同一附件渲染 DOM class 相同（复用 `GroupChatPanel.execLabels.test.tsx` 的模式）。

**回滚**：纯样式抽取，回滚即 revert。

### P2 — desktop：`openChatAttachment` dispatcher + ImageLightbox（~1.5d）

**改动**

1. 新增 `components/common/ImageLightbox.tsx`（当前仓库没有 lightbox）：遮罩 + 大图 + 文件名 + 下载按钮 + ESC/点遮罩关闭。图片源用 `chatAttachmentObjectUrl(chatId, id)`（不取 thumb）。
2. 新增 `lib/openChatAttachment.ts` —— **单一入口**，按 `mime` / `size` 分三支：

   | 条件 | 动作 | 复用 |
   |---|---|---|
   | `mime.startsWith("image/")` | ImageLightbox | `chatAttachmentObjectUrl`（已有） |
   | `canPreviewAttachment(a)`（文本可读且 ≤8MB） | 只读 Monaco 标签 | `fetchAttachmentText` + `openAttachmentPreview`（已有） |
   | 其余 | 保存对话框 + Rust 流式下载 | `downloadChatAttachment`（已有） |

3. `openAttachmentPreview` 的 tab key 由 `attachment:${userId}:${chatId}:${attachmentId}` 改为 `attachment:${chatId}:${attachmentId}`——群聊 surface 没有 `userId` 在手，而 `chatId` 全局唯一即可去重（改动点 [fileEditorStore.ts:614-650](apps/acowork-desktop/src/stores/fileEditorStore.ts#L614) + 其单测）。
4. `ChatAttachmentChip` 接 `onClick` → dispatcher；失败（blob 已删/离线）toast，不静默。
5. `downloadChatAttachment` 增加 session 维度入口（现有签名要 `userId`，群聊拿不到）；Rust 命令 `download_attachment(url, path, ...)` 接受任意 URL 并自带 account token，故**无需改 Rust**。

**测试边界**

- dispatcher 三支路由（mock fetch / invoke / store action），含失败降级。
- tab key 去重：同一附件二次点击激活既有 tab（扩展现有 `fileEditorStore.attachmentPreview.test.ts`）。
- Lightbox：ESC 关闭、点遮罩关闭、焦点不逃逸。
- 迁移行（本机 4 条 `migrated:…` 且带附件）点击时的降级路径。

**回滚**：新文件 + 一处 onClick 接线，可单独 revert 而不影响 P1。

### P3 — mobile（单列，~1.5d，不并入本批）

`📎 {a.name}` 纯文本 → 胶囊；点击语义还缺基础设施：无图片查看器、无「系统打开方式」能力。需要新增：图片全屏查看器、文档下载到 App 目录后 `open`、Tauri mobile 侧能力声明。**风险与工作量都独立于 desktop，建议单独立项**。

---

## 5. 关键设计决策

**D1 — ADR-046「上传件不可点」的适用范围。**
ADR-046 §2.5 的点击表只列了 file / selection / folder（工作区引用），上传件是**被省略**而非被禁止；[openWorkspaceRef.ts:25-27](apps/acowork-desktop/src/lib/openWorkspaceRef.ts#L25) 的注释把「省略」写成了「禁止」。事实依据是：`openAttachedRef` 的入参是 `absPath`，而 chat 上传件没有工作区路径——它从来不在该 dispatcher 的域里。因此本计划**不修改 ADR**，只做两件事：新增独立的 `openChatAttachment`（域是 chat blob），并把 `openWorkspaceRef` 的注释改成「chat 上传件由 openChatAttachment 负责」。执行侧上传件维持惰性（NG2）。

**D2 — Block C 与 Block B 共用渲染函数。**
不是「分别写两份、注意保持一致」，而是 Block C 直接调用 Block B 已有的 `attachments_line`。parity 由代码结构保证，测试只需守一次。

**D3 — 元数据走 delivery params，不新增信封字段。**
信封字段（`chat_attachment_ids`）是 id 通道，语义清晰；元数据是「本轮消息的内容」的一部分，与 `message` 同级，放 params 天然正确。代价是 params 变大（512B 上限），收益是零协议变更、旧 Runtime 忽略即兼容。

**D4 — 纯附件轮的占位文案。**
空 `[user message]` 会让模型当作「没说话」。固定占位 `(no text — the message is its attachments)` 明确告诉它：人发了东西，只是没打字。必须逐字节固定（Block C 虽不进缓存前缀，但决策可复现性要求这一点）。

**D5 — 安全边界：附件内容永远是数据，不是指令。**
本期只送元数据。若将来做「正文预览」，必须在 Block C 明确标注该段为不可执行的数据，且沿用 512B 级别的上限治理；`http_request` 与 `dispatch` 的既有约束不变。**这是 NG1 的理由，不是遗漏。**

**D6 — P2 为何不复用 `openAttachedRef`。**
它的入参是 `absPath` + 工作区解析，chat 附件两者都没有；扩宽它会让「工作区引用」与「chat blob」两类对象混在一个 dispatcher 里，正是 §3 B5 想避免的那种漂移。两个 dispatcher，两套域，各自可测。

**D7 — tab key 去掉 `userId`。**
群聊 surface 拿不到 `userId`（chatSessionStore 走 session 路由正是为此）。`chatId` 已全局唯一，去掉 `userId` 后 key 仍不会冲突，且两种 surface 用同一 key 规则。

---

## 6. 实施顺序与批次

| 批次 | 内容 | 依赖 | 可独立交付 |
|---|---|---|---|
| 1 | P1（胶囊统一） | 无 | 是 |
| 2 | P2（可点 + 打开） | P1 | 是 |
| 3 | M1 → M2 → M3 | M1 先于 M2/M3 | 每一步是 |
| 4 | M4 | M3 | 是 |
| 5 | P3（mobile） | 单独立项 | 是 |

批次 1–2 与 3–4 互不依赖，可并行；同时改两边的文件只有 `ChatAttachmentChip`（P 侧）与 `context.rs`（M 侧），无交叉冲突。

---

## 7. 验收指标

- **V1（G5）**：`build_block_b` 与 `TurnContext::render` 对同一行消息的附件投影逐字节相等（单测守）。
- **V2（G1/G2）**：构造「字符 + 截图」与「纯附件」两种轮次，Block C 渲染出的 `[attachments]` 行与占位文案符合预期（单测守）。
- **V3（G3/G4）**：desktop 两个 surface 的 chat 附件均可点，三支路由各有单测；点击失败有可见反馈。
- **V4（G6）**：`chat-router-decisions.jsonl` 出现 `attachments` / `attachment_only` 字段。
- **V5（回归）**：`only_human_chat_messages_are_router_turns`、`ChatAttachmentChip` 既有测试、`AttachmentChipRow` 既有测试全绿。

---

## 8. 风险与回滚

| 风险 | 影响 | 缓解 |
|---|---|---|
| Block A 常量变更导致一次缓存前缀失效 | 一轮成本上升，无功能影响 | 单独提交，`prefix_hash` 变化可在日志确认 |
| 群聊 `chat.md` 覆盖体不含新指令 | 群聊 router 对附件提示不敏感 | 文档注明。注意 `seed_chat_prompt` **只在文件缺失时写盘**，存量安装不会自动获得新指令，需删文件重 seed 或改写覆盖体 |
| `append_message` 返回值改动波及 8 处调用点 | 编译期可见，无运行时风险 | 除两处外全部 `let _ =` 适配 |
| 迁移行附件 blob 已不在 | 点击失败 | P2 必须实现 toast 降级 + 单测覆盖 |
| 群聊缺少 `userId` | 预览无法走 `openAttachmentPreview` | D7 去掉 tab key 的 `userId` |
| 抽屉式误解：顺手让 `exec_attachments` 可点 | 违反 ADR-100 D12 | §3 B5 显式列为禁区 |

**回滚粒度**：P1 / P2 / M1 / M2 / M3 / M4 各自一次提交，均可单独 revert；M2 的 params 新键对旧 Runtime 完全无害，是唯一可「只回滚消费端」的组合。
---

## 9. 实施记录（2026-10 / 本轮）

### 已交付

| 模块 | 状态 | 落点 |
|---|---|---|
| M1 共享契约 | 完成（`kind` 已删，见 §4 M1） | [protocol.rs](core/acowork-core/src/protocol.rs) · `ChatAttachmentMeta` |
| M2 生产端 | 完成 | [chat_client.rs](core/acowork-gateway/src/intent/chat_client.rs) · [intent_api.rs](core/acowork-gateway/src/http/intent_api.rs) 两入口 |
| M3 Block C | 完成 | [context.rs](core/acowork-runtime/src/agent/chat_router/context.rs) · [runner.rs](core/acowork-runtime/src/agent/chat_router/runner.rs) · [chat_prompt.rs](core/acowork-core/src/chat_prompt.rs) |
| M4 降级 + 可观测 | 完成 | [gateway_loop.rs](core/acowork-runtime/src/startup/gateway_loop.rs) · [decision.rs](core/acowork-runtime/src/agent/chat_router/decision.rs) |
| P1 胶囊原语 | 完成 | [AttachmentCapsule.tsx](apps/acowork-desktop/src/components/chat/AttachmentCapsule.tsx) · [ChatAttachmentChip.tsx](apps/acowork-desktop/src/components/chat/ChatAttachmentChip.tsx) |
| P2 打开文件 | 完成 | [openChatAttachment.ts](apps/acowork-desktop/src/lib/openChatAttachment.ts) · [ImageLightbox.tsx](apps/acowork-desktop/src/components/common/ImageLightbox.tsx) |
| P3 mobile | 完成（受限，见下） | [chatAttachment.ts](apps/acowork-mobile/src/lib/chatAttachment.ts) · [ChatAttachmentCapsule.tsx](apps/acowork-mobile/src/components/ChatAttachmentCapsule.tsx) · [AttachmentViewer.tsx](apps/acowork-mobile/src/components/AttachmentViewer.tsx) |

### 与计划的偏离（3 处）

1. **M1 去掉 `kind`**（理由见 §4 M1）。
2. **消费侧过滤空描述**：`ChatAttachmentMeta` 三字段各带 `#[serde(default)]`（保留原有意图：字段残缺不得让整行消息掉出历史窗口），代价是形状不符的元素会解析成"空描述"。`runner::turn_attachments` 因此在消费处丢弃 `filename` 与 `mime` 同时为空的条目——否则 Block C 会输出一行没有名字的 `[attachments]`，且决策日志的 `attachments` 计数会退化成"数组槽位数"而不是"文件数"。生产端（Gateway）保持忠实地原样投影，不做过滤：过滤属于消费侧的归一化职责，生产端过滤会把 store 的 bug 藏起来。
3. **decision 日志的 `attachment_only` 与 Block C 占位判定共用一个谓词**（`context::is_attachment_only`），避免"日志说纯附件轮、提示词却渲染了文本"这类漂移。

### 新增测试

- `context`：Block B / Block C 附件行**逐字节相等**；纯附件轮占位；纯文本轮无附件行。
- `runner`：`turn_attachments` 容错矩阵（缺失 / `null` / 字符串 / 对象 / 元素形状错 / 部分字段）+ 纯附件轮可寻址。
- `gateway_loop`：`degraded_brief` 纯函数四态（纯附件轮带 excerpt 且不以换行开头、文本前缀、无 excerpt、只有空白）。
- `decision`：新字段序列化 + 旧行兼容（读 0 / false）+ 日志不含 `filename`。
- `intent::chat_client`：落库行 → 三字段投影（`id` / `thumb` 不外泄）、缺失/null/非数组、坏元素跳过其余存活。

### 验证

- `cargo test -p acowork-runtime -p acowork-gateway --lib`：**1675 + 681 passed / 0 failed**（最终复跑）。
- `cargo clippy --all-targets -- -D warnings`（整个 workspace）：干净。
- desktop：234 文件 / 2057 测试全绿，`tsc --noEmit` 干净。
- mobile：21 文件 / 253 测试全绿（2 skipped），`tsc --noEmit` 干净。
- 注：runtime 全量并发（默认线程数）下有 4 支与本次改动无关的旧测试会抖动（`e2e_prompt_cache` / `http::server` ×2 / `security::audit_log`），单跑与 `--test-threads=4` 均通过——既有并发隔离问题，不在本计划范围。

---

## 10. P3 实施记录（mobile）

**已交付**

| 件 | 说明 |
|---|---|
| `lib/chatAttachment.ts` | `attachmentKind` / `isTextual` / `humanSize` / `attachmentObjectUrl`（按 session 路由取字节，带 401 阶梯）/ `fetchAttachmentText`（2MB 上限） |
| `api.ts` | 抽出 `authedFetch`（401→rotate→replay 一条阶梯，两个读者：JSON 与字节），新增 `fetchChatAttachmentBlob` |
| `components/ChatAttachmentCapsule.tsx` | 真 `<button>`（不是带 onClick 的 span），类型图标 + 文件名 + 尺寸 + 完整 `aria-label` |
| `components/AttachmentViewer.tsx` | 图片 → 全屏（整面可点关闭）；文本 → 底部 sheet 预览；其他 → 明确说明无法预览 |
| `AgentChatScreen` | 行内胶囊接入；viewer 状态提升到屏幕级（轮询重绘不会关掉正在看的文件） |

**与计划的偏离：mobile 的「系统打开方式」没有做，这是有意的**

`tauri-plugin-opener` 的 Android 实现是 `Intent(ACTION_VIEW, Uri.parse(path))`（`OpenerPlugin.kt`，本机核实过源码）。裸文件路径没有 scheme，`Uri.parse` 出来的东西找不到 activity；而 `content://` 需要 FileProvider authority —— 声明 authority 要改 `src-tauri/gen/android` 的 manifest 与 `res/xml/file_paths.xml`，而那个目录由 `dev/android/build_mobile.ps1` 生成（`tauri android init` 会整目录覆盖）。所以这条链路要么写进 build 脚本（可，但需要在真机上验证一个我无法在此环境验证的 Intent），要么现在就不加。

选择：**不加一个半可用的依赖**。已加后又撤回了 `tauri-plugin-opener`（见 §9 之外的 diff 说明：Cargo.toml 最终无新增依赖）。

当前 mobile 的能力边界：图片、文本可看；二进制（PDF/zip/docx）打开一个说明「请在桌面端打开」。这比「点不动的胶囊」诚实，也比「点了报错」好。

**若要做完这一步**（需真机 + 一次 `build_mobile.ps1` 改动）：
1. build 脚本里给应用加 FileProvider（authority = `<applicationId>.fileprovider`）与 `res/xml/file_paths.xml`（`<cache-path>`）；
2. mobile crate 加一个 `open_file` 命令：把字节写到 `app_cache_dir/attachments/`，经 JNI 发 `ACTION_VIEW` + `FLAG_GRANT_READ_URI_PERMISSION` 的 `content://` Intent（`jni` 已是依赖）；
3. 前端 `other` 分支改为调用它，失败回落到当前的说明 sheet。

**验证**

- `npx tsc --noEmit`：干净；`npx vite build`：通过。
- `npx vitest run`：**21 文件 / 253 passed / 2 skipped**（含新增 12 支附件测试）。
- live 只读契约探针（`ACOWORK_E2E_LIVE=1` vs `127.0.0.1:19876`）：**2 passed**。

---

## 11. 端到端冒烟（本轮结论）

| 层次 | 内容 | 结果 |
|---|---|---|
| 契约端到端 | `runner::tests::the_gateways_delivery_params_reach_the_router_prompt` —— 用 **Gateway `intent_api` 写出的同一份 params 字面量**（同 key、同嵌套、同字段名）喂给 router，断言 Block C 渲染出附件行。两侧单测各自绿仍可能写错 key，这一支守的就是 key | passed |
| Rust 全量 | `acowork-runtime --lib` 1675 passed / `acowork-gateway --lib` 681 passed；`clippy --all-targets -D warnings` 干净 | passed |
| desktop | 234 文件 / 2057 passed；`tsc --noEmit` 干净 | passed |
| mobile | 21 文件 / 253 passed（2 skipped）；`tsc --noEmit` 干净 | passed |
| live 只读 | mobile 的四 tab + drawer 路由契约探针对运行中的 Gateway | passed |

**活体端到端（隔离实例，2026-10-28）**：用 `CARGO_TARGET_DIR=target-test` 编译，另起一套**独立** Gateway（`--home %TEMP%/acowork-e2e --addr 127.0.0.1:29876 --mqtt-addr 127.0.0.1:29875 --auth-mode multi_user`），跑的是**新二进制**，生产实例（19876）全程未动。链路是真 HTTP、真 MQTT、真 Runtime：`admin-setup` → login → 起 `com.acowork.quality-assurance` → `GET /api/chat-sessions/default?pair=…` → `POST .../files` 上传 `e2e-build.log`（40B, text/plain）→ `POST /api/intents/chat-message` 带 `attachment_ids`。

落库行确认带附件（`GET .../messages` 回 `attachments:[{filename:"e2e-build.log",mime:"text/plain",size:40,thumb:"txt"}]`），三支轮次写出的 `chat-router-decisions.jsonl` 分别是：

| 轮次 | `attachments` | `attachment_only` |
|---|---|---|
| 文字 + 附件 | 1 | false |
| 只有附件 | 1 | true |
| 只有文字 | 0 | false |

这就是 M2（Gateway 从落库行投影元数据并写进 delivery params）+ M3（Runtime 解析 `chat_attachments`、纯附件轮占位）+ M4（日志字段）合起来的端到端证据——改动前这三行都会是"没有这个字段"。

**Block C 的真实提示词字节（同一隔离实例，配了本地 mock provider）**：`POST /api/providers` 注册一个 `openai` 兼容端点指向 `dev/e2e/mock_openai.py`（127.0.0.1:29999，把每次请求体落盘），并在 agent 的 `config/agent_config.json` 里写 `chat_router: {model:"mock-model", provider:"openai"}`，重启该 agent。于是路由真的发出了模型请求，抓到两条 Block C（逐字节，取自 LLM 请求体）：

```
[user message]
please look at this attachment and tell me what it says
[attachments] e2e-build.log (text/plain, 40B)
```

```
[user message]
(no text — the message is its attachments)
[attachments] e2e-build.log (text/plain, 40B)
```

同两轮的决策日志：`decision=fallback model=mock-model provider=openai att=1 only=False / only=True`（`fallback_reason` = 写回被 401 拒绝，见下）。**这就是这个缺陷原本的样子被翻转过来的证据**：改动前这两条 Block C 里只会有一行文字，`[attachments]` 根本不存在。

**这条路走不到的地方（环境限制，非代码问题）**：`multi_user` 强制下，节点拉起的 Runtime 没有用户身份，于是它读 chat 历史与写回 chat 各得一个 `HTTP 401 Unauthorized: missing bearer token`（这就是每轮 `history: unavailable` 与 `decision=fallback` 的原因，`history_len` 因此恒为 0）。**路由本身跑通了**——提示词组装、模型调用、决策解析全在（上面的 Block C 就是从模型请求体里取的），断的只是它与 chat 存储之间的鉴权。生产的 multi_user 部署由 Gateway 下发 node token 解决这一层，本隔离实例因为绕过了那条引导链路而没有。

**清理**：隔离 Gateway 与其拉起的 QA Runtime 已按 PID 关闭，生产实例 19876 仍返回 200；`target-test` 保留（复跑用，勿与 `target` 混淆）。

**原先没做、现已做**：活体端到端。下面是当时记录的原因（保留以便回溯为什么绕了一圈）：

~~没有做的~~：对运行中的 Gateway 做一次「真实附件走完 router」的活体验证。原因不是环境不通（`127.0.0.1:19876` 与 `192.168.3.43:19876` 都返回 200），而是**运行中的进程跑的是改动前的二进制**——M2/M3 不在里面，跑出来必然是旧行为；而换成新二进制需要重启 Gateway/Runtime，规则明确禁止（我本身跑在该 Runtime 里）。这一条留给下一次自然重启后复核：发一条带附件与文字的 chat，然后看 `chat-router-decisions.jsonl` 里该行是否 `attachments=1`。
