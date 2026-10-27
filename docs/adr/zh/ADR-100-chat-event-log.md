# ADR-100：Chat 事件日志化——移除执行状态卡片，在途轮次落时间点事件行

- **编号**：ADR-100
- **状态**：已接受（Rev. 6，2026-10-27：非 pinned 的本地回合由运行时补齐 chat 坐标，卡片不再丢失，见 D12 补记；Rev. 5，2026-10-24：ADR-099 删除轮转后 `opened_new` 的取值面收敛，见 D11；Rev. 4，2026-10-23：执行视图的输入改走 chat 入口 + 定向直投，见 D12；Rev. 3：路由反应行落盘 + 头像不变量，见 D11；Rev. 2：撤销中间状态落盘，见 D10）
- **日期**：2026-10-22（Rev. 2 / Rev. 3 / Rev. 4：2026-10-23；Rev. 5：2026-10-24；Rev. 6：2026-10-27）
- **决策者**：ACowork 架构组
- **关联**：设计 27（Intent 消息与群聊 §5.5.4 / §5.5.5 / §6）、ADR-099（Chat Router，D3 投递时锚定 pointer）、ADR-086（移动端多会话）、ADR-049（Session 状态子态）、ADR-096（请求路径禁止全量读会话）、ADR-098（统一标准群组模型）

## 摘要

v2.1（ADR-099 同期）在 chat 事件日志之上叠加了一块 **live 状态镜像卡片**（`ExecSessionStatusCard`）：它的视觉状态读时投影自 execution session 的 `session_state`，用来表达"这条消息正在被处理 / 执行到哪一步"。本 ADR 决定**撤销这块卡片**，让 chat 回归**纯时间点事件日志**——在途轮次由固定的 `turn_dispatched` / `turn_started` 事件行如实表达，"仍在执行"由新一轮自己的开始行表达，执行过程细节一律靠事件行上的"查看执行"跳转进 execution 视图，不在 chat 侧镜像。（事件行的**呈现方式**见 D7：turn 指针按"agent 的归属化反应"渲染，而非居中系统通知。）

根因：chat 是 append-only 的 durable 事件日志，live 卡片是在其上叠加的**第二块真相**（数据源为 execution session 状态机），两套真相永远对不齐，导致两类用户可见缺陷。

> **修订 Rev. 2（2026-10-23）**：D2 / D3 / D6 / D7 / D8 把"已派发 / 已开轮"落成了 durable 行，实测仍然错位——对**历史会话**而言，"你的消息到达了 agent""这一轮开跑了"不携带任何读者想要的信息，却在每段历史里堆满状态行；而真正需要解释的"这条消息为什么没有回答"反而由终态行表达。因此中间状态**不再落盘**：Runtime 停止写 `turn_dispatched` / `turn_started` / `router_fallback`，运行中的等待反馈改由前端从"最新人类消息尚无终态回答"**派生**一条聚合状态行；历史里已存在的旧行在渲染期一律隐藏；`aborted` / `failed` / `timeout` / `waiting` 终态保留并改为居中状态行。详见 D10。

> **修订 Rev. 3（2026-10-23）**：Rev. 2 把"派发了 / 开轮了"这类**进度**全部赶出历史，是对的；但它连带删掉了 D8 的**对话反馈**——用户发出消息后，从"{{name}}正在处理…"的派生等待行到最终回复之间（可能是几十分钟到几小时），chat 里没有任何一条 agent 的话。聊天界面的基本交互感是**有来有回**：人类消息之后必须紧跟一条 agent 头像 + 名字 + 气泡，无论内容。因此恢复**一条**（且仅一条）durable 反应行 `turn_routed`：路由结论得出时立即写入，气泡文案为"启动新任务：{任务名}"或"继续之前的任务：{任务名}"——它不是进度，是"你的消息去了哪里"这一**对话事实**的回答，历史读者同样需要。同时确立头像不变量：空正文终态行渲染为居中状态行，其**后**的 agent 气泡不得因它折叠头像。Rev. 2 的其余决定（进度不落盘、派生等待行）不变。详见 D11。

> **修订 Rev. 4（2026-10-23）**：D9 把绑定 chat 的执行会话的**输出**回写进了 chat，但那条回合的**人类消息本身**仍只存在执行会话的 JSONL 里——chat 因此呈现"只有回答、没有提问"的半轮对话。最省事修法（Runtime 以用户身份代写一行）被否决：写入口的身份必须是发起人。Rev. 4 把入口换掉而非补写：**执行视图的 composer 改走 chat 入口**（`POST /api/intents/chat-message`），并携带 `for_execution_session` 指名目标执行会话；Runtime 收到带指名的消息后**跳过 Chat Router** 直投该会话，以 `SessionMessage::ChatMessage`（不是 Intent）进入原执行面同一套处理，因此技能、附件、JSONL 用户行零能力回写。一个过期 id 只多花一次路由，不丢一条消息。详见 D12。

> **修订 Rev. 5（2026-10-24）**：ADR-099 的 2026-10-24 修订删除了执行会话的物理轮转，`chat_dispatch` / `route_intent_session` 的返回值随之从三元收回**二元**（`(session_id, opened_new)`）。D11 的 `opened_new` 语义不变：它仍是"这一轮是否物理新建了会话"的一次性事实，只是取值面不再包含 rotation 分支——reuse（含 `dispatch(existing)` 切换到另一存活会话）报 `false`，仅物理新建报 `true`。`turn_mode` 的两态、durable 写法与渲染契约均不变。

> **修订 Rev. 6（2026-10-27）**：Rev. 4 把"这一轮对 chat 负有哪些义务"记在**客户端的 pin**上，而 pin 的 chat id 来自会话列表行——Router 创建的会话从不发 `session_created`，桌面端可能压根没有那一行。于是执行视图发出的消息落回 `POST /sessions/{sid}/messages`（`surface` 为空），该轮既不在 chat 里留下人类消息，`ask_user_question` / 审批卡片也无处可落：群聊看不见"这一跑停在等什么"，只留着上一次的中断状态。修法不是再加一层客户端判据，而是**由运行时补齐 surface**：会话的 chat 绑定（durable）给 chat id，写入口已验证的调用者（`x-user-id` scope）给 `answerable_by`；`card_target()` 因此只要求"有 chat + 有可答人"，**不再要求触发行**——§5.5.4 的答案通道本来按卡片自身 id 解析，触发行只是渲染上的归属坐标。卡片以 standalone 形态落盘（`in_reply_to` 为空）。仍然缺的是 Rev. 4 的**头行**：Runtime 不能以用户身份写 `user_message`（D12 已否决该做法），非 pinned 回合在 chat 里依旧没有提问行——补那一半要么客户端把 pin 修好，要么改身份模型。

## 背景

### 1. 两套真相的结构性冲突

- **可见性空窗（问题1）**：一条消息被路由进一个正忙的 execution session 后开新一轮，chat 侧的早期 `turn_started` pointer 又被前端刻意隐藏（`AgentChatView` / `GroupChatPanel` 中 `notice_type === "turn_started" return null`），于是"派发后到回复前"这段时间 chat 里毫无反馈。
- **卡片与真相打架（问题2）**：卡片镜像 live session status。用户 stop 当前轮 → `mark_turn_aborted` 判 aborted → 卡片即时消失/显示中断；但被强制加塞的 pending 消息已开新一轮、execute 并未 idle。卡片显示的"已结束"与真实的"还在跑"矛盾。

### 2. 三条链路本就同构

1:1 单聊、人类群聊 @、PM 任务派发经核实走**完全相同的路由链路**（`IntentMessage` → `route_intent_inbound` / `route_intent_legacy` → chat_dispatch 创建/复用 execute session），后端在路由时点已向对应 chat session id 写了早期 pointer，只是前端把它 `return null` 藏掉了。因此"pointer 改为可见事件行"天然同时修复三条链路，**数据侧零新增写入**。

## 决策

### D1 — chat 回归纯事件日志，删除 live 状态镜像卡片

删除 `ExecSessionStatusCard`（及其两端读时投影 `projectLiveTurn` / `projectInFlight` / `live` / `inFlight`）。不再在 chat 侧维护任何 execution session 实时状态的镜像。理由：live 镜像与 durable 事件日志是两套真相，无法对齐；一旦删除卡片，冲突的两类缺陷同时消失，且"仍在执行"这一事实本就应由新一轮的 `turn_started` 行表达，不需要持续态。

### D2 — 在途轮次落最小集两类指针行，均由 Runtime 写

- **派发行 `turn_dispatched`**：Runtime 接受一条 `user_message`（或 PM 的 `request`）并解析出 execution session、写入路由的时点，经唯一聊天写入口追加一行 `system_notice`，`notice_type = turn_dispatched`，携带 `in_reply_to` + `agent_session_id`，幂等 id 前缀 `td:{message_id}`。这是把 ADR-099 D3 里"投递时写入的早期 `turn_started` pointer"**改名**而来（两处同改：`route_intent_legacy` 与 chat_router drain 的 `runner.rs`）。
- **开始执行行 `turn_started`**：该轮真正从队列出队、开跑时写（`session_task.rs`），保持现有 id `ts:{message_id}`，语义收窄为"已开轮"。
- **并存不互斥**：同一 `message_id` 因前缀不同（`td:` / `ts:`）是两个 id，store 按精确 `message_id` 去重；强制加塞 pending 开新一轮时，新一轮的 `turn_started` 如实显示"仍在运行"。
- **终态沿用 `agent_message`**（completed / failed / aborted / waiting），不新增行。

写入方仍是 Runtime，与 ADR-099 D3 同源，不新增第二个写入口，不触碰 Gateway fs 红线（ADR-009 §5）。

### D3 — 事件粒度取最小集，执行过程细节不进 chat

不为"排队 vs 立即执行"再细分第四类行。`turn_dispatched`（路由即写）覆盖问题1 的即时可见反馈，`turn_started`（开轮写）覆盖"仍在跑 / 新轮开始"。工具执行、流式输出、todo 进度等过程细节**不进 chat**（不进事件行、不进 router 上下文 Block B）。execute 视图内的服务端 pending 队列只读镜像（设计 27 §5.5.5）保留不动——那是 execution 侧能力，与 chat 侧 live 卡片无关。

### D4 — "查看执行"跳转，不是状态镜像

每条携带 `agent_session_id` 的事件行（派发行、开始行、以及任何解析出执行会话的 `agent_message`）附一个离散跳转 affordance（脑图标 + "查看执行"），点击 best-effort 打开当轮实际的 execution session（存在性探测 404 → toast 不跳转）。chat 界面不读、不持有 execution session 的实时状态。

### D5 — 交互卡片仍行内可答，三链路共享同一渲染器

`interaction_request`（ask_question / 权限审批）仍是 §5.5.4 的决策卡片，在 chat 流里就地可答，答复走 chat 通道 → Gateway `resolve_card_answer` 继续原轮次。Desktop 抽出两个共享组件 `ChatNoticeEventRow`（事件行）与 `InlineInteractionCard`（行内卡），供 1:1、群聊、PM 派发复用同一语法；Mobile 折叠为单行文本 + "查看执行"跳转，交互项走底部 sheet。

### D6 — 发送侧即时反馈改用前端瞬态占位

派分行比按下发送晚一个往返出现，用**不落库的前端瞬态"发送中"占位**吸收；`turn_dispatched` 落库后替换为持久事件行。占位不入库，因此刷新后若持久行尚未落、占位消失是正确行为。这与 D1 删除 live 卡片不矛盾——瞬态占位是纯前端渲染态，不构成第二块持久真相。

### D7 — 事件行呈现：turn 指针是 agent 的归属化反应，非居中系统行

chat 是对话：用户说完一句，agent 必须"接话"。`turn_dispatched` / `turn_started` 不是频道级系统通知（那类是 `router_fallback` 这种 impersonal 事实），而是**执行它的那个 agent 对这一轮的非文本化反应**（等价于人类回一句"好的，我去看看"）。因此呈现上：

- **归属到 agent**：turn 指针渲染在 agent 那一侧，带其头像 + 名字（1:1 与群聊都显示；群聊尤其必须，否则多 agent 时"不知道谁在处理"）。归属数据 durable 行本就携带（`from` = 执行 agent），只是渲染层此前把它当居中系统行丢掉了。
- **一行一反应（折叠）**：同一 `in_reply_to` 的 `turn_dispatched` + `turn_started` 在渲染期折叠成**一行**，反映最新指针（已开轮显示"我来处理"，仅派发显示"排队中"）。这是 durable 流之上的读时投影，**不是 live 镜像**；JSONL 两行都保留，去重 / 未读 / 上下文 / 乐观标题种子仍读全量。
- **一轮一气泡（并入回复）**：折叠后的反应行若与其**紧邻**的终态 `agent_message` 同属一轮（同一 `in_reply_to` + 同一 `from` + 同一或其一缺失的 `agent_session_id`），则反应行不再独立占行，而是并入该回复气泡**顶部的一句小 caption**——一轮只剩一个头像 / 名字 / "查看执行"，消除"反应行 + 回复气泡"各挂一遍身份与跳转的重复。轮次仍在进行（回复尚未落库）或有卡片插入打断相邻关系时，反应行保持独立显示，作为进行中反馈。同为读时投影（`groupTurnReactions`），durable 行不动；Desktop 两视图与 Mobile 遵循同一配对契约。
- **agent 口吻文案**：措辞从系统黑话（"已派发执行 / 开始执行"）改为 agent 口吻（"收到，排队中" / "好的，我来处理"）。
- **impersonal 回执保持居中**：`router_fallback` 及通用 body/status 仍是安静居中系统行——它们是路由 / 通道事实，不是某个 agent 在说话。
- **纯呈现层**：本决策只改 Desktop / Mobile 渲染与 i18n，不动后端写入口与 durable 语义（D2 / D3 不变）。

### D8 — `turn_started` 反应区分"新任务 / 继续任务"，并命名所属执行会话

D7 的反应是单一"好的，我来处理"，每个轮次读起来都一样，用户无法从 chat 侧分辨"开了个新任务"还是"接着之前的任务做"。本决策把 `turn_started` 反应进一步分成两种口吻，判据由**后端在路由时点的权威判定**给出并落进 durable meta，前端只渲染、不推断：

- **`turn_mode` meta（新增 durable 字段）**：`chat_dispatch` 与 `route_intent_session` 的返回增 `opened_new`（Rev. 5：轮转删除后返回值是 `(session_id, opened_new)` 二元）——reuse（已映射 / 切换到存活会话）报 `false`（同一任务的延续），仅 fresh-create 报 `true`（`Target::New` 恒 `true`）。该值随路由锚点透传进 `SessionMessage::Intent.opened_new`，由 `dispatch_pointer_meta` / `started_pointer_meta` 落成 `"turn_mode": "new" | "continue"`，**同时写进 `td:` 与 `ts:` 两行**（折叠后展示的一般是 `ts:`，两行都带才不丢信息）。它是路由时点的一次性事实，读时不再反推。
- **两态文案**：`turn_started` 且 `turn_mode=new` → `chatView.notice.turnStartedNew`（"好的，启动新任务来处理：{任务名}"）；`turn_mode=continue` → `chatView.notice.turnStartedContinue`（"好的，继续之前的任务：{任务名}"）。`turn_dispatched` 保持中性"收到，排队中"不分裂。
- **任务名解析（呈现层，不动 durable 行）**：优先执行会话自身标题（Desktop `useSessionTitle` 读 live overlay / 快照；Mobile 读 agent store 的 session 列表，"新会话"占位视同空）；标题为空（新建 run 尚未命名）回退到触发消息正文（父层 `in_reply_to → body` 索引，`truncateTaskName` 截断）。两者都解析不出时回退到原单一条 `chatView.notice.turnStarted`，绝不渲染"：（空）"这类无名残句。`ViewRunButton` 的 tooltip 同步显示该任务名。
- **范围与兼容**：Desktop 两视图 + Mobile 反应行，五语 locale 全量补 `turnStartedNew` / `turnStartedContinue`。新增的是**一个 durable meta 字段**，向后兼容——旧 `turn_started` 行无 `turn_mode` 即走单一条，无需迁移。写入方仍是这两个 builder 与两个 dispatch 站点，不新增写入口、不触碰 Gateway fs 红线（ADR-009 §5）。

### D9 — 从 chat 发起的执行会话，其本地续聊回合也必须回写该 chat

一个执行会话若由某条 chat 发起，它就**永久属于**该 chat（`set_chat_binding` 已把 `chat_session_id` 持久化进会话 meta）。但回写在实现上曾是**回合级、由触发它的那条 chat 请求决定**的：chat 路由进来的回合带着"欠 chat 一条回写"（`chat_writeback`），回合结束回写；而用户在**执行会话输入框里本地发起**的回合（典型场景：先手动 stop 掉一个 chat 回合，再在执行会话里直接发新消息续做）没有请求上下文，`chat_writeback=None`，于是它的输出永不回写——chat 停在 `aborted`，后续续聊与最终完成都不可见。

本决策把回写依据从"触发它的消息"扩展为"会话归属"：执行会话只要绑定了 chat，其**本地回合**的最终输出也必须回写该 chat。

- **触发点**：`SessionMessage::ChatMessage` 回合（执行会话 composer 的唯一入口）结束时，若会话 `chat_session_id` 非空且输出非空，往该 chat 追加一条**独立 `agent_message`**。
- **行形状**：无请求可答、无线程可挂，故 `in_reply_to` 为空（standalone）；`meta.message_id = "exec:{本地回合 message_id}"`（幂等键，重投递不重复，且与 `td:` / `ts:` 指针 id 不冲突）；`meta.sender_session_id = 执行会话`（渲染层据此出"查看执行"，无需新增前端逻辑）；`status="completed"`；`to` 为空（广播——群聊全员可见 / 1:1 即对方）。body 走与 §7.1 相同的 `truncate_response_body` 上限。
- **不双写**：chat 路由进来的回合仍走原"请求 / 响应"回写（Intent 分支），本地回合走本决策的回写（ChatMessage 分支），两条分支互斥，同一回合只落一条。
- **边界放开**：`POST /api/intents/chat-append` 的 kind 白名单由 `interaction_request | system_notice` 扩为三者，新增 `agent_message`（写入方身份仍由 node token 校验、`from` 必须是会话参与者、`meta.message_id` 仍必填）。store 侧 `IntentMeta` 字段已齐，无需改存储。
- **best-effort**：回写失败只记日志、不影响回合；缺的是一条 surface 行，不是对话本身。
- **调试重放不另立行**：DevMode 的 resume / step 重放（ADR-013）继续一个被暂停的**同一回合**，不是用户发起的新回合；`exec:{message_id}` 按回合幂等，故重放不追加第二行——该调试流程的执行面仍是真相来源。
- **被 stop 的本地回合不落行**（已被 D10 取代）：只有产出最终输出才回写。本地回合被 stop 时执行面照常显示中断，但 chat 不追加空行；chat 里的“已中断”仍只来自 chat 路由回合的 `aborted` 回写（§7.1），因此本地反复 stop 不会在 chat 里累积中断提示。

### D10 — 修订（Rev. 2）：中间状态不落盘，历史只保留终态

D2 / D3 / D6 / D7 / D8 的共同前提是“在途轮次必须落成 durable 行”。实测这个前提在半条链路上是错的：**运行中的反馈需要实时性，而历史阅读不需要进度**。一条消息“已被派发”“这一轮开跑了”是执行面的运行时事实，对几个月后翻聊天记录的人为零信息量；落盘反而是把进度日志混进了对话日志。因此本轮把“实时状态”与“对话历史”彻底拆开：

1. **停写中间状态。**Runtime 不再写 `turn_dispatched` / `turn_started` / `router_fallback` 三类行，并删除 `dispatch_pointer_meta` / `started_pointer_meta` 两个 builder 及其三个写入站点（chat_router drain 派发、`route_intent_legacy` 派发、`session_task` 开轮）；`turn_mode` durable meta（D8）随之废止，不再产生新值。store 仍能反序列化旧行（字段保留），但不再新增。
2. **历史行渲染期隐藏。**已存在的旧 `turn_dispatched` / `turn_started` / `router_fallback` 行不迁移、不删除，但在 Desktop 两视图与 Mobile 一律不渲染（`ChatNoticeEventRow` 的 `RETIRED_NOTICES`）。刷新、换设备、旧备份都不会再把状态行刷回屏幕。`ChatNoticeEventRow` 随后只负责真正的频道回执（`budget_exhausted` / `delivery_failed` / `truncated` / `unavailable` 等）。
3. **运行中由前端派生一条聚合等待行（不落盘）。**判据是唯一 durable 的证据：**最新一条已向 agent 发出请求的 `user_message` 尚无终态回答**。"这条人类行是否真的惊动了 agent"同样是 durable 行事实，不是猜测：`/api/intents/chat-message` 落的行 `to` 就是它投递的 agent（1:1、答卡、pinned 执行回合都走这里），§5.3.1 的群 @ 群发只落一条广播行（`to` 空）而把被点名的 agent 放在 `meta.mentions`；除此之外——`user_user` 私聊、群里未 @ 的普通发言——走的是仅归档的 chat 写入口，`to` 为空且没有 mentions，它谁也没唤醒。等待行断言的是"有 agent 在干活"，因此**没唤醒任何 agent 的人类行既不能开启等待、也不作为"更新的人类消息"顶掉前一回合**（一个未 @ 的插话不取消正在跑的任务）；反之，一个没有任何 agent 参与者的 chat（私聊、纯人类群）永不显示等待行，连 `sending` 窗口也不——那里没有任何人会来回话。`pendingTurn(rows, sending, closedCards, agentReachable)`：`agentReachable=false` 直接为假；HTTP 发送窗口（`sending`）短路为真；否则倒序取最新一条**开启 agent 回合的**人类行，若其后出现 answering 的终态 `agent_message`（`in_reply_to` 匹配）、或一张未关闭的 `interaction_request`（球在人类）、或更新的一条同样开启了回合的人类消息，则结束等待。有 agent 名时渲染 `{{name}}正在处理…`（带 shimmer），否则 generic；不区分排队 / 开轮 / 工具执行，不新增 MQTT 通道、不改会话列表 API。窗口被 locate 定位到中段（后面还有更新页）时不渲染。
4. **终态保留，空终态改居中。**`aborted` / `failed` / `timeout` / `waiting` 的 `agent_message` 继续写、继续存（它们回答的是“为什么没有回复”，正是历史需要的信息）。空 body（且无附件）的终态不再渲染成 agent 气泡——状态词不是 agent 说的话——改为居中状态行（`aborted` 中性、`failed` / `timeout` 红色），并保留“查看执行”；有 body 的终态仍是普通气泡。
5. **本地回合被 stop 落 `aborted` 终态行（取代 D9 末条）。**一个绑定 chat 的执行会话，其本地回合被 stop 时不再静默：写一条空 body 的 `agent_message`（`exec:{turn_id}` 幂等、`status="aborted"`、`sender_session_id`），让 chat 能解释“这条消息为什么没有回答”。重复 stop 由幂等键去重，不会累积提示。
6. **呈现与范围。**Desktop `AgentChatView` / `GroupChatPanel` 与 Mobile `AgentChatScreen` 共用同一组规则与同样的居中状态行（Mobile 自带 shimmer）；五语 locale 新增 `pendingTurn` / `pendingTurnGeneric` / `interrupted` / `failed` / `timeout` / `waiting` / `noAnswer`，删除 `emptyReply` 与四个旧 notice key（`turnDispatched` / `turnStarted` / `turnStartedNew` / `turnStartedContinue`）；`routerFallback` 键仅为旧回执兜底保留，实际渲染已隐藏。

**为什么不是“落盘但渲染隐藏”。**那就把进程内状态换成了永久垃圾：写进去就得考虑截断、压缩、上下文过滤、未读计数、导出与备份，而它一无对话价值；且仍需要前端 live 信号才能做到“发出去立刻有反馈”。不落盘从根上消除了两侧同步问题。

**与 D1 的关系。**D1 删除的是 chat 侧对 execution 状态的**读时镜像**；D10 进一步删除 chat 侧对执行进度的**写入**。两者合起来才是完整的“chat = 对话历史”：历史只记说过的话与未得到回答的事实，进度只在运行时、只在当前窗口里存在。

### D11 — 修订（Rev. 3）：路由反应行 `turn_routed` 落盘，并确立头像不变量

Rev. 2 把"派发了 / 开轮了"这类**进度**赶出历史是正确的，但它连带取消了 D8 的**对话反馈**，实测留下一个更基本的缺口：用户发出消息后，chat 里只有居中的派生等待行（"{{name}}正在处理…"），从路由结论得出到最终回复之间（几十分钟到几小时）没有任何一条 agent 的话。聊天界面的交互感来自**有来有回**——人类气泡之后必须紧跟 agent 头像 + 名字 + 气泡；缺了这条即时反应，用户无法确认消息被接住、更不知道它去了哪个任务（本 ADR 落地当天即发生的事故：一条新话题消息被降级路由塞进不相关的历史会话，chat 界面全程只有"正在处理"，无任何下文）。

1. **恢复且仅恢复一条 durable 反应行。**Runtime 在路由目标解析成功、intent 投递之后，写一条 `system_notice`，meta 由共享 builder `turn_routed_meta` 构造：`message_id = "tr:{request_id}"`（按触发行幂等，重投递不重复）、`in_reply_to = {request_id}`（线程挂到触发的人类消息下——这正是"紧跟用户消息"的判据）、`agent_session_id`（可下钻）、`turn_mode = new | continue`（路由时一次性事实：仅物理新建为 `new`，复用已有会话（含映射不变时的原地追加）均为 `continue`——同一逻辑任务在继续）、`notice_type = "turn_routed"`。body 恒为空：文案是界面语言的事，由渲染层从 i18n 出，不把用户语言烙进 durable 数据。写入点两处：chat_router drain 的 `dispatch` 成功分支（覆盖 1:1 / 群聊 @ / 降级前的正常路由），以及 `route_intent_legacy` 中"欠 chat 一个回写"的回合（覆盖 router 关闭时的人类消息与 PM 派发），与 Rev. 2 移除的三个写入点相比只剩这一条。
2. **它不是进度，是对话事实。**"你的消息被派发了"对历史读者是零信息量的进度；"**这条消息由哪个任务处理、是新任务还是旧任务的继续**"是会话本身的事实——几个月后翻记录的人需要知道这条消息的去向。这正是 Rev. 2"历史不记进度"原则与对话价值之间的边界，`turn_routed` 落在价值一侧。派生等待行（D10.3）不因此撤销：反应行落地后运行仍在进行，等待行继续覆盖到终态，两者职责不同（一个是 agent 的回答，一个是运行时状态）。
3. **渲染为 agent 归属气泡，永不居中。**Desktop `AgentChatView` / `GroupChatPanel` 与 Mobile `AgentChatScreen` 把 `turn_routed` 行合成进普通 agent 气泡路径：头像 + 名字 + 气泡，文案 `启动新任务：{{task}}` / `继续之前的任务：{{task}}`（五语 locale key `chatView.notice.turnRoutedNew` / `turnRoutedContinue`）。任务名取执行会话标题，未生成时回退触发消息正文（截断），与 D8 的命名规则一致。`RETIRED_NOTICES` 不变——`turn_dispatched` / `turn_started` / `router_fallback` 仍然永不渲染。
4. **头像不变量。**凡真正渲染成 agent 气泡的行必须携带头像 + 名字归属；判定"上一条已是气泡从而折叠本条头像"时，**空正文终态行不算气泡**（它是居中状态行，D10.4），`turn_routed` 反应行与其后的真实回复各自独立带头像（两者间隔可能以小时计，是同一条消息的"接住"与"回答"，不是一条发言的续行）。Rev. 2 后 `AgentChatView` 的 `showHeader` 判据只看 `kind === "agent_message"`，把居中终态行误算成气泡，导致其后的回复孤零零无头像——本条把判据钉死为"上一条**渲染为**气泡"。执行会话视图同理：用户消息后紧跟的 agent 消息必须带头像。

**为什么不干脆用带文案的 durable 行。**把"启动新任务：xxx"烙进 body 意味着 durable 数据携带界面语言、任务名在标题迟到时要么写错要么补写（append-only 不许），而 `turn_mode` + 触发正文这两个 durable 事实已足够让渲染层在任意时刻合成正确文案。

**与 D10 的关系。**D10 撤销的是**进度**落盘（派发 / 开轮 / 降级回执），D11 恢复的是**对话反馈**（消息去向）——前者对历史读者零价值，后者有；判据始终是"读者几个月后重看这条消息时是否想知道"。

### D12 — 修订（Rev. 4）：执行视图的输入改走 chat 入口，并以定向直投跳过 router

D9 让绑定 chat 的执行会话把**本地回合的输出**回写进 chat，但那条**人类消息本身**仍只在执行会话的 JSONL 里——chat 于是呈现"只有回答、没有提问"的半轮对话，几个月后翻记录的人看不到任务是被什么话开起来的。补上它最省事的写法是让 Runtime 以**用户身份**替 chat 写一行，这条路被否决：写入口的身份必须是发起人的身份，代写等于把"谁说的"变成实现细节。因此本决策把入口本身换掉——**人在执行视图里说的话，从 chat 入口进去**。

1. **入口换掉，目标不换。**执行会话若绑定了 chat（`SessionInfo.chat_session_id`），其 composer 的发送改走 `POST /api/intents/chat-message`，携带 `for_execution_session = 该执行会话`。Gateway 以调用者身份落一条 `user_message`（这一轮的头），Runtime 收到后**跳过 Chat Router** 直投指名会话（这一轮的尾）。"谁来回话"由人在哪个视图里敲定，不需要一次路由 LLM 调用来重新发现它已经确定的事实。绑定的 chat 是项目群时（PM 任务的执行会话绑的正是派发它的那个群，ADR-099），群里不止一个 agent，目标无法由 chat 本身推出，因此请求另带 `pinned_agent` 指名拥有该执行会话的 instance——它是**被校验的声明，不是信任**：该 instance 必须是本 chat 的 agent 参与者，否则 403 且不写任何行（这一轮的回答会落进这个 chat，指名的会话就得属于它）。未带 pin 的群发仍走 §5.3 的归档入口，本决策没有为它开门。
2. **直投投的是 `SessionMessage::ChatMessage`，不是 `SessionMessage::Intent`。**Intent 是 agent↔agent 的协议形状，没有 `active_skill` / `content_parts` 两臂（`attached_items` 已于第 8 条补齐）；投它就等于把执行 composer 的能力削掉一截。投 ChatMessage 使直投回合与执行面原路径**逐项等价**：slash 技能仍由 Runtime 按 `SkillRegistry` 解析（指令永不信任客户端）、附件仍按 ADR-046 联合类型落到本会话 `files/`、JSONL 仍原生记录用户行。零能力回退是这条选择的直接结果，不是额外的兼容层。
3. **pin 是提示，不是闸门。**目标会话未加载 → Runtime 清空 hint、落回正常路由（一次路由调用的代价，不是丢消息）。"在本实例里已加载"还不构成归属：一个 agent 实例服务多个 chat，而 session id 会出现在别人读得到的行上（群面的 `meta.agent_session_id` 对全体群成员可见），所以接收侧 Runtime 还要校验被指名会话绑定的 chat **等于**本次发送的来源 chat，不符同样清空 hint 落回正常路由。少了这一道，任何人都能把文本、slash 技能与 node-local 附件路径注入别人正在跑的任务，而那个任务的回答会以 agent 身份写进受害会话的 chat、问话人自己的 chat 只留下一条从没在那里跑过的头行。空内容（无文本、无附件、无 content_parts）的定向发送直接判为已完成：既不唤醒会话后 WARN 丢弃，也不让 router 白烧一次调用；Gateway 已落的 chat 行仍如实记录"人发了什么"。
4. **客户端行 id 让三处对齐一条。**执行视图先为乐观气泡 mint `message_id`，同一 id 既落 chat 行（`is_valid_row_id`：1–64 个 `[A-Za-z0-9_-]`，越界即 400，不做静默规范化）也落执行会话 JSONL 行。因此**乐观插入、回滚、reconcile 全部不动**，两条发送路径自然收敛到同一 id。
5. **回写线程化，读作一轮。**`exec_echo_meta` 新增第 4 参 `trigger_message_id`：pinned 回合的终态 `agent_message` 携 `in_reply_to = 该 chat 行 id`。于是 D10.3 的派生等待行判为"已回答"并消失，chat 把这条消息读成完整一轮（此处所问 + 它带回来的答案）。非 pinned（执行面原路径、以及 chat 自己的 composer）仍写 standalone 的 `exec:` 行，不带 `in_reply_to`——它不欠任何可见行一个回答。
6. **附件不跨系统搬运。**字节永不骑 MQTT（`/api/intents/chat-attachments/…` 流式端点就是为这条纪律存在的），而 node-local 路径在 Gateway 与 Node 分机部署时必然 5xx（ADR-009 §5 红线）。因此 chat 行只在 `meta.exec_attachments` 里带**显示名**（上传臂取 `filename`、工作区臂取 `name`，只给了路径时才退到 basename），不带 documentId、不带路径、不带可打开的引用；执行面仍是附件唯一能真正打开的地方。这不是妥协——把一条打不开的引用写进 chat 比不写更糟。落到实现上有两条硬约束：chat 存储的 `IntentMeta` 是强类型结构，**未声明的 meta 键会被 serde 静默丢弃**，所以这个字段必须在存储层声明，否则投影写了也落不了盘；同时存储对 `user_message` 的"必须有内容"判据认这枚标签为内容——"只交文件不发消息"是执行面合法的回合，拒掉它就等于把执行面接受的回合的头一行丢掉。渲染侧两端都把这枚标签画成**不可点的静音 pill**（与可下载的 chat 附件芯片视觉区分），且标签自身就算内容，因此不会退化成一条空白气泡。
7. **`command` 只在带 pin 时被采纳，`attached_items` 两条路径都采纳。**`command` 是 slash 技能名，而 routed 回合被消费成 Intent，Intent 没有 `active_skill` 臂——因此无 pin 时置空而非 400（人写的正文仍一字不差）。`attached_items` 不同：Intent 已补上这一臂（见第 8 条），chat 面的工作区引用与执行面走同一条 ADR-046 落盘路径。带 pin 时两者仍受上限护栏约束：`command` ≤128 字符（是技能名，不是指令）、`attached_items` ≤32 条且序列化后 ≤32KB。
8. **chat 面的 "Add to Chat" 落在 chat 会话自己的待发引用列表里。**右侧工作区被两个视图共用，而每个 composer 有自己的附件清单：执行面挂在执行会话（`chatStore.attachedContext`），chat 面挂在 chat 会话（`chatSessionStore.attachedContext`）。写进看不见的那个，与什么都没做无法区分——这正是它当初被报成 bug 的原因。引用是 node-local 路径，由接手的执行会话用自己的工具实时读取，不是文件副本（副本是 composer 上那枚回形针的职责），因此它照 ADR-046 union 进 `attached_items`，Gateway 两条路径都转发给本轮实际接手的执行会话。发送失败的回合不清空芯片。
8. **前端只认服务端事实。**Desktop / Mobile 的 exec composer 按 `chat_session_id` 是否有值分流；无绑定、或本地模式（无账号，chat 通道是身份通道，硬走只会 401）保持原 `POST /sessions/{sid}/messages`。绑定值只读自 Runtime 暴露的会话行，前端不发明、不缓存、不改写。
9. **每一条出口都欠一行终态。**头行既然每次都写，尾行就不能只在"模型有话可说"时才写：完成 / 被 stop / 附件读不出来 / agent loop 报错四条出口都以同一 `trigger` 线程化落一条 `agent_message`，`status` 取 `completed` / `aborted` / `failed`；`failed` 的正文只用人可读单行，detail 与 error_type 不进历史。否则一轮从那一刻起在 chat 里永远显示"处理中"，真相只在执行视图里。D9 原有的"普通空答案不留行"只保留在非 pinned 路径上——它本就没有 chat 问题要关闭。
10. **尾行同时关闭 mailbox 里的请求。**Gateway 投递这条 `user_message` 时在 mailbox 记一条未答请求（§7.1 的超时兜底靠它），而 pinned 回合不走 Intent 写回，没有别的东西来关它；因此 `chat-append` 在 `kind=agent_message` 且 `meta.in_reply_to` 非空时，于落库成功之后 `mark_answered(in_reply_to)`——顺序与 Intent 写回一致，先有答案再有关闭。缺这一步，一轮即便答得完好，也会在响应超时（默认 3600s，或 responder 离线满 60s 的存活宽限）之后由 sweeper 在同一 chat 追加一条 `fallback-{message_id}` 的 `failed` 回执，`in_reply_to` 指向那条早已回答的头行：用户看到的是"我问了、它答了、一小时后它又说没收到回答"。同一 relay 上的决策卡片与决议通知各带自己的 `in_reply_to`（执行侧 request id / 自身卡片），不是 chat 请求，因此不关闭任何东西。

**为什么不复用"执行回合结束后再补写一条 chat 行"。**那仍是代写，只是将伪造点后移：写入方是 Runtime、身份是用户，且要额外解决与投递结果的时序竞态。走 chat 入口则落库先于投递（§5.1）本就是既有契约，无需新增写入方。

**与 D9 的关系。**D9 补的是**尾**（本地回合的输出要回写），D12 补的是**头**（那条回合的人类消息要在场）。D9 的回写契约不改：非 pinned 的本地回合仍写 standalone `exec:` 行；pinned 只多带一个 `in_reply_to`。

**与 D10 / D11 的关系。**D10 取消 durable 进度行、D11 恢复一条对话反应行；D12 不新增任何行类型，只是把已存在的 `user_message` 从执行面搬进 chat 面。pinned 回合也不写 `turn_routed`——它没经过路由，没有"去了哪个任务"可宣布，人就在看着它跑。

### D13 — 修订（Rev. 5）：回合对 chat 的义务收敛为单一 `TurnSurface`；两条传输仍不合一

D12 把 pinned 回合投成 `SessionMessage::ChatMessage`，转换时只从信封取走 `content` 与 `message_id`——`source_chat_session_id`、`from`、`has_human` 三个字段在换传输的那一刻被丢弃。它们不是投递细节，而是**这一轮对 chat 负有哪些义务**的事实：交互卡片落在哪个 chat、挂在哪个行 id 上、谁能答、这一轮能不能停下来等人、终态行回显到哪里。于是同一句话在两个入口得到两种行为：执行视图里 `ask_user_question` 弹框正常，群聊里**根本没有卡片**（卡片写入的判据是 `chat_writeback.is_some()`，而 pinned 分支从不设置它）；工具审批卡片同样消失；撞到迭代上限 / 循环检测的 pinned 回合既不写 pause 卡片也不落 `waiting`，直接以 `aborted` 收场；可答性判断退化成猜。五个症状一个根因：**信封上的一等事实在换传输时被降级成"这里没有 chat"**，而消费端把"没有 write-back"当成了"没有 chat"的替身判据。

1. **`TurnSurface{chat_session_id, trigger_message_id, answerable_by, has_human}` 是回合对 chat 义务的唯一载体。**两条传输都在投递点由同一个产出函数生成（信封原样在场时用 `TurnSurface::from_envelope`，已解构处用 `TurnSurface::new`，规则只写一遍），消费端——卡片落点与 `to` / `in_reply_to`、审批与提问的可见性和阻塞闸门、pause + `waiting`、终态回显、pending 指针——只读这一个值，不得再从 session 绑定或 write-back 上下文里推断。强制点是编译期的：新的传输或派发场景必须显式产出一个 `TurnSurface`（或写明它为何不欠 chat 任何义务），否则拿不到这些字段。
2. **`chat_writeback` 回归本义。**它只表示 §7.1 那条回给发起 agent 的 response 通道。它曾兼职当"这一轮有没有 chat"的替身，正是这个替身让 pinned 分支静默失效。`IntentTurnContext` 随之瘦身：收拢后 `conversation_id`、`source_chat_session_id`、`outcome` 三个字段已无任何读取方，连同 Intent 传输上同样无人再读的 `conversation_id` 一并删除。
3. **`TurnOutcome` 取代两面镜子。**"这一轮怎么结束的"此前同时存在于 `AgentLoop.turn_aborted: bool`（回显读）与 `IntentTurnContext.outcome: String`（write-back 读），而 pinned 分支只有前者，因此**为等待人类而暂停的回合被回显成 `aborted`**。现在循环上只有一个 `turn_outcome` 字段，两个终态写入方读同一个值，`completed / aborted / waiting / failed` 在两处一致。
4. **回合收尾合并成一个函数。**`end_turn_obligations` 由两条传输共用：交还 §7.1 上下文、关闭本轮仍未被回答的 `approval` / `ask_question` 卡片（§5.5.4 安全网；`pause` 卡片按设计活到下一轮，由回答它的那一轮关闭）、再把 Chat 事实清零以免下一轮继承。pinned 分支过去不经过这段收尾（它当时写不出卡片，也就无需关闭）；修好卡片之后它**必须**经过，否则卡片会永远停在可点状态。
5. **已评估并否决：把两条传输合一（pinned 也投 `SessionMessage::Intent`）。**D12.2 的分歧是真的：附件模型一边是 ADR-046 判别联合（写进本会话 `files/`、node-local 路径），一边是 `chat_attachment_ids`（跨进程 HTTP 拉取）；per-turn skill 注入只存在于 ChatMessage 臂；渲染上人话对信封串；角色上 initiator 对可能的 responder；收尾上 `exec:` 独立行对 §7.1 write-back。合一要么给 Intent 补三个可空字段并在臂内到处 `if is_pinned`（约 700 行带模式标志的单臂，总行数不降反升），要么在运行时和解两套附件语义——后者本身是另一场尚未开始的讨论。它还牵动 mailbox / sweeper / response target 的跨进程语义，需要 Gateway + Runtime + PM 三边同改并重新论证混部兼容。本决策已把"必须显式产出 surface"变成编译期强制点，合一剩下的收益只有"enum 少一个变体"。**重新考虑的触发条件**：出现第 3 条传输；或两套附件模型统一（`AttachedItem` 与 `chat_attachment_ids` 合一）时。
6. **影响面仅 Runtime 内部，并有一处有意的行为扩张。**Gateway 未改：这些事实本就在信封上（D12 起 pinned 发送即 `has_human: Some(true)`），丢失发生在 Runtime 的转换处。agent↔agent 请求的消费语义不变（pair session 的卡片仍写、`has_human = Some(false)` 仍禁止阻塞）。扩张在 `notify`：fire-and-forget 回合过去因 `chat_writeback` 为空而不写卡片，现按同一判据（绑定了 chat、且有可答的发送人）会写。这是修正而非新策略——问题既然落在群里就该看得见，回合结束时未答的卡片由第 4 条关闭，不留死按钮。

## 验收标准

> 1–8 为 Rev. 1 的验收、9–13 为 Rev. 2 的验收（均保留作历史记录）；当前验收以下方 9–17 为准（14–16 为 Rev. 3 新增，17 为 Rev. 4 新增，与 9–13 并存——Rev. 3 只恢复 `turn_routed` 一条反应行，Rev. 4 不新增任何行类型，都不撤销 Rev. 2 的进度不落盘）。

1. **问题1 消失**：消息发出后、回复落库前，chat 里至少有一条 `turn_dispatched` 事件行可见（1:1 / 群聊 @ / PM 派发三链路一致）。
2. **问题2 消失**：stop 当前轮后，被加塞的 pending 开新一轮时，chat 出现新一轮的 `turn_started` 事件行如实表达"仍在运行"，不再有无状态的持续态卡片与真相打架。
3. **双写去重**：`turn_dispatched`（id `td:`）与 `turn_started`（id `ts:`）并存不互斥。两条 pointer 行的 meta 分别由共享 builder `dispatch_pointer_meta`（两处派发写入点：chat_router drain 与 `route_intent_legacy` 同调）与 `started_pointer_meta`（开轮）构造；单测锁定这两个 builder 的 `td:`/`turn_dispatched` 与 `ts:`/`turn_started` 字面量（改回任一旧值即失败），store 层单测验证两个 id 各存一行且各自幂等。三链路（1:1 / 群聊 @ 经 chat_router drain、PM 派发经 `route_intent_legacy`）都经这两个 builder 落地，故共享同一去重契约、无需各自一条集成测试。
4. **无 live 投影残留**：两端 store 不再有 `projectLiveTurn` / `projectInFlight` / `live` / `inFlight`；仅保留 card receipt 所需的 `closedCardIds` / `humanAnsweredCardIds`。
5. **跳转可用**：事件行与 `agent_message` 的"查看执行"跳转对 PM 任务会话天然可用（`agent_session_id` 在 pointer meta 里，依 ADR-099 D5 绑定到群）。
6. **呈现归属化（D7）**：turn 指针在 1:1 与群聊都渲染为 agent 侧、带头像 / 名字的归属反应行；同一轮的派发 + 开始折叠为一行（`hideSupersededDispatchIds` 读时投影，Desktop / Mobile 同一契约）；紧邻回复落库时反应行并入回复气泡作 caption、一轮只剩一个"查看执行"（`groupTurnReactions` 读时配对，回复未到时反应行独立显示）；`router_fallback` 仍居中、无归属。
7. **反应区分新/继续（D8）**：`turn_started` 反应依 durable `turn_mode` 渲染"启动新任务"或"继续之前的任务"并命名执行会话（标题为空回退触发正文，都无则回退单一条）；`turn_dispatched` 恒为中性"收到，排队中"；`chat_dispatch` / `route_intent_session` 对 reuse 报 `opened_new=false`、仅 fresh-create 报 `true`（builder 单测与 dispatch 单测锁定）。
8. **续聊回写（D9）**：chat 发起的执行会话中，用户在执行会话 composer 本地发起的回合（含 stop 之后的续做）结束时，其最终输出以 standalone `agent_message` 回写到该 chat（`exec:{message_id}` 幂等、`sender_session_id` 可下钻、广播）；chat 路由回合仍走请求 / 响应回写、不双写；`chat-append` 接受 `agent_message`、仍拒绝 `user_message` 与未知 kind。
9. **中间状态不落盘**：新建一轮（1:1 / 群聊 @ / PM 派发）并等到终态后，chat 日志不出现 `turn_dispatched` / `turn_started` / `router_fallback` 行；两个 pointer builder 已下线，旧 builder 单测改为断言“不再写入”。
10. **历史隐藏**：人为构造携带三类旧 notice 的窗口，Desktop 两视图与 Mobile 均不渲染任何文本；真正的频道回执（`budget_exhausted` / `delivery_failed` / `truncated` / `unavailable`）仍渲染。
11. **等待行**：仅对真正惊动了 agent 的回合显示——`user_user` 私聊（无 agent 参与者）打开即不显示，发送窗口内也不显示；群里未 @ 任何 agent 的普通发言不显示；1:1、群 @（单条广播行 + `meta.mentions`）、pinned 执行回合照常显示。发送窗口内（`sending=true`，且该 chat 有 agent）即使窗口为空也显示 generic 等待行；最新的开启回合人类行未被终态回答时显示 `{{name}}正在处理…`；终态落地、卡片打开、或出现更新的**开启回合的**人类行后消失；一个未 @ 的插话落在进行中的回合之后时等待行仍在；locate 到中段（`lastPos < total - 1`）不显示。
12. **终态居中**：空 body 的 `aborted` / `failed` / `timeout` / `waiting` 渲染为居中状态行（`aborted` 非红、`failed` / `timeout` 红）并带“查看执行”，不出 agent 气泡；有 body 的终态仍是普通气泡。
13. **本地 stop**：绑定 chat 的执行会话其本地回合被 stop 后，chat 出现一条 `exec:` 幂等的 `aborted` 空行；重复 stop 不重复。
14. **路由反应行（D11）**：任何欠 chat 回写的路由回合（1:1 / 群聊 @ / PM 派发 / router 关闭时的人类消息 / 降级新建）投递成功后，chat 出现一条 `tr:{request_id}` 的 `turn_routed` 行，meta 携 `in_reply_to` / `agent_session_id` / `turn_mode`，body 为空；信封重投递不产生第二条；三端渲染为 agent 头像 + 名字 + 气泡（"启动新任务：X" / "继续之前的任务：X"），永不渲染为居中行；等待行在其后继续存在直至终态。
15. **头像不变量（D11）**：空正文终态行（居中状态行）之后的 agent 回复必须重新带头像 + 名字；`turn_routed` 与其后的真实回复各自独立带头像。
16. **降级新建（ADR-099 修订联动）**：router 降级（`Fallback`）的 chat 人类消息不再复用 intent_map 映射的历史会话，改经 `chat_dispatch(Target::New)` 新建并绑定同一 chat、重指向映射；`PassThrough`（PM / agent-agent / response / router 关闭）的确定性复用不变。
17. **执行视图走 chat 入口（D12）**：绑定 chat 的执行会话，其 composer 的发送落 `/api/intents/chat-message` 并携 `for_execution_session` 与客户端 `message_id`；chat 里出现一条**该用户身份**的 `user_message`，其后同一执行会话的终态 `agent_message` 携 `in_reply_to` 指向它，等待行随之消失；执行会话 JSONL 里同一 `message_id` 只有一条用户行；未绑定或无账号时仍走 `POST /sessions/{sid}/messages`，chat 不增行；pin 指向未加载会话时消息仍被投递（走正常路由）；pinned 回合的 slash 技能与 `attached_items` 生效，chat 行只出现显示名而不出现路径或 documentId。
18. **D12 的四个失败面（修复轮补锁）**：① 绑定群 chat 的执行会话其定向发送返 200 且头行落进群、`to` 是 `pinned_agent` 指名的 instance，指名一个非本 chat 参与者时返 403 且不写任何行，未带 pin 的群发仍返 400；② pin 指名一个**绑到别的 chat** 的已加载会话时，该回合并不在别人的任务里跑，而是落回发送者自己 chat 的正常路由；③ 回合以报错或附件读取失败收尾时，chat 里仍出现一条 `status=failed` 的终态行指向那条头行（等待行不会永久悬挂）；④ 尾行落库后 mailbox 里那条 `user_message` 请求转为已回答，`GET /api/intents/{message_id}` 报 `answered=true`，sweeper 不再对它产生 `fallback-` 回执。

## 兼容与迁移

- 不迁移历史：既有 `turn_started` 行仍有效，语义收窄为"已开轮"，与新增的 `turn_dispatched` 行并列渲染。
- 数据侧对 PM 派发零改动：其早期 pointer 正是要改名 `turn_dispatched` 的那条 `route_intent_legacy` 写入。
- 事件行文案为固定 i18n key（`chatView.notice.turnDispatched` / `chatView.notice.turnStarted`），body 存人可读短文本，meta 存 `agent_session_id` 供跳转，与现有 pointer 一致。D7 只改这两个 key 的取值（改 agent 口吻）与渲染归属，key 与 pointer 契约不变。D8 在此之上新增 `turn_mode` meta 字段与 `chatView.notice.turnStartedNew` / `chatView.notice.turnStartedContinue` 两个 key（带 `{{name}}` 插值）——`turn_started` 反应在有任务名时按 `turn_mode` 二选一，否则回退 `chatView.notice.turnStarted`；`turn_dispatched` 契约不变。
- D9 扩展回写依据而不改既有契约：chat 路由回合的请求 / 响应回写不变；新增的是"chat 绑定会话的本地回合"这一条回写来源与 `chat-append` 的一个 kind。历史 `aborted` 行照旧；修复前已发生的"续聊丢失"不追溯（那些回合本就没有落过 chat 行）。
- 保留 execute session 视图（含其 pending 队列只读镜像与 todo 进度），本次只改 chat 侧呈现。

**Rev. 2（D10）：**

- **不迁移历史**：旧 `turn_dispatched` / `turn_started` / `router_fallback` 行留在原文件，不删除、不改写，渲染期一律隐藏；`turn_mode`（D8）字段保留可反序列化，不再产生新值。历史未读数不回溯修正。
- **等待行不需要迁移**：它完全由 durable 行派生——刷新后若最新人类消息仍无终态回答，`pendingTurn` 会重新给出同一行；若已有终态行，它自然消失。
- **旧回执仍可见**：`budget_exhausted` / `delivery_failed` / `truncated` / `unavailable` 等真正的频道回执不受影响。

**Rev. 3（D11）：**

- **只新增一种行，不迁移旧行**：`turn_routed` 是 Rev. 3 起新写入的唯一反应行；Rev. 2 隐藏的 `turn_dispatched` / `turn_started` / `router_fallback` 继续隐藏，不回填、不复活。Rev. 1 时代携带 `turn_mode` 的旧 `turn_started` 行仍走 `RETIRED_NOTICES` 隐藏路径，不映射成新气泡。
- **白名单不变**：`chat-append` 已接受 `system_notice`（Rev. 1 起），`turn_routed` 复用该 kind，Gateway 无改动。
- **文案不进 durable 数据**：五语 locale 新增 `chatView.notice.turnRoutedNew` / `turnRoutedContinue`（带 `{{task}}` 插值）；Mobile 无 i18n 体系，按现有惯例硬编码同一中文文案。
- **降级语义修订在 ADR-099 侧**：`Fallback` 对 chat 人类消息改新建会话（见 ADR-099 修订），本 ADR 只约定反应行在两条路径（router drain / legacy 降级）下写的是同一个 `turn_routed_meta`。

**Rev. 4（D12）：**

- **不改任何既有行的形状**：`user_message` / `agent_message` 的字段与 kind 都不变，pinned 回合只是多了一个来源（执行视图 composer）与一个 `in_reply_to`。历史数据无需迁移，旧执行会话不会因为升级而多出一行或少一行。
- **`for_execution_session` 是可选字段**：不带它的 `chat-message` 请求行为完全不变（chat 自己的 composer 走的就是这条），因此 Gateway / Runtime / 前端可以任意顺序发布，不存在版本锁。
- **`meta.exec_attachments` 是新 meta 键**：读侧不认识它时直接忽略，最坏情况是 chat 里看不到附件显示名，不影响解析与渲染。
- **无绑定即无变化**：未绑定 chat 的执行会话、本地模式（无账号）仍走原 `POST /sessions/{sid}/messages`，链路与升级前逐字节一致。

## 非目标

- 不在 chat 侧重新引入任何形式的 execution 实时状态镜像。
- 不改变卡片答复闭环、@ fanout 的授权与写回通道。
- 不改 PM 派单接口 `POST /api/intents/task-dispatch` 的语义。
- Rev. 2 不在历史中回填任何“当时正在执行”的补偿行；未回答的人类消息本身已足够表达这件事。
- Rev. 3 不恢复任何**进度**行（派发 / 开轮 / 降级回执仍不落盘、仍隐藏）；`turn_routed` 只回答"这条消息去了哪个任务"，不是执行状态镜像，也不随执行进度更新。
- Rev. 4 不做附件的跨系统搬运：chat 行只带显示名，不带可在 chat 里打开的引用（既为 ADR-009 §5 红线，也因为打不开的引用比没有更糟）。
- Rev. 4 不改 chat 侧 composer：chat 输入框仍只发 chat 消息、仍不带 `for_execution_session`，路由照旧。
- Rev. 4 不做 chat ↔ 执行会话的历史双向同步：只有"这一轮的头"从执行面写进 chat 面，历史回看不搬运、不镜像。

## 与现有决策的关系

| 来源 | 关系 |
|---|---|
| 设计 27 §5.5.5 / 决策 17（v2.1） | 本 ADR **撤销** v2.1 的执行状态卡片与"状态读时投影"叙述，改为纯事件行 + 跳转 |
| ADR-099 D3 | 投递时锚定的早期 pointer 由 `turn_started` **改名** `turn_dispatched`；pending 队列只读镜像保留 |
| ADR-086 | 移动端事件行折叠为单行文本 + 跳转，交互项仍走底部 sheet |
| ADR-049 | execution 状态子态仍在 execution 视图展示，不再投影进 chat |
| 本 ADR Rev. 2（D10） | 取代 D2 / D3 / D6 / D7 / D8 的落盘与呈现：中间状态不写、历史隐藏、运行时派生等待行、终态居中 |
| 本 ADR Rev. 3（D11） | 在 Rev. 2 之上恢复**一条** durable 反应行 `turn_routed`（消息去向 = 对话事实，非进度），并确立头像不变量（居中终态不折叠其后气泡头像）；不撤销 Rev. 2 其余决定 |
| 本 ADR Rev. 4（D12） | 落实 D9 的对称缺口：D9 回写**尾**、D12 补上**头**。不改 D9 回写契约、不新增行类型；pinned 回合显式**不写** D11 的 `turn_routed`（未经路由，无去向可宣布） |
| ADR-046 | 直投必须投 `SessionMessage::ChatMessage` 才能保住 `content_parts` / `active_skill` 两臂（`attached_items` 两条路径都有，见 D12 第 8 条）；chat 行的附件显示名取自该联合类型的 wire 字段 |
| ADR-009 §5 | 附件不做跨系统搬运的依据：`install_path` 是 node-local 路径，Gateway 侧文件读在分机部署时必然 5xx |
| ADR-084 | 写入口身份必须是发起人：这是否决"Runtime 代写 chat 行"、选择换入口的直接原因 |

## 测试

- 后端：共享 builder 单测 `pointer_metas_lock_the_dispatch_and_start_rename` 锁定派发/开轮两行的 `message_id` 前缀与 `notice_type`（这是改名回归网），并断言 `opened_new=true/false` 落成 `turn_mode="new"/"continue"`（D8）；`chat_dispatch_applies_the_seed_on_every_reuse_branch` 断言 reuse / 切换都报 `opened_new=false`、fresh 报 `true`；store 层单测 `dispatch_and_start_rows_coexist_and_each_dedups` 验证 `td:`/`ts:` 各存一行且各自幂等去重；`bodyless_notice_rows_do_not_inflate_unread` 验证空 body 的 `system_notice` 事件行不计入未读。三链路（1:1 / 群聊 @ / PM 派发）均经这两个 builder 落地，契约一致；PM `request` 终态经 `deliver_response` 落群聊 `agent_message`（含 sweeper 超时 `failed` 回执）。
- 后端（D9）：`exec_echo_meta_is_a_standalone_agent_message` 锁定 `exec:` 幂等 id、`sender_session_id`、`status` 且无 `in_reply_to`、不与 `td:` / `ts:` 冲突；Gateway `chat_append_kind_whitelist_includes_continuation_echo` 锁定白名单接受 `agent_message`、拒绝 `user_message` 与未知 kind。`ConversationSession::chat_session_id()` 读取绑定；`SessionMessage::ChatMessage` 回合结束时按绑定回写（best-effort）。
- Desktop：删除 `ExecSessionStatusCard.test.tsx`；更新 `AgentChatView.test.tsx`（断言派发/开始事件行可见、`InlineInteractionCard` 就地可答、乐观标题种子经事件行触发）；更新 `chatSessionStore.test.ts`（移除 live 投影用例，保留 `closedCardIds` / `humanAnsweredCardIds`）；新增 `ChatNoticeEventRow.test.tsx`（四类 notice 文案 + 跳转 + `interaction_resolved` 折叠为无 + 无文案无跳转不渲染 + D7 归属：turn 反应带头像 / 名字、`router_fallback` 无归属、`hideSupersededDispatchIds` 折叠契约、`groupTurnReactions` 配对——紧邻折叠、非紧邻 / 异发送者 / 异触发 / 异执行各自独立）与 `InlineInteractionCard.test.tsx`（可答选项 / 自由回答 / 无选项 / answered 收据 / closed 收据 / readOnly）。`AgentChatView.test.tsx` 额外断言一轮一气泡：紧邻回复吸收其反应作 caption、一轮只渲染一个"查看执行"。`ChatNoticeEventRow.test.tsx` 另加 D8 两态用例：`turn_mode=new`/`continue` 分别渲染 `turnStartedNew`/`turnStartedContinue` 并插值任务名、无标题时回退触发正文、`turn_mode` 存在但无名时回退单一条、`turn_dispatched` 不分裂。
- Mobile：更新 `agent-chat-session.test.ts`（移除 `projectInFlight` 漂移守卫套件）。
- 回归：`cd core && cargo test` + `cargo clippy --all-targets -- -D warnings`；两端 `tsc` + `vitest`。

**Rev. 2（D10）补充：**

- 后端：`pointer_metas_lock_the_dispatch_and_start_rename` / `dispatch_and_start_rows_coexist_and_each_dedups` 随 builder 下线一并移除；`exec_echo_meta_is_a_standalone_agent_message` 扩展断言本地回合被 stop 时落 `status="aborted"` 的空行且幂等。
- Desktop：`chatTurnState.test.ts` 覆盖 `isTerminalReply` / `isEmptyTerminalReply`（含附件不算空）/ `isFailureStatus`（`aborted` 不红）/ `terminalNoticeKey` 全映射 / `pendingTurn` 的发送窗口、终态结束、旧轮回复、非终态回复、开放式卡片、只等最新人类行、无 id 不等待；`ChatNoticeEventRow.test.tsx` 断言三类 retired notice 与 `interaction_resolved` 渲染为空、真实回执仍渲染；`AgentChatView.test.tsx` 断言旧里程碑不渲染 + 派生的等待行出现、终态落地后消失、空终态居中且带 drill、`sending` 窗口显示通用等待行。
- **等待行的归属前置条件（运行期实测补锁）**：`user_user` 私聊打开即在底部永久显示 shimmer——因为当时唯一判据是"最新 `user_message` 无终态回答"，而私聊里根本没有任何 agent 能来回答。补的锁定：`chatTurnState.test.ts` 的 `opensAgentTurn` 三例（`to` 指名 / `meta.mentions` 指名 / 空白 `to` 与空 mentions 不算）与 `pendingTurn` 新例（归档行不开启等待也不顶掉进行中的回合、fan-out 行仍等待、`agentReachable=false` 连 `sending` 也关）；`GroupChatPanel.waitLine.test.tsx` 六例到渲染层：私聊打开不显示、私聊发送中也不显示、群里未 @ 不显示、群 @ 与 `to` 指名行显示、终态落地后消失、未 @ 插话落在进行中回合之后时等待行仍在。
- Mobile：`AgentChatScreen` 同步同一组规则（渲染期隐藏三类 notice、空终态居中、思考 shimmer 等待行），由 `screens-smoke` 与 `tsc` 覆盖。`chat-routing-reaction.test.ts` 同时锁定与 Desktop 同构的等待规则：`to` 为空且无 mentions 的行不开启等待、也不顶掉进行中的回合，`meta.mentions` 的广播行仍等待（drift guard；移动端暂无私聊 / 群聊界面，故不传 `agentReachable`）。

**Rev. 3（D11）补充：**

- 后端：`intent_turn.rs` 的 `turn_routed_meta_threads_the_reaction_to_its_trigger` 锁定 `tr:` 幂等 id、`in_reply_to`、`turn_mode` 二值与 `notice_type` 字面量；`session_manager.rs` 的 `opened_new_tracks_physical_creation_not_dispatch_intent` 锁定 `chat_dispatch` / `route_intent_session` 的 `opened_new` 仅在物理新建为 `true`（reuse / `dispatch(existing)` 均为 `false`），即 `turn_mode` 的唯一诚实来源。
- Desktop：`chatTurnState.test.ts` 覆盖 `isTurnRoutedNotice` 判定与"反应行不终结等待"；`AgentChatView.test.tsx` 断言反应行渲染为带头像气泡（文案 key + 触发正文回退命名）且等待行仍在其后、空终态行之后的回复重新带头像（`showHeader` 不再被居中行折叠）。
- Mobile：`chat-routing-reaction.test.ts` 锁定同一谓词与等待规则（drift guard）；`AgentChatScreen` 的反应行气泡由 `tsc` + smoke 覆盖。

**Rev. 4（D12）补充：**

- Gateway 单测（`intent_api.rs` 的 `card_answer_tests`）：`a_client_row_id_must_be_a_bounded_id_and_nothing_else` 锁定客户端行 id 只接受 1–64 个 `[A-Za-z0-9_-]`（越界即 400，不做静默规范化）；`attachment_labels_carry_a_basename_never_a_path` / `a_workspace_attachment_is_labelled_by_its_display_name` / `an_unnamable_attachment_is_dropped_not_rendered_blank` 三例锁定 `meta.exec_attachments` 只出显示名（上传臂取 `filename`、工作区臂取 `name` 原文、只给路径时退 basename）、无名项不渲染成空白芯片，绝不泄露路径或 documentId。
- Gateway e2e（`chat_sessions_e2e.rs` 的 `a_pinned_send_lands_the_head_row_with_display_only_attachments`）：走真 HTTP + 真存储覆盖 handler 级语义——绑定执行会话的发送返 200 且行落库，`kind=user_message`、`from` 是调用者本身（不是 Runtime）、`meta.message_id` 是客户端 mint 的那个 id、附件只剩显示名且序列化里不出现路径；“只交文件不发消息”的定向发送不得被 400（零能力回退）；`command` 超 128 时带 pin 则 400 且不写任何行，不带 pin 则丢弃该字段而照旧收下整条消息。
- 存储（`acowork-user` 的 `chat_sessions.rs`）：`exec_attachment_labels_are_durable_on_a_user_message` 锁定标签真的落进 JSONL 行并能读回——`IntentMeta` 少声明一个字段，症状就是投影写成功而磁盘上没有那一项；`an_attachment_only_user_message_is_accepted_from_the_execution_surface` 正反两例锁定"只有标签、没有正文"的 `user_message` 可以落库，而正文与任何附件都没有时仍被拒。
- Gateway 集成（`intent_delivery.rs` 的 `intent_wire_fields_roundtrip`）：断言 `for_execution_session` 穿过 MQTT 信封 encode/decode 往返不丢失——它是直投的全部信息，丢了就默默退回路由。
- Runtime（`gateway_loop.rs`）：`a_pinned_turn_never_reaches_the_router_queue` 锁定直投不经 Chat Router（不占 drain、不入队，因此没有路由 LLM 调用）；`a_stale_pin_falls_through_instead_of_losing_the_turn` 锁定目标未加载时清空 hint 落回正常路由（pin 是提示不是闸门）；`a_pinned_turn_with_nothing_to_say_wakes_no_session` 锁定空内容定向发送在查会话之前就判为已完成。`intent_turn.rs` 的 `exec_echo_meta_threads_a_pinned_turn_under_its_chat_row` 锁定 pinned 终态行携 `in_reply_to`，而 `exec_echo_meta_is_a_standalone_agent_message` 锁定非 pinned 仍写不带 `in_reply_to` 的 standalone `exec:` 行。
- Desktop：`chatStore.pinnedSend.test.ts` 六例覆盖分流双分支——有绑定时命中 `/api/intents/chat-message` 并携 `for_execution_session`；无绑定、本地模式（无账号）均回到 `POST /sessions/{sid}/messages`；pinned 分支携带会话写本来会带的技能名与 `attached_items`（wire 形状逐项断言，含 `clientId` 与原路径的 parity）；空可选字段省略而不发空白；发送用已渲染乐观气泡的同一 `message_id`（乐观插入与 reconcile 不动，所以行 id 必须三点一线）。无权限与连不上由 `sendPinnedChatMessage` 将 401/403/404 转成 `SessionAccessError`、其余原样上抛来保证。`ExecAttachmentLabels.test.tsx` 四例锁定标签 pill 的显示契约：逐项各画一枚、既非 button 也非 link（没有可打开的东西）、给出 ADR-046 臂名、项为空时什么都不画；`GroupChatPanel.execLabels.test.tsx` 三例锁定群面（PM 任务会话绑定的就是群 chat）：带正文时标签与正文同现，只交文件时不得出现空轮占位，真正空行仍出现占位。
- Mobile：`__tests__/pinned-send.test.ts` 七例分层覆盖——store 层五例（绑定读自会话行时传 pin、未绑定不传、无账号即使绑定也不传、所选 skill 在 pinned 发送上不丢、乐观气泡沿用 transport 生成的 id）；`api.ts` 层两例（经 `setAuthBridge` 注入，带 pin 命中 chat 入口、不带 pin 命中会话入口），两者返回同一个客户端 mint 的 `message_id`。移动端的标签渲染（`AgentChatScreen` 的 `chat-attachment-label` chip）没有独立单测——该端不发送附件，只是读侧兜现桌面写出的行，且移动端测试基建不做行级渲染断言。
- D12 修复轮（①–⑥）补的锁定：Gateway 单测 `only_the_pinned_echo_closes_a_pending_request` 锁定只有 `agent_message` 且 `meta.in_reply_to` 非空才关闭 mailbox（卡片与决议通知的坐标不算），`a_pinned_group_send_may_name_only_an_agent_participant` 锁定 `pinned_agent` 只认本 chat 的 agent 参与者（human 拒绝、Legacy 无类型行仍按成员资格、陌生 instance 拒绝）；二者均由 `chat_append` 落库后 `mark_answered` 接入，`GET /api/intents/{message_id}` 因此同时报 `delivered` 与 `answered`。mailbox 层的 `stale_unanswered_selects_only_open_delivered_requests` 早已锁定一个被 `mark_answered` 关掉的 `user_message` 不会再进 sweeper 候选集，即不会多出一条 `fallback-` 回执。Gateway e2e：`a_pinned_send_into_a_group_files_the_head_row_and_checks_its_target` 覆盖群绑定的 200 / 403 / 未带 pin 的 400，`a_send_with_nothing_to_deliver_is_a_readable_400` 覆盖内容门与 `attached_items` 的 ADR-046 union 校验都在投递之前判完（请求形状错不能伪装成 503）。Runtime：`a_pin_naming_another_chats_session_does_not_take_that_turn` 锁定跨 chat 的 pin 被丢弃、回合落回发送者自己的 chat。两端 `pinned-send` / `chatStore.pinnedSend` 各断言 body 带上 `pinned_agent`。
- 回归：两端 `tsc --noEmit` + 全量 `vitest`；`cd core && cargo check --all-targets` + `cargo test` + `cargo clippy --all-targets -- -D warnings`。

**Rev. 5（D13）补充：**

- 后端（`intent_turn.rs` 单测，钉住根因而非症状）：`pinned_turn_surface_keeps_its_card_target` 断言换传输后卡片三坐标（chat / 触发行 / 可答人）与 `has_human` 仍在；`request_turn_owes_its_card_to_the_pair_session_it_was_bound_to` 锁定 Intent 传输走同一产出函数；`unbound_or_senderless_turn_writes_no_card` 覆盖 mailbox-only 与无发送人两种"写不出可答卡片"的情形；`a_bound_chat_with_an_answering_human_makes_a_standalone_card` 锁定 Rev. 6：绑定 chat 的本地回合只要知道谁可答就照样写卡片（standalone，无触发行）；`waiting_is_not_aborted` 锁定 `TurnOutcome` 四态字符串。
- 回归：`cargo test -p acowork-runtime --lib`（1783 通过）+ `cargo clippy -p acowork-runtime --all-targets -- -D warnings`。
- 手工验收（真实项目群）：群内 @ 一个 agent 让它提问 → `ask_user_question` 卡片出现在群聊且可就地作答；触发工具审批 → 审批卡片同样落在群里；迭代上限触发的 pause → 群内出现"回复以继续"卡片且终态行状态为 `waiting` 而非 `aborted`；同一问题在执行视图与群聊两处都可答，先答者生效（§5.5.4 一个问题两个回答面）。
