# ADR-107：引用部件——chat 背景随行，`user` 行只留人的原话

- **编号**：ADR-107
- **状态**：提议（待架构评审）——**已落地**：`acowork-core` 的 `MessageRef`、Runtime 的 `params.context_refs` 解析 / 模型侧渲染 / `context_ref` 条目持久化、Chat Router 的 `exec_session_create(background)` 与降级路径、Desktop 的 `user_with_parts` 折叠与引用块 markdown 渲染，均有单测钉住。
- **日期**：2026-11-04
- **决策者**：ACowork 架构组
- **关联**：[ADR-046](./ADR-046-unified-attachment-entries.md)（统一附件条目——本 ADR 的引用部件与之平行）、[ADR-099](./ADR-099-chat-session-llm-router.md)（Router 分层）、[ADR-106](./ADR-106-chat-execution-reference-projection.md)（chat/execute 边界）、[ADR-060](./ADR-060-prompt-cache-friendly-context-block-reorg.md)（稳定前缀 + 末尾追加）、设计 27 §5、设计 29 §11

## 摘要

Chat Router 通过 `exec_session_create` 新建执行会话时，把 router 写的"简报"（brief）与人的原话拼成**一个字符串**投递：`{brief}\n\n[user message]\n{words}`。执行会话的 JSONL 第一条 `user` 行因此不是人说的话，而是 router 的框架 + 人说的话。三处后果，都是实测到的现象而不是推断：

1. **标题互相雷同**。自动标题取第一条 `user` 行（ADR-046 §2.1 的约定是"标题来自用户自己的消息"），而所有经 chat 派发的会话的第一行都以 brief 的固定首句开头，于是标题前缀全部撞车。
2. **违反 ADR-046 §2.1**：JSONL 的 `user` 行 MUST 存原始输入。拼接发生在人输入之后的第一步，原话在这一跳之后就不可恢复。
3. **转录本撒谎**：router 的框架文本被当成人的话渲染出来。

本 ADR 把"人的话"和"Runtime 补的上下文"拆成两个**部件**（part），而不是一个字符串的两段：

- `user` 行只存人的原话，**逐字**，不做 trim、不拼接、不重写；
- router 的背景信息降级为**引用部件**（reference part），与附件（ADR-046）平行：飞行时走 `params.context_refs`，落盘时是紧随 `user` 行的一条 `role="system"`、`metadata.type="context_ref"` 条目；
- 模型看到的消息是**原话在前、引用块在后**；人看到的是气泡（原话，纯文本）+ 气泡下方的折叠引用卡片（markdown 渲染）。

净效果：标题重新由人的原话决定；`user` 行恢复为可引用的原话；引用文本第一次可以按 markdown 渲染，而人的话依然按字面渲染。

## 背景（全部经代码核实）

### 1. 拼接发生在哪里

[runner.rs](../../../core/acowork-runtime/src/agent/chat_router/runner.rs) 的 `dispatch_content` 把 router 工具调用的 `message` 参数与 `params.message`（人的原话）拼成一个 `content` 交出去；chat 面板的 `new_task` 链路与群聊 `@` 链路共用它。`{brief}\n\n[user message]\n{words}` 的字面量因此是**每条** chat 派发回合的开头。

### 2. 标题实际读什么

标题生成在 [loop_.rs](../../../core/acowork-runtime/src/agent/loop_.rs) 的回合末段，输入取自当轮 `raw_user_message`；会话恢复 / 二次进入时的输入取自 [conversation.rs](../../../core/acowork-runtime/src/conversation.rs) 的 `read_first_user_message`。两处都指向"第一条 `user` 行"，与 ADR-046 §2.1 一致——所以问题不在标题，而在 `user` 行被污染。

### 3. 附件已经提供了正确的先例

ADR-046 规定附件是**部件**：`user` 行之后新增 `role="system"` 的条目，`metadata.type` 判别联合区分 `file_upload` / `image_upload` / `attached_file` / `attached_selection` / `attached_folder`；前端 `foldMessages` 把它折回同一个视觉块。引用部件要的正是同一套机制，只是内容来源不同：附件是人拖进来的，引用是 Runtime 知道会话需要而人没打的。

### 4. 降级路径有同样的问题

`gateway_loop.rs` 的 `degraded_brief`：当 router 不可用（或没有可用模型）时，Runtime 用有界 chat 尾部自己拼一个 brief，同样以固定首句开头——固定首句问题在降级路径上重复出现。修一处必须修两处，否则"平时正常、降级时又撞车"。

## 决策

### D1　引用是一等部件，不是字符串的前缀

新增 `acowork_core::message_ref::MessageRef { kind, content }`（[message_ref.rs](../../../core/acowork-core/src/message_ref.rs)）。`kind` 来自 `REF_KIND_*` 常量（当前只有 `chat_background`），`content` 是 markdown 正文。没有任何"拼接"辅助函数存在于核心类型上——引用只能通过它自己的通道流转：

| 通道 | 形状 |
|---|---|
| Intent 飞行 | `params.context_refs: [{"kind","content"}]` |
| 落盘 | `role="system"` 条目，`content` = 正文，`metadata` = `{"type":"context_ref","kind":…}` |
| 模型输入 | 原话 + 空行 + `[background: …]` 块 |
| 人看到的 | 气泡（原话）+ 折叠卡片（正文 markdown） |

正文放在条目的 `content` 而不是 metadata 里：不认识引用的读者（导出、检索索引、纯文本转录视图）仍能看到文本，而不是看到一个空行加一段不透明 payload。metadata 只承载 `kind`，因为 `kind` 是唯一同时决定"人看到的标签"和"模型看到的块头"的字段。

### D2　`user` 行只留原话（MUST NOT 拼接）

重申并收紧 ADR-046 §2.1：`user` 行 MUST 是 `params.message` 的逐字副本。Runtime 添加的任何东西——附件、引用、系统块——都是紧随其后的独立条目。`exec_session_create` 因此不再产出"整段消息"，只产出一个引用部件：

- 入参从 `message`（整段、自包含简报）改为 `background`（REQUIRED、非空，**只是背景**）；
- 提示词同步改写：明确"人的话由 Runtime 自己投递，你只补那句话指涉的上下文；把人的话再写一遍只是重复"。

这条也是标题问题的真正修复：标题读的是 `user` 行，`user` 行是原话，标题自然由原话决定。

### D3　模型侧顺序固定为"原话在前、引用在后"

`intent_turn::render_user_turn(content, refs)`（[intent_turn.rs](../../../core/acowork-runtime/src/agent/intent_turn.rs)）：

- 无引用时**逐字节**返回 `content`——没有引用的回合，prompt 输入与改动前完全一致（ADR-060 的前缀缓存不受影响）；
- 有引用时 `{原话}\n\n{块1}\n\n{块2}`，每块以 `model_header` 开头（`[background: the chat conversation this task was opened from — context, not a new instruction]`），明确标注"这是上下文，不是新指令"；
- 原话为空（附件消息、或 router 路由了一条只有附件的消息）时**不**留下空的首行，块直接成为整条消息——空首行在模型看来是"人什么都没说"，虽然是事实，但让块挂在空白下面只是噪声。

块头由 `kind` 决定而不是由 router 写入的文本决定：未知 `kind` 没有块头，`context_refs_from_params` 与 `write_message_refs` 双双丢弃它（渲染不出来就不落盘，两端口径一致）。

### D4　落盘顺序：`user` → 引用 → 附件

`loop_.rs` 在持久化 `user` 行之后、写附件条目之前调用 `loop_persist::write_message_refs`。理由与 ADR-046 相同（部件必须紧邻它的 `user` 行），顺序固定让前端折叠窗口与日志读法都可预测。

引用条目不传 client id：与附件不同，引用没有客户端孪生条目需要去重，新 uuid 就是它的 id（`append_message_with_id(..., None)`）。

### D5　前端：一种折叠、两种槽位

`foldMessages` 的折块类型从 `user_with_attachments` 更名为 `user_with_parts`，折叠判据从"附件条目"扩为"任一部件的条目"。

**部件类型的判据只允许有一份定义**（`[messageParts.ts](../../../apps/acowork-desktop/src/components/chat/messageParts.ts)`）。它同时被两个消费者使用：JSONL 加载器（`chatStore.convertConversationEntry`，决定"这条行的 `metadata` 值不值得留在消息上"）与折叠器（`messageFolder`）。首版把这份判据写了两份——加载器里一份附件集合、折叠器里一份——于是新增第二种部件时加载器先丢掉了 `context_ref` 的 metadata，引用行以"没有 metadata 的 `system` 消息"抵达折叠器，不折叠，最终被渲染成一条满宽的系统气泡（实测缺陷）。一份定义，两个消费者；`messageRefPart.test.tsx` 的 "the JSONL load path" 用例从 Runtime 真实写出的 wire 形状出发钉死这条链。块的 `items` 保持日志顺序，**渲染器**决定每个部件进哪个槽位：

- 附件 → 气泡上方的 chip 行（不变）；
- 引用 → 气泡**下方**的折叠卡片（[MessageRefBlock.tsx](../../../apps/acowork-desktop/src/components/chat/MessageRefBlock.tsx)），`ChatMarkdown` 渲染正文，默认收起。

顺序由对话阅读顺序决定：人的话先读，背景后读；长背景默认展开会把 agent 的回答挤出屏幕。

### D6　人的话永远是字面文本，引用才是 markdown

用户已明确：**不**把用户消息渲染成 markdown，只有引用正文渲染 markdown。这条与 `GroupChatPanel.userLiteral.test.tsx` 钉住的"说话人决定渲染方式"一致：人的输入是字面文本（`whitespace-pre-wrap`，无 `.prose` 外壳），agent 与 Runtime 产出的文本走 markdown。引用卡片是中性面（不是 accent 填充的用户气泡），所以 `prose` 外壳的正文色在这里是正确的——这正是用户气泡用不了它的原因（`prose` 把正文染成页面级近黑色，压在 accent 填充上就是那个"用户气泡字变黑"的缺陷）。

### D7　降级路径产出同样的部件

`degraded_brief` → `degraded_background`：不再拼字符串，而是产出 `Some(MessageRef::chat_background(...))`，正文以"router 不可用，以下是对话尾部"开头（这句对模型的判断有用，必须保留），填入 `params.context_refs`。`params.message` 保持不动。两条路径在前端与持久化上的形状因此完全一致。

### D8　引用正文里不出现机器标识：名称与 ID 一起交给模型

引用正文是模型写的散文（D3/D4），而它会被**人逐字读到**。因此交给模型的上下文不能只有一个地址：`ws-49616826fcfc` 对人没有意义，对模型却是当时唯一可用的"工作区描述"，于是它把这串 ID 抄进了正文（2026-11-06 已报缺陷：执行会话的背景卡片正文写着"工作区 ws-49616826fcfc"）。

Block C 的 `draft:` 行因此**同时给名称与 ID**：`workspace="ACoworkDev" (id ws-49616826fcfc)`。名称的权威来源与两个客户端同一条规则——`agent_workspaces.json` 的 `alias`，其次目录末段（`WorkspaceResolver::display_name`）；ID 不能省，`session_config.workspace` 按 ID 对注册表校验，所以 schema 里写明"要 ID，不要名称"。解析不到名称时只留 ID：不编名字，也不留空。

不做的：**不让前端对正文做 ID→名称的正则替换**。模型行为无法预测，只能引导；把"人看得懂"寄望于渲染期的字符串修补，换个模型、换种措辞就失效，还会让同一段正文在不同客户端读出不同内容。这条规则的位置是"给模型的输入"，不是"给用户的输出"。

## 备选方案与被否决的理由

| 方案 | 否决理由 |
|---|---|
| 保留拼接，只改标题取证（例如取 `[user message]` 之后的段落） | 治的是症状：`user` 行仍不是原话（违反 ADR-046 §2.1），转录本仍在撒谎，任何第二个消费者都要重新实现同一段解析。标题问题只是污染的最显眼后果。 |
| 背景作为 `attached_*` 附件的第六种类型 | 语义错误：附件是"人给的"，引用是"Runtime 给的"，两者在"能不能删"、"点了会打开什么"上不同（附件 chip 可移除、可打开工作区文件）。平行类型比塞进同一个判别联合更诚实，前端也是平行的两个槽位。 |
| 背景只放进 `system` prompt（不进转录本） | 会话重开后背景消失，模型在第二轮就丢了上下文；且人无从核对 agent 到底被告知了什么。 |
| 让 `user` 行存原话、背景写进 `metadata` 而不单独成行 | metadata 不是给模型看的通道；且会把 markdown 正文塞进一个所有条目共用的字段里，导出与索引都会漏掉它。 |
| 引用正文也按字面渲染（不 markdown） | 直接违背本次需求：引用正文需要列表 / 代码块 / 强调。人的话保持字面已经由 D6 保证，两者不冲突。 |

## 后果

**正面**

- 标题重新由人的原话决定；chat 派发的会话标题不再因为固定首句而收敛。
- `user` 行恢复 ADR-046 §2.1 契约，任何按"第一条 user 行"取原话的消费者（标题、导出、`@` 引用、检索）重新正确。
- 引用文本可 markdown 渲染，人的话仍逐字——两个需求同时满足而没有互相污染。
- 工作区在人类可读的文本里以**名称**出现（D8），不再把 `ws-…` 地址当名字用。
- 无引用的回合 prompt 输入逐字节不变（D3），前缀缓存不额外 miss。

**负面 / 代价**

- 多一个部件类型、一个 metadata 判别值、一次落盘写入；前端多一个组件与一套 i18n 文案（`messageRef.chatBackground` / `messageRef.generic`，5 种语言）。
- `exec_session_create` 的 wire 参数名变了（`message` → `background`），提示词、schema、`RETIRED_ACT_NAMES` 式的漂移守卫与既有单测断言必须同步改——一次性的前缀缓存 miss，属预期。
- 只有背景的回合（人只发了附件）`user` 行 `content` 为空：转录本会出现无气泡的块，靠引用卡片与附件 chip 表达这一轮。可接受（事实如此），但读法需要在 UI 上被理解。

**回滚**：`context_refs` 是可选的 `params` 字段，缺省即"没有引用"，旧版 Runtime 会直接忽略它（不认识的 params 键不参与构造）；渲染侧对未知 `kind` 一律跳过。因此回滚点是"让 `dispatch_parts` 退回把背景拼回 `content`"，其余层次无需改动。

## 落地清单（代码即契约）

| 层 | 位置 | 内容 |
|---|---|---|
| 数据 | [core/acowork-core/src/message_ref.rs](../../../core/acowork-core/src/message_ref.rs) | `MessageRef { kind, content }`、`REF_KIND_CHAT_BACKGROUND`、`model_header`、`is_empty`；3 个单测钉 wire 形状 / 未知 kind / 空正文 |
| 解析 | `intent_turn::context_refs_from_params` | 逐条容错：空正文与未知 kind 丢弃，不让一个部件毁掉整轮 |
| 渲染 | `intent_turn::render_user_turn` | 原话在前、块在后；无引用逐字节不变 |
| 持久化 | [conversation.rs](../../../core/acowork-runtime/src/conversation.rs) `MessagePartMeta` / `ContextRefMeta` / `MESSAGE_PART_TYPE_CONTEXT_REF`；`loop_persist::write_message_refs` | `{"type":"context_ref","kind":…}` + 正文在 `content` |
| 回合 | `loop_.rs` | `turn_message_refs` 在 `user` 行之后、附件之前落盘；标题输入仍取 `raw_user_message` |
| Router | [chat_router/runner.rs](../../../core/acowork-runtime/src/agent/chat_router/runner.rs) `dispatch_parts`、[chat_router/tool.rs](../../../core/acowork-runtime/src/agent/chat_router/tool.rs) schema、[chat_prompt.rs](../../../core/acowork-core/src/chat_prompt.rs) | 入参 `background`；新会话产出 `(原话, [ref])`，续会话产出 `(原话, [])`；背景与原话同文时丢弃 |
| 降级 | [startup/gateway_loop.rs](../../../core/acowork-runtime/src/startup/gateway_loop.rs) | `degraded_background` 产出引用部件，`params.message` 不动 |
| 词表（D8） | [tools/workspace_resolver.rs](../../../core/acowork-runtime/src/tools/workspace_resolver.rs) `display_name`、[chat_router/runner.rs](../../../core/acowork-runtime/src/agent/chat_router/runner.rs) `RouterDeps::workspace_display_name` + `draft_label` | ID→名称只有一条规则（`alias` → 目录末段 → `None`）；Block C 的名称与 ID 同出，解析不到只留 ID |
| 前端 | [messageParts.ts](../../../apps/acowork-desktop/src/components/chat/messageParts.ts)（部件类型唯一判据）、[messageFolder.ts](../../../apps/acowork-desktop/src/components/chat/messageFolder.ts) `user_with_parts`、[MessageRefBlock.tsx](../../../apps/acowork-desktop/src/components/chat/MessageRefBlock.tsx)、`chatStore.convertConversationEntry`（保留部件 metadata） | 折叠 + 折叠卡片 markdown；`messageRefPart.test.tsx` 钉 7 条契约（折叠、字面、默认收起、展开 markdown、无原话仍渲染、载入链保留 metadata、附件不回归） |
