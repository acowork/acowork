# ADR-106：chat 与 execute 的边界重划——引用而非副本

- **编号**：ADR-106
- **状态**：提议（待架构评审）——**已全部落地**：P1（`round_id` 铸造 + 轮次侧表 + 区间闭合）、P2（投影读接口）、P3（`rounds.sqlite` 缓存 + `GET /api/chat-blocks`）、P4（chat 写侧切换为引用行）、P5（寻址阶梯与两级 `@`）、P6（卡片归块与回答直投，块渲染器落在 Desktop 两视图与 Mobile）。本 ADR 列出的各"废止 / 退役"项目已从代码里删除；Desktop 与 Mobile 的转录形状已统一（四类词表 + 引用行 + 块投影）。**修订（2026-11-03·等待行）**：D9 的等待行判据窗口由"本地在途的 POST"延长为"本设备自己那次发送的 claim"（见文末修订章节与设计 27 v3.1.2）。**修订（2026-11-03·轮次门铃）**：新增轮次门铃事件 `round_state`（`SessionMessage` 字段 34），补回 D3 零写删掉的通知面，pending 重读由一次性 `setTimeout` 改为周期兜底（见文末修订章节与设计 27 v3.1.3）。**修订（2026-11-04·呈现分叉与 act 族更名）**：呈现契约的折叠规则按 `opened_new` 分叉（新开会话的触发行保留为独立气泡）；D6 的 act 空间更名 `exec_session_*` 族并新增只读 `exec_session_read_round`（见文末修订章节与设计 29 §11）。**修订（2026-11-04·引用重划）**：`@` 执行会话定死为引用语义——阶梯级别 2 的投递退役（`target_sessions` 从人 `@` 路径退役、由 `meta.refs` 材料坐标取代，寻址卡应答保留该通道），续投改由执行块就地输入框承担，交付物化取 ADR-107 参照 part 通道（见文末修订章节）。**修订（2026-11-04·引用部件）**：D3 派发链路上"router 简报"与用户原话不再拼成一个字符串——`exec_session_create` 的入参改为 `background`，其内容作为引用部件（`context_ref`）随行在 `user` 行之后，`user` 行只留原话；本 ADR 的"引用而非副本"边界因此也适用于 chat→execute 的那一条消息（见 [ADR-107](./ADR-107-message-ref-part.md)）。**修订（2026-11-05·门铃第二触发点）**：`round_state` 的触发点从"回合边界"扩到"活回合内卡片开关"——`ask_question` / `approval` 在回合**内部**打开与收口，此前两端都不发门铃，实测一张活了 54 s 的问题卡在 Chat 侧一次都没被读到（见文末修订章节）。**修订（2026-11-05·索引先于引用）**：新增"引用可见即可答"的次序不变量——轮边界先把该轮 `announce` 进侧表、再写 chat 引用行；`RoundRecord.user` 变为可缺省，已登记但暂无内容的轮投影为 `pending`（按 D9 重读）而非空洞，空洞自此只留给"转录不在本机"与"坐标漂移"两类事实（见文末修订章节）。**修订（2026-11-06·动作行）**：块的就地续投输入行从块底移到块头上方的动作行，与“查看执行 / `@` 引用 / 复制整块”并排成一行纯图标；续投改为 hover / 焦点 / 草稿展开的抽屉，`@` 复用引用材料通道，复制为整块的 Markdown 导出（见文末修订章节）。**修订（2026-11-08·轮次头）**：D9 的等待行在"本设备发送的 claim"窗口内由居中通用行升级为轮次头（预期响应者头像 + 名字 + 状态词，应答行到达即由块自有头像头同锚点接管）；"不猜响应者"的禁令收窄为"不从转录形状反推响应者"（见文末修订章节与设计 33）。**修订（2026-11-10·默认折叠）**：块与块内每一轮改为默认折叠，最新一条已答复的 assistant 消息改画在块下方的气泡里（同一份投影 `answer`，不新增读取），`hasFollowUp` 的自动展开状态机退役（见文末修订章节与设计 27 v3.5 §5.5.5）。
- **日期**：2026-11-02
- **决策者**：ACowork 架构组
- **关联**：ADR-100（本 ADR 废止其 D2/D8/D9/D10/D11/D12，重述 D1/D3 的边界）、ADR-099（收缩其 `dispatch` 权力）、ADR-105（废止其 D4/D5 的镜像判据）、ADR-097（可弃侧表模式的直接先例）、ADR-096（写时派生 / 禁止全量读）、ADR-092（per-session 单写者与有界读取）、ADR-084（chat 存储归属 acowork-user）、ADR-055（跨机可达性是本设计的支柱）、ADR-076（单发起人）、ADR-046（附件判别联合；§2.1 与 ADR-107 共同判废了"拼接进投递文本"的物化路径）、ADR-102（TurnOrigin 策略表仍生效）、ADR-107（参照 part：引用重划的交付物化走它的 `context_refs` 通道）

## 摘要

现状把 execute session 的消息**拷贝**进 chat session（终态 echo、路由反应行、交互卡片、失败回执），于是 chat 变成了两个会话的共同转录本：六个写入站点、八套幂等键前缀约定、best-effort 写失败即"永久说谎"、前端用"缺行即进行中"反推状态。ADR-100 从 Rev.2 打到 Rev.8 全在给这套镜像打补丁。

本 ADR 取消镜像本身，做三件事：

1. **引入 `round`（轮次）作为跨两个模型的唯一主键**——由 Runtime 在回合边界确定性铸造、durable、写进执行会话该轮每一条条目的 metadata。代码里今天不存在。
2. **chat 不再存 execute 的消息副本，只存一条 `execution_round` 引用行**（无正文）。块内容由执行会话**读时投影**，投影结果落在 **acowork-user 的可弃侧表**里（ADR-097 同型），首次渲染拉取后落盘，之后本地命中。
3. **chat→execute 的投递改为显式寻址阶梯**：`@` 承载"agent 寻址 + session 选址"两级语义；无 `@` 默认闲聊；LLM 只能**提议**目标，不能静默投递。

净效果：跨进程 durable 写从 6 类降到 1 类，exec→chat 方向**零写入**；谎言的可能性从"网络调用失败"降为"本地索引可重建"。

## 背景（全部经代码核实）

### 1. 镜像是回写复杂度的唯一来源

一个回合对 chat 的义务今天散在六个写入点：`post_intent_response`（Router 自答、chat 路由回合终态）、`echo_terminal_row`（[session_task.rs](../../../core/acowork-runtime/src/agent/session/session_task.rs) 三站点：附件解析失败 / 回合结束含 `aborted` / loop `Err`）、`write_interaction_card` + `close_chat_card`（[loop_interaction.rs](../../../core/acowork-runtime/src/agent/loop_interaction.rs)）、`turn_routed` 反应行（[chat_router/runner.rs](../../../core/acowork-runtime/src/agent/chat_router/runner.rs) 与 [gateway_loop.rs](../../../core/acowork-runtime/src/startup/gateway_loop.rs) 两站点）、`router_fallback`、Gateway 侧的 `delivery_failed` 与 §6.5 R4 回执与 sweeper 的 `fallback-` 超时行。幂等键是字符串前缀默契（`exec:` / `tr:` / `rf:` / `card:` / `ir:` / `resp-` / `dlv-failed:` / `fallback-`），没有类型约束。

后果不是"复杂"，是**会说谎**：这些写全部是 best-effort（失败只 `warn`），而 [chatTurnState.ts](../../../apps/acowork-desktop/src/components/chat/chatTurnState.ts) 的等待行判据恰恰是"最新人类行尚无终态回答"。终态行写失败 = 该轮在 chat 里永远进行中。

### 2. 镜像的存在理由已经被 ADR-100 Rev.8 和自己的路由改动取消

背景里"为了让 chat 上下文更完整"是拷贝的唯一动机——Router 需要在 chat 侧看见执行情况才能路由。但一旦 Router 不再拥有"猜旧会话并静默投递"的权力（D6），它就不需要 exec 的内容：闲聊只看 chat 自有行，派单只看候选元数据（[chat_candidates.rs](../../../core/acowork-runtime/src/agent/session/chat_candidates.rs) 已经只读 meta，符合 ADR-096）。**镜像的需求方消失了。**

### 3. Rev.4–7 的 pin 复盘：投影能免费偿还 Rev.8 的代价

Rev.4 把执行视图的输入改走 chat 入口，就为了让"人类在执行面敲的那句话"出现在 chat 转录里；Rev.6/7 连续给 pin 打补丁；Rev.8 最终取消该需求，并明确记下代价：「chat 转录里看不见执行面那句提问，那一轮在 chat 里只剩交互卡片与终态 echo」。

引用+投影模型下这笔代价自动归还：块渲染的是执行会话自己的转录，**两侧消息都来自唯一真相**，不需要代写、不需要身份破例、不需要 pin。

### 4. D1 的真实边界比它的措辞窄

ADR-100 D1 删的是 `ExecSessionStatusCard`——在 chat 转录里**读时投影执行会话的状态机**（busy/idle/进行到哪一步）并把它当成 durable 事实。易变态与不可变日志必然对不齐，这是 D1 的正确结论。

本 ADR 投影的是执行会话的 **durable 转录切片**（某一轮的用户消息 + 回复）。不存在两份真相，因为只有一份拷贝，而它是缓存不是事实源。因此 D1 需要被精确重述为：**禁止第二块*持久*真相；禁止投影执行会话的*状态机*作为事实**。块的"进行中"视觉态由 D9 的正向证据规则给出，不来自状态机轮询。

### 5. 群与单聊是同一个困境的两个投影

群：`@agent` 明确寻址，未 `@` 需要 ADR-105 的 `default_responder` 兜底。单聊：只有一个 agent，但有 N 个执行会话，未指明的输入要靠 LLM 猜会话。两者缺的是同一件东西——**一个有明确兜底语义的寻址模型**。今天群用 `mentions` + `auto_routed_to`、单聊用 Router 的 `dispatch(existing)`，两套机制做同一件事。

### 6. 地基的两项事实核查

- **`round` 主键不存在**：全仓 `round_id` 零命中（仅一个无关测试名）；执行会话存储是 [conversation.rs](../../../core/acowork-runtime/src/conversation.rs) 的 `append_message(role, content, metadata)` 扁平流，分页按 ADR-050 的**物理条目 offset**，无轮次概念。→ D2 是所有工作的第一步。
- **跨机读取已经可用**：Runtime 启动时发布 retained `acowork/agents/{id}/http_endpoint`，值为 `{node 反代 base}/agents/{id}`（[agent_init.rs](../../../core/acowork-runtime/src/startup/agent_init.rs)、§6.3.3 自愈重发）；Gateway 的 `proxy_to_runtime` 直接用它拼目标 URL 并附 `X-ACowork-Node-Token`（[proxy.rs](../../../core/acowork-gateway/src/http/proxy.rs)）。`GET /sessions/{sid}/messages` 走 UseCase 层的文件读取，**不要求会话 Active**——ADR-038 的 `Closed`（在盘上、不在内存）转录照样可读，这与 Rev.7 的结论一致。所以只要**Runtime 进程活着**，任意节点的任意（含未加载）会话转录都可读。Runtime 进程死了 → `agent_not_running` / `503` → 这正是 D5 缓存必须存在的理由。

## 决策

### D1 — chat 的内容词表收敛为四类，并确立四条不变量

| kind | 生产者 | 承载 |
| --- | --- | --- |
| `user_message` | 人类，Gateway 以 token 身份写 | chat 的**唯一**输入 |
| `agent_message` | chat session 自己（闲聊 / 分诊的回答） | 输出①：对话 |
| `execution_round` | Runtime 在把一轮工作交付给某执行会话时写 | 输出②：**引用**，无正文 |
| `system_notice` | 通道事实（`delivery_failed` / `budget_exhausted` / `truncated` / `unavailable`） | 与 exec 无关 |

`interaction_request` / `interaction_resolved` **退出 chat 词表**（D8）。legacy `text`、`intent_request` / `intent_response` 的归一化仍保留。

- **I1（单一容器）** chat 只存自己产生的话语 + 指向工作的引用。
- **I2（两种身份）** 对 exec 的任何了解，在 chat 侧只能是 durable 的**引用事实**（不承载内容）或 volatile 的**投影缓存**（可弃、可重建）。二者都不是第二块真相。
- **I3（寻址优先）** 投递决定权：显式寻址 > 提议-确认 > 默认闲聊。LLM 无静默投递权。
- **I4（一个主键）** 一轮 = 一个 `round_id`，跨两个模型的唯一连接键，由 Runtime 在回合边界确定性铸造，不由顺序推断。

### D2 — `round_id`：Runtime 在回合边界确定性铸造，durable，禁止推断

- **铸造点**：**执行会话的回合边界**，一处——`AgentLoop::run` 的进入与退出（[loop_.rs](../../../core/acowork-runtime/src/agent/loop_.rs)，所有回合形态——chat 投递、执行面 composer、intent / PM 派单——都经此漏斗）。不在 Gateway 入口铸造。
- **形状**：`round_id = "r:{inbound_message_id}"`，即该轮**入站消息 id 的确定性派生**，不透明串，≤ 128 字节。入站 id 对每个创建回合的入口都天然存在（chat 投递带 chat 消息 id；composer 带前端生成的 `msg-{uuid}`；intent 带协议里的 message_id）。**过长或字符集不可寻址的 id 一律哈希折叠**为 `r:{digest}-{len}`：这个串下一步就进 URL 路径、转录目录里的文件名与 SQL bind，所以下一步要用的字符集与上游可用的字符集必须是**同一个谓词**（`is_addressable`）——否则一个轮会生于可读之外；折叠也必须无界输入确定性，否则重投递会铸出两个轮。
- **折叠用的摘要必须是稳定摘要**（落地为 SHA-256 前 8 字节），不能是实现细节决定的标准库 hasher：这个串不是内存里的临时键，它写进该轮每一行、写进 D4 侧表，并从 P4 起被引用行当作幂等键 `round:{round_id}`——即一个必须永久稳定的身份。一次工具链升级改变摘要，同一条入站消息就会铸出第二轮（重复块、重复执行），而没有任何手段能发现这件事。
- **为什么是确定性派生而不是入口铸造 UUID**（本条为落地时对原决策的修正）：① **零协议变更**——不需要给 proto `Intent` 加字段、不需要 Gateway 与 Runtime 同批发版；② **保住既有的重投递去重**——MQTT 重投同一 `message_id` 时，现状正是用它去重（`exec:{message_id}`），随机 UUID 会把这条性质丢掉；③ 唯一性由铸造者（消息 id 的生成方）保证，与 UUID 等价。
- **落盘**：该轮产出的**所有**条目（user / assistant / tool / attachment 等）metadata 都带同一 `round_id`。注入点是写入漏斗 `append_message_with_id`（[conversation.rs](../../../core/acowork-runtime/src/conversation.rs)）——全部 JSONL 写入都经它，因此不需要逐个调用点传参，也不会漏。写时同步 upsert D4 的轮次派生索引。
- **答卡续跑不铸造**：上一回合以 `waiting` 结束（`pause` 卡片仍开着）时，本轮**延续**活跃轮——判据是「上一回合 outcome 为 `waiting`，或会话仍有未闭合的 `pause` 卡片」且活跃轮未闭合。进程重启后活跃轮状态丢失 ⇒ 退化成一个新轮，可自愈（D4 的崩溃修复把未闭合记录补成终态），不会留下永久的"进行中"。
- **入站 id 与活跃轮都没有时，本轮不开轮**（落地时对 D2 的补条）：`mint_round_id("")` 会兜底成随机 UUID，而随机 id 意味着一个谁也无法再寻址、chat 里那条引用行也永远指向不出来的轮——写下去就是一个幻影块。宁缺毋滥：这种回合整轮**不带 stamp**（行照样 durable 进转录，只是不进任何轮次索引、不投影成块）。每个传输都提供入站 id，所以这是退化分支，不是兜底分支；`TurnSurface::new` 与 `run` 两处都守住同一个判据，防止有人在边界之外"补一个 id"。
- **禁止由顺序推断**，四个反例：上下文压缩（ADR-010/032/061 改写进入的表示）、强制加塞开新轮（pending 队列插队）、DevMode resume/step 重放**同一**轮（ADR-013；`replay` 不开新轮，且重放的条目仍落在原 `round_id` 的 stamp 之下）、`append_message_with_id` 的幂等重投（同 id ⇒ 同 `round_id`，索引 fold 取末次记录，天然覆盖）。任何一条都能让"相邻消息即同轮"的推断错位。
- **重放共用回合边界**（第 3 个反例的落地点）：`AgentLoop::replay` 与 `run` 用同一对开/闭动作。重放不带 stamp 写行等于把该轮的 `answer` 坐标冻结在暂停前那一行，而那一轮往往已经终态、会被缓存，于是 resume 跑出的真答案在 chat 里永远看不见且无自愈路径。轮 id 优先取写者仍持有的活跃轮（延续轮的 id 不是本次 message_id 的函数），取不到时由 message_id 派生，两者都没有才让这批行不带轮次。
- **`TurnSurface` 随之收缩**为 `{ chat_session_id, round_id, answerable_by, has_human }`——ADR-100 Rev.6 的"运行时从会话绑定补齐 chat 坐标"这段补丁逻辑保留，但 `trigger_message_id` 从 surface 上消失（引用行自己就是锚点，呈现层不需要反向指向触发行）。**落地修正**：这个事实并没有从系统里消失——D3 的行形状一直要求 `in_reply_to`，所以它改由 `TurnReference` 供应（与 `opened_new` / `round_name` 同一位）。消失的是**镜像写入用它做幂等键 / 做线程指向**的用途，不是它本身。

### D3 — `execution_round` 是 chat 唯一的"工作"落盘单位

```json
{ "kind": "execution_round",
  "in_reply_to": "<触发它的 user_message id>",
  "meta": { "round_id": "…", "agent_session_id": "…", "agent_instance_id": "…",
            "opened_new": true, "round_name": "…(可空，派单文本或对端摘要)" } }
```

- **无 `body`**：引用行不承载执行内容。呈现层的任务名来自 D4 投影（+ 会话标题）。
- **幂等键就是自然键**：`meta.message_id = "round:{round_id}"`。`round_id` 由铸造者保证唯一，因此重投递天然去重——**前缀键族约定（`exec:` / `tr:` / `card:` / `resp-`）整体退役，只剩这一条规则**。
- **一致性等级显式化**：引用行是**前置型**（写成功才允许投递；失败即返回错误，人类消息退回未投递状态，不产生"投了但看不见"的轮）。通道回执与人类消息行同为前置型。exec→chat 方向不再有第三种，因为不再有写。
- **`turn_routed` 退役**：它的对话事实（"你的消息去了哪里 / 新任务还是继续"）由引用行本身表达（有引用行 = 去了某会话；`opened_new` = 新/续）。ADR-100 Rev.3 恢复这一行的动机（"有来有回"）由 D9 的块呈现满足，而且比一条空正文通知更诚实。
- **写入者是 Runtime**，走 Gateway 的唯一 chat 写入口；不新增写入口，不触 ADR-009 §5 的 fs 红线（`dev/ci.sh` 的 `run_gateway_fs_redline` 仍绿）。
- **唯一写入点在 `AgentLoop::run` 的轮边界，不在传输层**（落地确认，"六站点收敛为一站"的确切落点）：引用行必须写在这一轮即将使用的 `round_id` 之下，而 D2 的答卡续跑规则让那个 id **不再是本轮入站消息的函数**——只有轮边界知道自己正要开哪一轮，传输层写就必然在续跑分支上指错轮。传输层因此只供事实（`TurnReference{trigger_message_id, opened_new, round_name}`），不写行。Runtime 侧 `append_chat_row` 的调用点全仓只有这一处，这是"写入站点从 6 个变 1 个"的可验证形式。
- **前置失败即拒绝开轮**：引用行写失败、或执行会话连自己的 `session_id` 都没有（引用行的形状要求 `agent_session_id`，一个读者解析不出的引用不如不存在）时，回合在第一个 token 花出去之前就返回 `Err`——既不铸造轮，也不落 user 行。"投了但看不见"在这一层就是不可构造的。

### D4 — 读时投影：Runtime 拥有"一轮"的计算，暴露轮次读接口

- **新接口**：`GET /sessions/{sid}/rounds/{round_id}`（Runtime loopback，经 Gateway 代理）。返回该轮的**对话可见投影**：`{ user: {body, attachments, ts}, answer: {body, status, ts}, card: {kind, request_id, prompt, options} | null, terminal: bool }`。
- **"一轮是什么"由执行会话定义，不由 chat 定义**——这是边界的核心。工具调用、思考、流式细节不进投影（ADR-100 D3 的"执行过程细节不进 chat"在本模型里变成"不进投影"，语义不变但更严格：连落盘都不需要）。
- **必须有轮次派生索引**，否则该读接口要在请求路径扫 JSONL，违反 ADR-096 红线（`run_session_full_read_redline`）。
- **索引落点**：`{conversations_dir}/{session_id}.jsonl.rounds`——会话自己的 append-only 侧表，与 JSONL 主存同级，**ADR-097 的可弃侧表同型**（fold-on-read、首触重建、损坏即重建）。名字是主存名加后缀，不能是 `{session_id}.rounds.jsonl`：转录目录里 `.jsonl` 扩展名**就是会话身份**（会话索引按扩展名枚举会话、按 `{sid}.jsonl` 对齐转录），叫那个名字会被当成一个永远解析不出行的会话长期扫，且永远无法被清理掉。不选 SQLite 表：轮次记录每会话几十到几百行、JSONL 侧表把"可弃"做到零依赖，且写入方本来就是 JSONL 的单写者线程（ADR-092），不引入第二个写者或跨 crate 的 schema 依赖。
- **记录形状**：`{ round_id, user, answer?, card?, terminal, status?, ts }`。`user` / `answer` 是 **ADR-050 的 data_index 坐标**（由写入漏斗在提交该行时顺带记下，O(1)），投影因此只读两行 + 用户行后的一个**有界小窗**（折叠附件条目用），永不全量扫。
- **侧表由单写者维护**：写者见到条目的 `metadata.round_id` 与活跃轮不同即换轮；`terminal=true` 的记录**之后不再改写**——这是块可缓存性（D5）的根据。
- **坐标必须由行自身背书才算数**：D5 的缓存**没有失效路径**，所以一个错位的坐标一旦被当成内容端出去，就会被永久固化成"这个块的内容"。投影因此校验取回的两行：role 对、且行自己的 `metadata.round_id` 等于所询轮次。转录缺失、坐标越界、坐标漂到别的轮的行——三者都是**显式空洞**（单条读 404、批量读 `found: false`），既不是别人的消息，也不是 500（真实的读盘失败才报 `Err`，让调用方把它与永久空洞区分成可重试的 `unreachable`）。
- **有界窗也要背书**：窗是**范围**，而范围恰恰是这份投影唯一不能信任的东西——一个在答出之前就断掉的轮（崩溃、interrupt）没有 `answer` 坐标来给窗收口，于是窗会一直扫到下一轮。窗内的行因此同样要求行自身的 stamp 等于所询轮次，否则不属于这个块。写者路径上附件行与 user 行经同一漏斗（`append_message_with_id`），本来就带 stamp，所以这条校验不损失任何真实附件。
- **读路径也要首触重建**："首触"不能只等于写者 open。侧表为空（丢失/未生成）而转录存在时，读路径从转录重建、落盘、再投影——否则一个侧表被清理掉的会话会永远答成空洞，而它的全部事实还在转录里。已有记录时**不**重建（重建会绕过 terminal 快照里缓存层认可的 `card`）。
- **崩溃修复**：会话加载时若发现仍有 `terminal=false` 的记录，补一条 `terminal=true, status="interrupted"` 的终态。因此"进行中"不会因进程被杀而永久驻留——这正是 ADR-100 D10 的反证据推理所缺的性质。
- **进行中轮**：`terminal=false`。投影只读盘（侧表最近一次快照 + 已 flush 的转录行），**不读内存态** —— 因此活跃轮同样在会话未驻留、进程重启后仍可读，代价是它只到「最后一次落盘的 flush」为止。客户端**不得**缓存这种投影，且这一点由缓存层自身强制拒写（D5），不依赖编排方记得过滤。

### D5 — 投影缓存归 acowork-user，编排归 Gateway，按 ADR-097 模式建为可弃侧表

用户明确要求"首次渲染拉取落盘"，因为它决定性能与离线可读性。归属选择是这条决策的核心：

- **存储落点**：`{data_dir}/chat-sessions/{sid}/rounds.sqlite`，per-chat 一个，与 `search.sqlite`（ADR-097）平级同型。键 `(agent_instance_id, agent_session_id, round_id)`，值 = D4 的投影 + `fetched_at` + `terminal`。**终态轮不可变 ⇒ 无失效问题**：这是整个缓存设计成立的性质，也是为什么不需要 TTL、不需要版本号、不需要变更通知。
- **为什么在 acowork-user 而不在 Gateway / 不在前端**：① 生命周期——删 chat 必须连带删掉这份 exec 内容拷贝（隐私），目录级清理只有在 chat 存储同侧才免费（ADR-084 chat 存储归 acowork-user）；② 前端持缓存违反"Desktop 无状态持久化"约定；③ Gateway 不拥有任何转录数据，放它那儿会诱导 fs 越界。**执行内容的这份拷贝是 chat 的派生物，与 `lines.index` / `search.sqlite` 同一族**。
- **谁去拉**：Gateway。它是唯一能触达所有 Runtime 的进程（`proxy_to_runtime` + node 反代 + node token）。acowork-user 不能反向调用 Runtime——它的 `/internal/*` 面只接受 Gateway 的写入。
- **读路径**：`GET /api/chat-blocks/{chat_id}?refs=…`（**新顶层前缀**，不能挂在 `/api/chat-sessions/*` 下——该前缀被 [user_proxy.rs](../../../core/acowork-gateway/src/http/user_proxy.rs) 通配转发给 acowork-user，路由所有权已占）。流程：按引用批量查侧表（`GET /internal/chat-sessions/{id}/rounds`）→ 命中且 `terminal` 直接吐 → 未命中的引用**按 `(agent_instance_id, agent_session_id)` 分组，每个执行会话一次 `?ids=` 批量请求**（单批 ≤ 64，单次渲染 ≤ 200 个引用，超出 422）→ 只把 `terminal=true` 的投影经 `PUT /internal/chat-sessions/{id}/rounds` 写回 → 按**请求顺序**返回，每个引用是四态之一：`ok` / `pending` / `not_found` / `unreachable`。引用**永不静默丢弃**——丢弃会让前端为它编造一行。`X-Error-Origin` 语义沿用 ADR-055 §6.17。
- **refs 的两道守卫**：refs 由客户端给出，故 ① **形状校验**——三段式 `instance|session|r:round`，段内字符集限 `[A-Za-z0-9_.-]` 且 session/instance 禁 `.` 起始、禁 `:`，因为这些值下一步就进 URL 路径、query 与 SQL bind；② **绑定守卫**——Runtime 在每份投影上盖该执行会话自己的 `chat_session_id`（取自会话 meta，不受请求头影响），Gateway 只呈现、只缓存**绑定等于被读 chat** 的投影，其余记为 `not_found` 并 `warn`。缺第二道，同时属于 A、B 两个 chat 的成员就能把 B 的执行内容灌进 A 的缓存，并让它作为"历史"永久存活。
- **三道谓词必须同一集合，且本层只能更窄**：铸造谓词（`is_addressable`）、缓存键谓词（`rounds_cache::valid_id`）与 Gateway 的段校验必须是同一个字符集，且禁 `..`。因为一个本层放行、下游拒的 token 会在**下游整批**失败（缓存读对任一坏 token 直答 422），而 Gateway 把非 2xx 读成"缓存不可读 ⇒ 全部 miss"——于是一个坏 token 让整页缓存永久失效，每次渲染都跨机全量回源。那是 D9 的"一个坏引用不得传染"从另一个方向被违反。
- **形状校验不传染**：一个过不了 ① 的 token **不让整请求 422**，它在自己的响应槽位里以 `not_found` 作答（原 token 回显）。因为 422 会让一个坏的转录行把它周内的所有块一起变成读不了；而静默丢弃那个 token 又会让前端为它编造一行——D9 的"永不静默丢弃"两个方向都算。422 只留给**真正的请求级错误**：什么都没问、以及超过 `MAX_REFS`。Runtime 侧同理：批量读里单个畸形 `round_id` 答 `found: false`（单条读仍 400，因为那里只有一个答案）。
- **可重试的故障不能伪装成永久空洞**：批量读把"该轮存在但行读不出来"记在该项的 `found: false` **加 `error`** 上，这一项在 Gateway 必须映射为 `unreachable`（带重试入口），而不是 `not_found`。两者的区别就是 D9 的第一条不变量：把一次磁盘抖动渲染成"这一轮从未发生"，前端不会重试，而 D5 的缓存也没有失效路径能把它救回来。
- **可弃且自愈**（ADR-097 D3/D4 逐条照搬）：写缓存失败只 `warn`，绝不影响响应；侧表损坏/缺失/说谎只降级为"重新拉取"，绝不牵连 chat 读写；缓存满（字节或条数上限）LRU 淘汰；**顺序契约**：释放句柄先于 `remove_dir_all`（Windows 上 SQLite 文件不带 `FILE_SHARE_DELETE`，否则"可弃侧表"变成"删不掉 chat"），而且缓存读写必须在 per-chat `session_lock` **内**完成（与 `delete_session` 同一把锁）：`connect()` 会 `create_dir_all(父目录)`，一个落在锁外的 open 会复活正在被删的 chat 并留下活句柄——那时不只是删不掉，还是隐私残留：exec 内容的一份拷贝活过了它来自的 chat。
- **降级形状**：Runtime 不可达（`agent_not_running` / `session_not_ready` / node 离线）且缓存缺失 → 块渲染为"工作记录暂不可达（节点离线 / 会话已删除）"，带最后一次已知状态与重试入口。这是一个**明确的、可读的**空洞，不是谎言。缓存命中则离线仍完整可读——这是"首屏不依赖远程 RTT"的性能兑现。
- **不缓存什么**：进行中轮——而且这一点由**缓存层自身强制**（写入入口见到 `terminal=false` 一律 `warn` 并跳过），不依赖编排方记得过滤；读到 `terminal=false` 或非对象的投影一律当 miss（可弃缓存遇上说谎的行，正解是"重拉"，绝不是"信侧表"）；以及块内正文之外的任何东西（工具/思考不进投影因而不进缓存）。

### D6 — 寻址阶梯替代 Router 的单次猜测；LLM 失去静默投递权

优先级从高到低，第一条命中即止：

| 级别 | 触发 | 行为 | LLM |
| --- | --- | --- | --- |
| 1 | `in_reply_to` 指向某 `execution_round`（用户回复那个块） | 确定性续投该会话的下一轮 | 无 |
| 2 | `@agent` + 指名 session（两级 @ 的第二级） | 投给指名会话 | 无 |
| 3 | `@agent`，该 agent 在本 chat 只有 1 个可投会话 | 直接投；多个 → 回一张 session 选择卡 | 无 |
| 4 | 群无 `@` → `default_responder`（ADR-105 D1–D3 保留） | 进入该 agent 的 chat 会话，走 5 | — |
| 5 | 无 `@`（单聊 / 兜底后） | **默认闲聊**；仅当明显是新任务 → `exec_session_create(brief)`；仅当像接续某旧会话 → `exec_session_propose_continue` + **确认卡** | 只能提议 |
| 6 | PM `task-dispatch` | 机器形式的显式寻址（`conv:task` 键） | 无 |

（**2026-11-04 修订**：上表的级别 2 不再投递——人打字的 `@` 会话改为引用材料，消息照常落入本级阶梯（带 refs 时连级别 3 的续投分支也跳过）；续投改由块内就地输入框命中级别 1；级别 3 选择卡的应答仍经 `target_sessions` 投递——那是人选出的地址，不是打字寻址。见文末修订章节。）

- **级别 1 是最高频连续性场景的正解**：「agent 刚汇报完进展，我回一句」不需要 LLM 猜——回复那个块就是寻址。这让级别 5 的"猜"用例大幅减少，确认的点击成本不摊到每条消息上。
- **级别 5 的确认必须有廉价出口**：只有存在**可投候选**（D7 的可见集合非空）时才提议；候选为空则静默走 `exec_session_create` 或闲聊。确认卡上"新建任务"永远是并列选项，一次点击即可逃逸。
- **答一张寻址卡不是一句新话语**（P6 落地补条）：`session_choice` / `confirm_continue` 问的是"你刚才那句话要去哪"，所以答案 = **原文本 + 刚选定的地址重投递**，并带上原行的 `message_id`；chat 存储按该 id 幂等，转录因此仍只有一条消息——另起一行等于把同一句话给人看两遍。地址也只写给**提问的那个 agent**（D7 的 `force_new_for` 同理）。卡片答完即退场：已答的判据是转录里存在指向该消息的引用行（D3），命中即不再渲染（D14 的同一出口——"这里曾经有个决定"由那块自己的轮次与状态说出，不必再并列一行回执）；"一点就没"因此不是消失，而是**交棒**：卡让位给同一个头像与名字下的执行块。
- **卡必须能答，否则不许存在**（**Rev. 2026-11-06**）：寻址卡的 `in_reply_to` 必须指向**本 chat 里真实存在的行**，而这条行只有一条正解——"本次投递应答的是哪条 Chat 行"：群聊 `@` 扇出时是共享的 `user_message` 行 id（信封 `in_reply_to`），单聊时是信封 `message_id`。Runtime 的 `chat_row_id` 是这条推导的唯一实现，三个写入者（引用行、级别 3 选择卡、级别 5 确认卡）共用它；**不得**拿 per-target 投递 uuid 当卡的主语——它不在 chat 里，`GET …/answer` 只能回 `origin_gone`（422），卡片于是永远答不了、也永远不消失。约束在两处落实：写入口（`acowork-user` 的 `append_message`）拒绝主语不在日志里的卡——可答性是卡的**定义**而非可选项；读侧（`chat_view`）把旧版本已落盘的这类卡报成 `answerable: false`，呈现层据此不渲染（与"已答"同一个出口，D14：一个问不出来的问题同样不是对话记录）。日志只追加，旧行不迁移。**卡的持久化本身保留**：刷新页面时问题必须在场，这正是它不能是纯实时信息的原因；要修的是"死问题的持久化"，不是"问题"的持久化。
- **"谁能答"是呈现层的权限，不是数据的属性**（D10 第三层）：卡只递给 `meta.to` 那个人；本地模式（无账号体系，`to` 解不出 id）下只有一个人在说话，因此必须放行。
- **`dispatch(target=existing)` 的静默投递权删除**，Block C 的 `S1..S10` alias 表随之退役（寻址从"LLM 引用别名"变成"人指名"）。Router 的 act 空间：`answer` / `exec_session_create(brief)` / `exec_session_propose_continue(session, why)` / `ask_human(question)`（act 名随 2026-11-04 修订更名，旧名 `new_task` / `propose_existing` 见文末修订章节；只读侧同批新增 `exec_session_read_round`，别名表为该工具重新复活）。
- **`route_intent_legacy` 的 `routing_key` 猜测链**（`conv:task` > `chat:{csid}:{initiator}` > `conv` > `msg`）只保留 `task:` 与"引用行/ @ 指名"两支，`chat:csid:initiator` 的"默认回到上一个会话"一支删除——它就是"高概率出错"的那一类。
- ADR-099 的分层（D1）、无状态 Router + per-chat 队列（D2）、候选隔离（D4）、上下文三段式（D5）、chat/execute prompt 严格分离（D6）、失败降级（D8）**全部保留**；降级不再需要机器拼接 chat 摘要（级别 5 的默认目标是闲聊，不是某个会话）。

### D7 — `@` 的两级语义：agent 寻址 → session 选址

- **第一级（agent）**：`@` 的候选是 chat 的 agent 参与者。单聊只有一个 agent ⇒ 第一级退化为一，UI 不显示这一级，直接到第二级。这是"群与单聊同一逻辑"的落点：**单聊 = 群的特例（agent 数固定为 1）**。
- **第二级（session）**：（**2026-11-04 修订：选出的会话不再接收投递，而是成为引用材料 `meta.refs`；权限集合、候选读、分页即窗口全部不变**，见文末修订章节。）候选集合 = **权限**（[chat_candidates.rs](../../../core/acowork-runtime/src/agent/session/chat_candidates.rs) 原样复用：1:1 只列绑定本 chat + 本人手起；群只列"我发起的"；跨 kind 互斥；agent 发起的会话其 `initiator` 是实例 UUID，永远不等于人的账号 id，所以 agent↔agent 的运行不会出现在人的列表里）。`Errored` 保留（"需要你回去看一眼"是有效信号）；**权限集合之上不再叠展示谓词**——`Closed`（本进程非常驻）仍可寻址、dispatcher 会重开它，因此列表不按常驻与否丢行；唯一的剔除是 PM 任务已终态的会话（复活一条已结束的链不是合法的续投目标）。
- **`@` 是选址不是绑定**：会话归属仍由 D11 的单一真相决定；seed 只在**新建**时生效这一规则不变，且因为指名不再依赖映射表，"seed 静默失效"的分支消失。
- **可寻址集合只能由 Runtime 端出，而且不为此新建端点**（P6 落地补条，后修订）：`GET /api/agents/{id}/sessions?chat_session_id=&surface=`（Gateway 透明代理 → Runtime 那份本就存在的会话列表读）。"这个 chat 里我能寻址哪些执行会话"和"这个 agent 的会话列表"是同一个问题的两种问法，独立端点只会换来两份会漂移的答案——`/chat-candidates` 因此被删除。候选过滤就是权限，而权限长在拥有执行会话的节点上；让 picker 自己算一遍等于让表面端出 router 本会拒掉的会话。两道墙保留：chat 成员关系（不是本 chat 的人拿 404，与"chat 不存在"同形）+ 发起人可见集合（"我"来自 Gateway 注入的 `x-user-id`，不是查询参数——否则任何人都能指名别人的可见集合）。**过滤发生在分页之前** ⇒ `total_count` / `total_pages` 只数合法行，于是**分页本身就是窗口**：随端点一起删掉的是 20 条上限与 7d 新鲜度截断，人要翻多远都行；翻不到某条会话时它仍在转录里，靠回复它的块抵达，不是递不到。
- **两级 @ 都走同一个入口**（`POST /api/intents/mentions`，`targets` + 新增 `target_sessions` 与 `force_new_for`），客户端不再有"有 @ / 无 @"分叉——ADR-105 已消灭第一种分叉，这里消灭第二种。`force_new_for` **是列表而不是旗标**：一次扇出把同一句话投给多个 agent，而人只能拒绝自己正在回答的那一个的续投——旗标会让 `@A @B` 里给 A 选"新任务"顺带替 B 也新建会话。校验显式：地址里的 id 必须在本条 `targets` 内，同一 id 不得同时出现在 `target_sessions` 与 `force_new_for`（矛盾指令不由服务端猜）。

### D8 — 交互卡片归入块，回答直投执行会话

- `ask_user_question` / 权限审批**不再往 chat 写 `interaction_request` 行**。卡片是"那一轮在等什么"的执行面事实，D4 的投影把它作为 `card` 字段吐出来，渲染在块内。
- **回答走执行会话自己的写入口**（`POST /api/agents/{id}/sessions/{sid}/…`，ADR-076 §决策 4 第二波的身份校验），Gateway 校验调用者是 `answerable_by`（发起人）且是该 chat 的参与者。`resolve_card_answer` / `mark_answered` / 回合末安全关卡 / `chatTurnState.closedCards` 全部退役。
- **两类卡不要合写**（P6 落地补条）：执行面的交互卡（`ask_user_question` / 审批）住在块里，答它 = 写执行会话（本条）；寻址卡（`session_choice` / `confirm_continue`）是 chat 里的提问，答它 = 重投递（D6）。前者不留 chat 行，后者不碰执行会话。
- **群内可见性变化已被论证为不劣于现状**（D10）：今天 D9 的 echo 已把该轮输出吐给全群；新模型下参与者仍能看见块与卡片（读），只有发起人能答（写）。
- **`has_human == Some(false)`**（agent↔agent）不再需要"跳过写卡片"的特例——本来就不写 chat。

### D9 — "进行中"是呈现态，判据是正向证据

- 块显示"进行中"的**唯一 durable 判据**：存在 `execution_round` 引用行，且该 `round_id` 的**实时投影**（`source=runtime`）报 `terminal=false`。缓存里不可能出现 `terminal=false`（D5 的强制拒写），因此"进行中"永远不会来自一份陈旧的侧表快照。
- **不可达 ≠ 进行中**：Runtime 问不到且无缓存时该引用返回 `unreachable`（带重试入口），渲染为"暂不可达"而不是"还在跑"——把 outage 显示成进行中，等于让一个停掉的节点改写"发生了什么"。
- 这是**正证据**（有引用行 ⇒ 有一轮在跑），不是 ADR-100 D10 的"缺终态行 ⇒ 还在跑"的反证据。等待行的缺席派生逻辑（`opensAgentTurn` 认 `to` / `auto_routed_to` / `mentions` 三种证据 + `answeredAfter` 认 `delivery_failed` 收口）整体退役。**那条等待行自己该拿什么当正证据**，见文末「修订（2026-11-03）」——同一种证据，只是凭据从"POST 在途"延长为"本设备那次发送的 claim"。
- **流式文本与工具进度**只作为呈现层附加：沿用既有 `session_state` / chunk 的 MQTT 推送（ADR-036/049/035），仅订阅视口内块所属会话，**不入库、不参与上下文构建、不参与未读计数**。D1 的约束在这里生效：不读状态机来"断言"历史事实。
- 空正文终态仍渲染为居中状态行（`aborted` / `failed` / `timeout` / `waiting`）——`terminal.status` 从投影来，不再需要 chat 落一行。ADR-100 Rev.3 的头像不变量保留。

### D10 — 权限三层，chat 参与是块内容的授权基

| 要看什么 | 授权依据 | 落点 |
| --- | --- | --- |
| 引用行的存在 | chat 参与者 | acowork-user（既有 `not_a_participant`） |
| 块内容（投影） | **该 chat 的参与者**（本 ADR 新增的一层） | Gateway `GET /api/chat-blocks/*`：先验参与，再以服务凭证向 Runtime 取，吐给调用方前二次校验 |
| 点块里的按钮 | 该轮的 `answerable_by`（发起人，ADR-076） | 执行会话写入口的既有 owner 校验 |

- 执行会话自己的 `authorize_read` 保持 owner-only 不变——块投影**不是**放宽 exec 的读权限，而是 chat 数据面的一次授权读：Gateway 用 node token（机器凭证，ADR-055 Phase 5a）取回后按 chat 参与关系决定是否吐出。会话绑定 chat 这件事（D11 的单一真相）是这一层授权成立的前提。
- 非发起人的群成员看到的是内容（读）而非操作权（写）。这与今天（echo 行全群可见）等价，因此不是隐私放宽。
- 1:1 chat 的 `user_user`（无 agent）永不产生块。

### D11 — 绑定单一真相：废除 `intent-sessions.json`

- chat 绑定的唯一 durable 真相 = **执行会话 meta 的 `chat_session_id`**（ADR-100 D9 早已用它作"会话永久属于该 chat"的依据）；"某 chat 有哪些会话"改由会话 meta 的派生索引回答，只读元数据（ADR-096）。
- [intent_routing.rs](../../../core/acowork-runtime/src/agent/session/intent_routing.rs) 的 `IntentSessionMap`（`intent-sessions.json`）退役，连带 `chat_dispatch(Target::Existing)` 里"重新指向映射 + stamp"的双写动作与 `TerminalSessions` 的一份镜像归属问题。三处并存（映射文件 / 会话 meta / Gateway mailbox `source_chat_session_id`）收敛为一处 meta + 一处派生索引；mailbox 的 `source_chat_session_id` 是**投递事实**（不是绑定事实），保留。
- **允许一个执行会话被多个 chat 引用**吗：不允许。绑定唯一 ⇒ 块的归属与 @ 权限不复杂化；跨 chat 复用工作靠"查看执行"跳转，不靠引用复制。这是显式的功能边界，不是遗漏。

### D12 — agent↔agent 与 PM 派单归一为同一种引用

- agent↔agent 的 intent 不再往 chat 写 `agent_message` 请求行/响应行（`resp-`），而是**在目标会话绑定的 chat 上落一条 `execution_round` 引用行**——它本来就是一轮工作。`ensure_pair(agent_agent)` 保留（pair chat 仍是转录容器），但它不再承载内容。
- **Rev. 2026-11-06**：PM `task-dispatch` **仍然写一条 `user_message` 行**，引用行随后落在它下面（`in_reply_to` = 该行 `message_id`）——与单聊同构：一条人类输入 + 它那一轮执行会话，**成对持久化**，也**成对进入 chat 会话上下文**（`chat_history_read` 与 Chat Router 的 Block B 把这条行读成 `[user:<created_by>] …`；引用行按 D3 无正文，不进上下文——与单聊里那对的形式完全一致，chat 会话因此能从上下文里读出"谁让谁做了什么"的脉络）。原判词"那条消息在执行会话里，投影自然可见"只对**正文可见性**成立：它让引用行的 `in_reply_to` 指向信箱 id（`pm-…`）而非一条 chat 行，于是块没有触发行、转录里没有这句话（未读、chat 内搜索、列表预览都拿不到它）、块只能靠投影兜底命名——与"卡片指向 per-target 投递 uuid"是同一类悬空坐标缺陷。行的声音是 `Task::created_by`：人创建的任务由人说，PM agent 创建的任务由 PM agent 说；正文只取 `build_dispatch_message`（任务标题 + 描述），唤醒 playbook 仍只走信封的 `assignment_brief`、不上任何 chat 面（D13 Rev.、设计 27 §5.4.5）。task 键路由保留（D6 级别 6）。
- 于是"谁往 chat 写东西"只剩两个生产者：**Gateway（人类 `user_message`）** 与 **Runtime（`execution_round` / `agent_message` / `system_notice`）**，且 exec→chat 方向没有任何生产者。

### D13 — 搜索、预览、未读、标题的重新归属

- **列表预览 / 乐观标题**：引用行的 `round_name`（或首轮的一行摘要，由 Runtime 在写引用行时一并给出，一次性、不改写）足够。**Rev. 2026-11-06**：这一行的来源必须是"人类可读的派单文本"（`acowork-pm` 的 `build_dispatch_message`），**不得**取信封里同名的高信任 `assignment_brief`——后者是唤醒 playbook，`acowork-pm` 明确"never persisted, never displayed on a chat surface"，而 Runtime 曾把同名两个事实当成一个，于是 playbook 落库成行名、并在群聊列表预览与轮次行上屏。禁止为渲染列表去读投影（ADR-096）。**Rev. 2026-11-08**：该禁令的对象是**要为每一行预先取投影**的列表形态（侧边栏会话列表、乐观标题）；而会话内历史对话框是用户主动打开、只为**当前已加载可见行**、按页合并成**一次**投影读的面，不属于禁令对象——它不写 `search.sqlite`，也不把执行正文拷到别处缓存。**Rev. 2026-11-09**：引用行的持久化键由 `assignment_brief` 改名为 `round_name`。理由是这里真有一处歧义：信封的 `assignment_brief` 是高信任唤醒 playbook（`acowork-pm` 明确 "never persisted, never displayed on a chat surface"，设计 27 §5.4.5），而引用行这个同名的键装的是给人类看的一行任务名——一个名字两个事实，正是 2026-11-06 "playbook 上屏"的同源陷阱，故把引用行这一侧改名。**代价与硬规则**：改名在 Runtime / Desktop / Mobile 三处改完就上线了，而键的**登记方**——user 服务的 `IntentMeta`（唯一能声明 chat 行键的地方；未声明的键被 `check_write_contract` 硬拒 422）——漏改；又因为引用行是**前置型**（D3），拒绝它等于拒绝整轮投递：所有 PM 派发在首轮即死、任务永远停在 `pending`、下游依赖任务全部阻塞。所以从此这条键集合按跨进程契约对待：改名或新增必须在**同一变更**里动生产端、`IntentMeta` 声明和两端共用的黄金行 fixture（[`EXECUTION_ROUND_META_SAMPLE`](../../../core/acowork-core/src/intent.rs)）；rename 之前写下的行保留旧键，且只允许在 `IntentMeta::round_display_name()` **一处**回读。
- **全局搜索 / chat 内搜索**（ADR-097 / ADR-081）：`search.sqlite` 索引 chat 自有行 + 引用行的 `round_name`（**Rev. 2026-11-08**：引用行没有正文，`round_name` 就是它唯一的可检索文本，因此它是 `search.sqlite` 的**全文列**而非仅存一列——只存不索引，用户在历史列表里看得见的那行字反而搜不到它自己）；**不索引投影缓存**——那会让一份可弃拷贝进入另一个可弃索引，删除语义变成两跳，且 exec 侧本就有 conversation index（ADR-082 / ADR-095）负责正文检索。跨会话正文搜索走 exec 索引，命中的是"某会话的某轮"，跳转路径已存在（D4 的"查看执行"）。
- **未读**：只算 chat 自有行；块的"等待人类"态作为独立的徽标位（来自投影的 `card != null`），不混入未读计数。

### D14 — 废止与保留清单

| 处置 | 条目 |
| --- | --- |
| **废止** | ADR-100 D2（两类指针行）、D8（`turn_mode` 与两态文案的 durable 来源）、D9（执行会话本地回合的 chat 回写）、D10（停写中间状态 + 前端派生等待行 + 本地 stop 落 `aborted` 行）、D11（`turn_routed` 唯一反应行）、D12（pin / `for_execution_session` 及其全部 Rev.4–7 修订）；ADR-105 D4（`auto_routed_to` 作为等待行证据位）、D5 的 chat 行形态（`delivery_failed` 改为 `system_notice`，保留"失败必须可见"的语义）；ADR-099 D3 中 `dispatch(target=existing)` 的静默投递权 |
| **保留** | ADR-100 D1（重述边界，见背景 §4）、D3（执行细节不进 chat/投影）、D4（"查看执行"跳转）、D5（卡片就地可答——落点改为块内）、D6（发送侧瞬态占位）、D7（turn 指针按 agent 归属化反应呈现——现由块承担）、Rev.3 头像不变量、Rev.8 的"执行面消息不进 chat 通道"不变量；ADR-099 D1/D2/D4/D5/D6/D8；ADR-102 TurnOrigin 策略表；ADR-105 D1/D2/D3/D6/D7（默认接收方本身、写入四道门、生命周期、不引入群级 LLM 路由） |
| **改写** | ADR-097 的搜索范围表述（D13）；ADR-096 的"写时派生"清单里新增轮次索引（D4） |

### D15 — 写契约的三条硬规则（Rev. 2026-11-06）

前两条是把"成对的提问 + 执行"从**约定**变成**类型**；第三条让坐标不再靠推测。它们全部落在唯一写入口（[chat_sessions.rs](../../../core/acowork-user/src/chat_sessions.rs) 的 `append_message` 与 [chat_turn.rs](../../../core/acowork-gateway/src/intent/chat_turn.rs) 的 `open`），而不是读侧兜底——读侧兜底只会让"这条行指着一句不存在的话"在转录里继续显示成事实。

- **坐标必须可解析。** 任何带非空 `in_reply_to` 的行，被指向的行必须**已经在本会话日志里**；空 = **origin 行**（这一轮不是本会话里的某句话问出来的：agent↔agent 请求、cron 唤醒、作者自己开的派单），合法且是事实。同一形状的缺陷实测三次，每次都让人类看见一个洞：卡片指向群 `@` 扇出的 **per-target 投递 uuid**（提问永远答不了，`…/answer` 恒 422 `origin_gone`）、块指向 PM 信箱 id `pm-…`、限额回执指向对端请求 id。生产者要么先写被指向的行，要么声明自己是 origin。
- **派单以作者身份发言。** 一条 `task-dispatch` 先是一句 chat 话，再是一轮执行：行的声音是 `Task::created_by`——人创建的任务由人说（`user_message`），agent 创建的任务由该 agent 说（`agent_message` + `meta.task_id`，即 **assignment**：本会话里没有哪句话要求过这份工作，所以它**不需要** `in_reply_to`，这正是 D12 早期文本想表达的"origin"）。于是派单要求作者是本群参与者；没有声音的作者不能派单（Gateway 返回 `chat_log_unavailable`，PM 把原因记在任务上并按既有退避重试）——反过来说，**允许某人在项目里创建任务，就等于允许他在项目群里说话**，这两件事的判据必须一致。所以判据落在**创建**处：非项目成员对项目只读，`POST /projects/:id/tasks` / MCP 建任务返回 403 `author_not_project_member`（`acowork-pm` 的 `ensure_author_has_voice`，与 `ensure_assignee_is_member` 同一份名册：owner 或成员）；派单侧的 `chat_log_unavailable` 只剩兜底作用——它挡的是绕过创建的路径，不是正常的权限表达。
- **坐标由生产者声明，不由接收方推导。** 投递在信封上显式携带 `chat_row_id`（`mqtt_payload.proto` 的 `Intent.chat_row_id`），它就是 Gateway 刚写下的行 id；Runtime 直接采用，不再从 `message_id` / `in_reply_to` 反推——反推在群扇出与信箱 id 两种情形下都推错过，是本 ADR 三个缺陷的共同机制。

## 呈现契约（块 = 执行会话的 explore 块的对话版）

执行会话里的 explore 块折叠了一次"多工具调用 + 思考"的过程；chat 里的块折叠的是**一个执行会话**的"多轮提问 + 回复 + 等待中的卡片"。语法照抄 explore 块，唯一发明是行的载荷：`user`/`assistant` 取代 `tool_call`/`tool_result`。

**存储按轮，渲染按会话**（D3/D4 的呈现面）：一个执行会话在转录里只出现一次，成一个块；被它折叠的触发行（引用行 `in_reply_to` 指向的人类消息）不再作为独立气泡出现——一句话只出现在一个地方，D6 的重投递是同一规则的既例。**修订（2026-11-04）**：开新会话的那一轮（引用行 `meta.opened_new` 为真）是这条折叠规则的分叉——它的触发行保留为独立气泡，见文末修订章节。

- **块的锚点**：该会话在已加载窗口里的**最新**引用行。长命会话跨天推进时块跟着往下走，而不是停在第一次出现的位置——这是"session 是渲染目标、不是存储单位"的落地形式。同一窗口里两个会话各自成块，块的先后由各自锚点决定。
- **折叠头**：agent 头像 + 名字（块总是起新一轮，所以它戴气泡那套头像/名字）+ 任务名（优先会话标题，空则首轮触发消息正文截断——ADR-100 D8 的任务名解析逻辑原样搬到呈现层）+ `N 轮 · 状态词` + 任务号（D13：标签，不是链接）。
- **状态词**：优先执行会话的 `session_state`（等模型响应 / 正在输出 / 执行工具中 / 等你审批 / 已暂停…），取不到时回落最新一轮投影的终态文案。v1 不做流式，块头不承担逐 token 更新；"是否还在跑"仍然是 D9 的 `pending` 读，`session_state` 只细化文案。
- **展开态**：默认最近 **2** 轮，其余收成块内的"查看更早 N 轮"；只有最新一轮默认展开自己的问答对，更早的轮收成一行——这就是块不至于把转录拉高一大截的原因。
- **一轮一行**：行首是轮号 + 状态符（进行中=脉冲点 / 完成=勾 / 失败=红叉 / 中断=黄叉），行内容是提问摘要；点开是问题面板 + 答案面板，照抄 explore 块的"参数面板 / 结果面板"两层语法。读不到就说读不到（unreachable 带重试、not_found 显式空洞），不画空框（D5）。
- **跳转**："查看执行"在**块上方**独立成钮——块打开的是执行会话这个对象，不是块里某一行；每轮不再重复挂跳转。
- **卡片**：某轮停在的待决卡片在**块级**呈现，块折叠时也可答（D8）；群聊里非发起者只看到问题与"只有发起本轮的人可以回复"（D10 的读写分界）。
- **@ 寻址**：单聊与群聊的 composer 都有 `@`。群聊一级选 agent / 成员，单聊一级退化（只有一个 agent），二级照旧选任务——地址是人给的指令，不能靠模型替人决定（D7）。
- Mobile：同一套语法与同一套类名结构，delta 只有呈现密度——最新一轮答案内联、卡片收成一行且按钮进底部 sheet（§6.5）；`session_state` 只覆盖前台被监视的会话，所以块头只在它就是本块会话时才敢写活线。

## 被拒绝的备选

| 备选 | 拒绝理由 |
| --- | --- |
| 继续做卫生工程（类型化 `ChatWrite`、统一幂等键、best-effort 写进 outbox 重试） | 治的是症状：写入站点从 6 个变 1 个才是根因。outbox 重试仍然要处理"跨进程写一个不存在于自己存储里的东西"这一整类失败模式 |
| 每个 exec session 在 chat 里只有**一行**引用（session 级而非轮级） | 时间线说谎：会话跨天推进后，块停在第一次出现的位置。"session 作为整体"是**渲染**目标（按 session 分组），不是**存储**单位（必须 per-round） |
| 引用行里冗余一份最终答案摘要（"轻量镜像"） | 立刻回到镜像：摘要需要更新，更新就是跨进程写，写失败就是谎。列表预览改用一次性 `round_name` 满足，摘要不进 transcript |
| 缓存放 Gateway 自己的数据目录 | 生命周期与 chat 删除脱钩（隐私残留），且诱导 Gateway 持有转录数据。ADR-084 已把 chat 数据面归 acowork-user |
| 缓存放前端（IndexedDB / localStorage） | 违反"Desktop 仅呈现、无状态持久化"约定，且每端各存一份 |
| 不缓存，每次渲染远程读 | 首屏 N 个块 = N 次跨机 RTT；Runtime 重启后所有块变成空洞；离线/弱网不可读。用户明确要求落盘 |
| 轮次索引冗余 answer 全文而 JSONL 为真相 | 不采用：侧表只存 data_index 坐标，正文取 JSONL 主存。冗余正文会多出一份可弃拷贝，而投影只读两行，性能上并无收益；更重要的是它会把 ADR-096 的“写时派生”变成立一份可写的第二内容源 |
| LLM 判断连贯即静默续投旧会话 | Rev.4–7 与"每发一条多一个空会话"的缺陷族都源于此。高错误率的选择必须可撤销 = 确认 |
| 给投影开一个新的实时通道 | 已有 `session_state` / chunk 推送；新增通道只会造出第二块 live 真相（正是 D1 要杀的） |

## 影响

- **数据**：exec 侧新增 `round_id` metadata + `{sid}.jsonl.rounds` 可重建侧表（**随会话一起消亡**：`delete_session` 删掉它，`prune_excess_sessions` 把它转录一起归档，不留孤儿索引）；chat 侧新增 `execution_round` kind + `rounds.sqlite` 可弃侧表；`intent-sessions.json` / `terminal-sessions.json` 退役（**P5 已删除**：唯一真相是 `sessions` 表的 `intent_key` / `intent_closed` 两列（SCHEMA_VERSION 10）加上执行会话 meta 的 `chat_session_id`；两列不进 `SessionMeta` 结构体，因此 `build_meta()` 的全量 upsert 擦不掉路由决策，`upsert_row` 的显式列清单保证两个写入者互不干扰）。**不考虑兼容**：旧行按现状渲染（历史 `agent_message` echo 仍走普通 `agent_message` 话泡；历史 `interaction_request` 行与 `interaction_resolved` 回执留在盘上但呈现层**隐藏**——P6 定下的就是这一支：旧转录不得复活一张可点的卡，因为答案的写入口已不存在），不做迁移。旧会话无 `round_id` ⇒ 无轮次索引 ⇒ 投影 404 ⇒ 块降级为空洞（而非谎言）。
- **协议**：新增 `GET /sessions/{sid}/rounds/{round_id}`（Runtime）、`GET /api/chat-blocks/{chat_id}`（Gateway，**新顶层前缀**）、`GET`/`PUT /internal/chat-sessions/{id}/rounds`（user internal）、`GET /sessions?chat_session_id=&surface=`（Runtime——第二级 `@` 复用同一份会话列表读，只加两个查询参数；`/chat-candidates` 与其 Gateway 代理已删除）；`/api/intents/chat-append` 的 kind 白名单从 `interaction_request|system_notice|agent_message` 改为 `execution_round|system_notice|agent_message`（P4 当时仍含 agent↔agent 回答行；P6 落 D12 后 `agent_message` 只剩 Chat Router 的自答）；`mentions` 新增 `target_sessions`（agent → session 的第二级 `@`）与 **`force_new_for`（按 target 的列表，不是旗标——见 D7）**。三级寻址事实以 delivery `params` 键跨越 Gateway→Runtime（`target_session_id` / `explicit_mention` / `force_new`，两枚旗标只在为真时写入），因此阶梯的前三级零 proto 变更；`force_new` 是 **per-target 投递参数**，请求体的列表只在装配时扇出。Router 的提问卡走 `system_notice`：`notice_type` 为 `session_choice`（级别 3 多候选）与 `confirm_continue`（级别 5 提议），meta 携 `options[]` + `allow_new_task: true`，幂等键 `{notice_type}:{author}:{inbound message_id}`（**Rev. 2026-11-06**：原写作 `{notice_type}:{message_id}`，漏了"提问的是哪个 agent"——群聊 `@A @B` 是同一行投给两个 router，各自问自己的候选，按行去重会让第一张卡把第二个 agent 的问题吞掉，而 `append_message` 对重复 `message_id` 回 `already_written`，写者读作成功，于是那个 `@` 静默得不到会话）——复用白名单已允许的 kind，写入口不新增。**答卡不新增协议形状**：它是一次普通重投递，只是 `message_id` 取原行 id（幂等去重）+ 地址字段。`round_id` 不需要 proto 变更（D2 的确定性派生以入站 `message_id` 为唯一输入）；`Intent.for_execution_session` 的删除仍按 Rev.8 已做的形状；`trigger_message_id` 作为 `TurnSurface` 字段与镜像幂等键在 P4 退役，但同一事实作为引用行的 `in_reply_to` 保留（由 `TurnReference` 供应，见 D2/D3）。
- **删除**：Runtime 的 `exec_echo_meta` / `turn_routed_meta` / `router_fallback_meta` / 三站点 `echo_terminal_row` / `write_interaction_card` / `close_chat_card*` / surface 补齐补丁（**P4 已全部落地**：Runtime 侧 chat 写入口只剩引用行一处）；Gateway 的 `resolve_card_answer` / `mark_answered` / 答案行写入 / sweeper 的 chat 失败行（改为通知）与前端整个 `chatTurnState.ts`（含其 `closedCards` 簿记）、`InlineInteractionCard`、`ReplyQuote`、`ChatNoticeEventRow` 的 `interaction_resolved` / `answered` 卡片闭合链（**P6 已删**——卡片归块与回答直投同批才有可答路径；Desktop 两面（1:1 + 群）与 Mobile 1:1 面的块渲染器、两级 `@` 的第二级选址同批接入）。**明确保留**：`ChatNoticeEventRow`（Desktop）与 `ChatNoticeRow`（Mobile）的渲染期隐藏集合 `RETIRED_NOTICES`——它不是镜像簿记，而是 D14「旧行留盘、渲染期隐藏」的唯一实现点，删掉它等于把历史里的中间状态重新画给今天的读者。**同一规则按行生效的部分**（隐藏集合只能按 `notice_type` 归类，而行级判断要读投影）：寻址卡一旦被 round 回答就属于中间状态，渲染期整行不画——问句是那一轮的中间态，不是对话的记录；"话去了哪个执行会话"这块事实由 round 的块给出，且只说一次（一个事实一个位置）。在此之前它先退化为一排禁用按钮、再退化为一枚"已交给 <会话名>"收据，两种都是同一事实的第二份表示，后者还会按消息条数堆成一列一模一样的行。**另一处按行生效**：留在盘上的通知行若带着行的坐标（`in_reply_to` 指向一条还在窗口里的消息），它就是**那条消息**的回执，渲染在这个人的那一列，并带指向回上文的箭头；只有无坐标、或所指消息已被翻出窗口时，才回到居中的通道事实。居中的失败回执读起来像「关于整段对话」的陈述、和谁都不相连——而 `delivery_failed` 的语义恰恰是「某人这条消息没送出去」。列的归属只由坐标读出来：送达与否仍然是 `row.delivery`，呈现层只决定列，不推导状态。
- **性能**：exec→chat 方向的跨进程 HTTP 写从"每回合 1–3 次（含超时 15 s 预算）"降到 **0**；首屏读一次 `GET /api/chat-blocks`（批量 refs），命中即本地；跨机读只在 miss 时发生。列表/搜索路径仍零投影读（ADR-096 守住）。
- **隐私与保留**：`rounds.sqlite` 是执行会话正文的一份派生拷贝，必须与 chat 主存同等加密/保留策略，并随 chat 删除一并删除（目录级）。这一点必须写进 module-design 的存储清单。
- **回滚**：本 ADR 不提供平滑层（四端 + Runtime 协议同批变更）。回滚 = 恢复镜像写入与 pin 机制。

## 分阶段实施（每阶段可独立验证，不考虑兼容）

| 阶段 | 内容 | 验证判据 |
| --- | --- | --- |
| P1 | `round_id` 地基：回合边界铸造 + 写入漏斗 metadata 注入 + `{sid}.jsonl.rounds` 侧表 + 终态闭合与崩溃修复 | 单测固定四个反例（压缩后 / 加塞后 / 重放同轮 / 幂等重投）轮次边界不错位；未闭合记录在会话加载后变终态；铸造器永不产出一个不可寻址的 `round_id`（过长或字符集不合 ⇒ 在稳定摘要上哈希折叠）；重放写出的行仍落在它所恢复的那一轮之下 |
| P2 | 投影读接口：`GET /sessions/{sid}/rounds/{round_id}` + 轮次侧表 fold / 首触重建（写者与**读路径都是**）+ 有界性（无全量扫） | ADR-096 红线测试；`Closed` 会话可读；跨 node 手工验证（ADR-055）；坐标失效/转录缺失一律显式空洞（404 / `found: false`），绝不端出别人的行——包括附件窗内的行 |
| P3 | 缓存与聚合端点：`rounds.sqlite` + `GET /api/chat-blocks` + `/internal` 写回 + 降级形状 | 删缓存只导致重拉；Runtime 停机时块有空洞而非谎言；缓存命中首屏无跨机 RTT；`remove_dir_all` 在 Windows 通过 |
| P4 | chat 写侧切换：新 kind `execution_round`、删 `chat-append` 的内容类 kind、删六类写入站点 | chat 转录里 `agent_message`（exec 来源）计数归零；fs 红线仍绿。**已落地**：`chat-append` 白名单为 `execution_round|system_notice|agent_message`；Runtime 全仓只剩 `AgentLoop::run` 一处 chat 写（自然键 `round:{round_id}`，空正文，前置型）；执行面引用行由 `TurnReference` 供事实。未盖到：D12 的 agent↔agent `agent_message` 请求/响应行仍在 Gateway 侧（它需要一份 Gateway 并不拥有的绑定读，随 P6），D8 的卡片回答随 P6 |
| P5 | 路由重做：寻址阶梯、两级 `@`、act 空间收缩、`propose_existing` 确认卡 | 静默 `dispatch(existing)` 计数恒为 0；无 @ 的单聊默认闲聊率与确认率可观测。**已落地**：级别 1 = Gateway 校验 `in_reply_to` 指向的 `execution_round` 行（participant-checked 且限本 chat ⇒ 跨 chat 投毒不可能）后写 `target_session_id`，Runtime 在 router 之前读它并 `chat_dispatch(Existing)`；级别 2 = `target_sessions`；级别 3 = `chat_continuation` 恰好一个可投候选才直投，多个 ⇒ `session_choice` 卡（发卡失败 warn 后降级回 router，消息不丢），零 ⇒ 落到 router；级别 5 = act 空间只剩 `new_task(brief)` / `propose_existing(alias, why)` / 不调工具即回答，别名表只服务提议、模型再无任何投递权，提议落 `confirm_continue` 卡且卡上"新建任务"恒并列。显式取舍：降级路径（router 关闭或失败）不再有 `chat:` 映射，多候选与零候选一律新建——**多个候选是一个决定，不是一个事实**；候选行的 `current` 标记同时退役，Block C 纯按 recency 排序 |
| P6 | 卡片归块 + 回答直投 exec；前端 `chatTurnState` 退役；块渲染器（Desktop 两视图 + Mobile） | 等待态只由引用行 + `terminal=false` 决定；旧派生逻辑无引用。**已落地**：Gateway 删 `resolve_card_answer` / `mark_answered` / 答案行写入，sweeper 失败改发通知；卡片在 `track` 时以非终态 `card` 快照进块（D4）；D12 引用行归一（Gateway 不再代写 agent↔agent 的请求/响应行）；`chatTurnState.ts` / `InlineInteractionCard` / `ReplyQuote` / `closedCards` 全删；寻址卡按钮 = 原文本重投递（`target_sessions` / `force_new_for` + 原行 `message_id`）；两级 `@` picker 接两视图，候选走带 chat 作用域的会话列表读 `GET /api/agents/{id}/sessions?chat_session_id=&surface=`（独立的 `chat-candidates` 端点是同一问题的第二种问法，已删除）。Mobile 同批落地：行形状换四类词表 + 新 meta 字段；读层 `lib/chatBlocks.ts` + `stores/chatBlockStore.ts`（按 chat 合批、缓存无失效路径、批失败逐 ref 记 `unreachable`）；`ChatExecutionBlock` 折叠头 + 答案内联 + 卡片行；`ChatAddressSheet` 承担第二级 `@` 选址与寻址卡的重投递按钮；旧派生（`closedCardIds` / `interaction_resolved` / `turn_started` 那一套）全删 |

## 验收（以测试固定，不是约定）

1. **零回写**：一次 chat 路由回合 + 一次执行面本地回合，chat 存储新增的行必须是 `{user_message, execution_round}` 与 `{execution_round}`——断言 exec→chat 无任何写（`chatStore.execSend.test.ts` 的不变量在新形状下延续）。
2. **自然键去重**：同一 `round_id` 投递两次（重放/重连）只产生一条引用行。
3. **谎言不可构造**：杀掉 Gateway→Runtime 的投影读，块必须显示不可达空洞；不存在任何"进行中"状态因写入失败而永久驻留的路径（因为根本没有那条写）。
4. **可弃性**：删除 / 损坏 / 篡改 `rounds.sqlite` 只导致重拉，chat 读写与分页不受牵连。
5. **轮次不可推断**：D2 的四个反例。
6. **寻址优先级**：级别 1（回复块）与级别 2（@ 指名）都不进 LLM；级别 5 的提议必须产出确认卡且不投递。
7. **红线**：`run_gateway_fs_redline` 与 `run_session_full_read_redline` 同时为绿；`/api/chat-blocks` 的读路径不出现 exec JSONL 全量扫。
8. **权限三层**：非参与者读块 404；参与者可读；非发起人点按钮被拒。
9. **跨 chat 投毒不可能**：引用指向别的 chat 的执行会话时，该块既不呈现也不写回（记为 `not_found`）；伪造 chat-scope 请求头无法改写投影上的 `chat_session_id` 绑定——绑定来自磁盘上的会话 meta。
10. **错坐标不可被当成内容，坏引用不传染**：侧表记下坐标后转录发生变化（行被改写/转录不存在）时，投影必须因行自身的 `round_id` stamp 对不上而答成空洞，绝不返回坐在那个位置上的行；一个畸形的 ref / `round_id` 只坑它自己那个槽位，同一请求里其余块照旧服务。无 `answer` 坐标的轮，其附件窗不得把下一轮的附件算进来。
11. **一时的读故障不是历史没发生**：批量项的 `found: false` + `error` 在块端点必须呈现为 `unreachable`（带重试），与永久 `not_found` 可区分。

## 修订（2026-11-03）：等待行的正证据延长为"本设备自己那次发送的 claim"（D9 重述）

D9 落地时把人类行的等待行收缩成"本地在途的那次 POST"（`sending`）。边界是对的，长度是错的：POST 的往返实测约 150 ms，它只证明**投递已落地**，而该轮第一条 durable 行实测落在 0.008 s – 81.3 s 之后。于是"发出去到 agent 开口"这段几秒到几十秒在转录上是一片空白，composer 同时恢复可发——用户读到的是"我这条没发成功"，并且能在上一句尚无落点时压入下一句，而 chat 级队列并不存在。D14 保留清单里的 ADR-100 D6（发送侧瞬态占位）覆盖的正是这一整段窗口，本修订把它交还给 D6，同时守住 D9 的形状。

凭据的生命周期从 POST 往返延长到"这句话在转录里有了落点"，**判据的形状不变**：仍是正证据，仍只谈本设备自己的发送，仍不读转录的排列。

- **唯一状态源**：`inflight[chatSessionId] = { messageId, at }`，只在内存，只由本设备那次发送的**响应**写入（id 是 Gateway 给那条人类行的 row id），从不由行的顺序或种类推断。
- **建 claim 的条件是"这句话确实被接下了"**：1:1 `POST /api/intents/chat-message` 返回 `status === "delivered"`；群 `POST /api/intents/mentions` 只要 `results[]` 里有任一 target `delivered`，claim 记**顶层** `message_id`——一行 N 次投递，欠答案的是那一行，不是某一次投递。`routed: "archived"` 与全失败不建 claim：一次没有接收人的发送不欠转录任何东西，等待它等于替转录伪造一个结局；1:1 的 `failed` 同理（限额收据 `budget_exhausted` 是另一种结局，它自己可见）。
- **收口**：转录里出现第一条 `meta.in_reply_to === claim.messageId` 且 kind ∈ {`execution_round`, `agent_message`, `system_notice`} 的行即释放；呈现层隐藏的 `RETIRED_NOTICES` 不算落点（Desktop 两项、Mobile 五项，各自与该面通知行的隐藏集合对齐）。群的 `delivery_failed` 回执因此正是结算该 claim 的那一行——"失败必须可见"与"失败结束等待"由此是同一条规则，不再是两条。
- **300 s deadline 兜底**（与 sweeper 的超时口径同量级）：结局行永不出现时释放 claim。释放按 messageId 匹配，所以迟到的 settle / expire 不会误清后继发送的 claim。"进行中"因此是一份**有到期日的声明**，不是一条派生自缺行的事实——这是它与 ADR-100 D10 的根本区别，也是本修订不破 D9 的地方。
- **同一个声明驱动等待行与 composer 闸门**（`turnInflight` ⇒ `composerLocked`）：上一句未确认落点前发不出下一句。锁是诚实的呈现，不是排队的伪装——系统里没有队列，所以不允许压第二句；锁定期间发送按钮禁用并给出原因（`chatView.notice.turnInflightTooltip`）。
- `sending` 退回原义：POST 在途。它不再是等待行与锁的信号。

**不变的边界**（写下是为了防止这条修订被读成回到反证据）：等待行**不**从"最新人类行之后有没有终态行"推 liveness，也**不**读 `auto_routed_to` / `mentions`；刷新或重开后没有等待行——本设备没有自己的发送可声明，这是 D9 的第一人称纪律，不是缺陷；claim 只覆盖本 session、本设备的一句话，agent 那一轮跑到哪与它无关，落点一旦出现人就可以立刻发下一句，即使那一轮尚未终态；块的 `pending` 读（D9 第一条）不受牵连，两者说的是两个对象。后端零协议变更。

落点：[chatTurnInflight.ts](../../../apps/acowork-desktop/src/lib/chatTurnInflight.ts) 与 Mobile 同形孪生 [chatTurnInflight.ts](../../../apps/acowork-mobile/src/lib/chatTurnInflight.ts)；claim 写在两端的 store，deadline 定时器也在 store 模块级（不依赖视图挂载，视图只负责结算）；接线为 Desktop 的 `AgentChatView` / `GroupChatPanel` 与 Mobile 的 `AgentChatScreen`。同批改写：本 ADR 的 D9 相关表述、[ADR-105](./ADR-105-group-default-responder.md) D4 的废止注记与 D5 的"回执结束等待"现状说明、设计文档 27 §5.3.1 与 §5.5.5 第 2/3 条。

## 修订（2026-11-03）：轮次门铃事件补回 D3 零写删掉的通知面

缺陷形状：在 chat 里 @ 一个会话发消息，进入执行状态；人一直停在 chat，执行会话早已跑完，块仍显示 running；点"查看执行"确认已结束，切回 chat 时块才变 Done。即"停留在 chat 界面不会实时刷新终态"。

两条根因同源，且都是本 ADR 自己的账：

1. **D3 把 exec→chat 写成零写的同时，删掉了通知面**。镜像时代"写终态行成功"这件事本身就是通知——写的人顺手就知道了结束。零写之后，D5 的缓存又被明确设计成**只缓存终态投影、无失效路径**（这是它的优点：不需要失效协议），于是块唯一的恢复路径只剩"重新读一次"，而本 ADR 从未写下由谁触发这次重读。验收第 3 条只保证了"不会有永久驻留的进行中因写失败"，没保证"不会因无人重读而驻留"。
2. **前端的重读装配是错的**：`useChatBlocks` 的 pending 重读是一次性 `setTimeout`，重装配依赖 `[chatId, key, anyPending, reload]`。零写让这几个依赖永不变——上一句"写终态行"恰好是它们里唯一会变的那个。于是定时器跑完就再也装不回来，只有卸载/重挂载才恢复；这正好解释了"切走再切回才对"。

后端无罪：磁盘侧表实测同一轮的记录 09:02:43.370 非终态 → 09:02:43.378 终态（8 ms 闭合）。修的是通知面，不是投影。

**决策一：新增轮次门铃事件 `round_state`。** proto 形状 `RoundStatePayload`，挂 `SessionMessage` oneof 字段 **34**（当时的最大已用字段是 33）。链路照抄既有会话事件，不开新通道：`AgentLoop::close_turn_round` 在关闭轮次后（round id 必须在关闭**之前**取，关闭会清掉写者的 active round）经每会话 chunk 通道发出 → `relay_chunk_event_mqtt`（逐个 await，故调用序等于 broker 入队序）→ `MqttChunkPublisher::publish_round_state` → topic `acowork/agents/{instance_id}/sessions/{sid}/messages/round_state`，**QoS 1、非 retained**。载荷只有两个坐标 `round_id` / `chat_session_id`，正文与终态词都不上线（D4 不变）。

- 语义是**"值得现在重读"**，不是"这是最终内容"：事件在回合边界触发，而轮次终态记录是会话单写者的又一次 append（ADR-092），实测晚约 8 ms。因此读侧必须允许一次有界重试（500 ms），并把"读到还是 pending"当成正常而非异常。
- 它也覆盖停在卡片上的轮次：`close_turn_round` 不区分两者，同一条门铃既报终态也报停留；块重读后读到 `waiting`，把问题画出来并**继续**显示这一轮在进行中。此前这一支同样零通知——它与终态是同一个洞，不是两个洞。
- **终态词为什么不上线**：没有订阅方能在不重读的前提下用它（每个订阅方都要拿到正文才画得出块，词随重读一起回来），而 proto3 事后加字段是免费的。现在带一个投机字段，只是把一个公共契约连同手写解码器（`proto-wire.ts`）那份必须跟着维护的诚实一起放宽给一个不存在的读者。门铃的职责边界因此是"哪一块、哪个 Chat"。
- **不违反"给投影开一个新的实时通道"那条拒绝**：拒绝的理由是不要造第二块 live 真相。本事件不带内容，改变不了任何投影值，它只是一个触发重读的信号；live 真相仍只有 `session_state` 与投影读。
- **字段号是跨仓契约**：Mobile 不发 protobuf 运行时，线格式是手写的（`proto-wire.ts`），所以 Rust 侧单测钉住字段 34 的 tag 与 proto3 默认值省略规则——`.proto` 里一次静默改号应当炸掉测试，而不是把每个块永久停在"running"。

**决策二：把周期重读降级为丢帧兜底。** pending 重读改为 `setInterval`（这是根因修复：装配与卸载对称），周期从 4 s 放宽到 **15 s**——它不再是主路径，只负责门铃丢帧时收口。门铃命中**已挂载**的块时立即 reload，带一次有界重试；未挂载则无事发生（挂载时本来就要读一遍）。

**订阅落点的选择**：Desktop 的通配订阅 `acowork/agents/+/sessions/+/messages/#` 已覆盖该 topic，零新增订阅。Mobile 的 `realtime.ts` 是设计 25 §8.0 的**单会话精确订阅**不变量，给它加伴生订阅会破坏这个形状；门铃因此落在 `presence.ts`——它本就通配订阅 `agents/+/status` 与 `agents/+/sessions/+/state`，且事件非 retained，订阅时不会全量重放。被否决的第三种写法是由执行会话把事件也发到 chat 的 topic 树下：它依赖 broker 的 permissive ACL，并且破坏了"topic 归属等于会话归属"这条纪律。

**决策三（2026-11-05 补）：门铃的第二个触发点——活回合内的卡片开关。** 缺陷形状：执行会话弹出 `ask_question` 卡片并阻塞等人回答，Chat 里对应的执行块却不弹卡。实测证据（同一轮，磁盘侧表 + Gateway 请求日志）：卡片 22:22:21 写入 `card` + `status=waiting`，22:23:15 被回答后收回，而该 ref 的块读请求只有 22:22:17 与 22:23:37 两次——**卡片活着的整段时间里 Chat 没有读过一次**。原因不是投影缺字段，而是通知面缺触发点：`publish_round_state` 只在 `close_turn_round` 调用，而这两类卡片在回合内部开合，两端的发生与消失都不经过回合边界；15 s 的周期兜底又被"窗口被遮挡时 webview 计时器被钳制"吃掉了（日志里前一段就有一次 28 s 的丢帧可作旁证）。

修法：把 `publish_round_card_state`（`track_interaction` 打开卡片、`forget_interaction` 收回卡片共同经过的唯一一点）改为"写侧表 + 打门铃"。三个细节是刻意的：

- **与写同一顺序**：先 `note_round_state` 再打铃。铃只表示"值得现在重读"，写者侧表 flush 晚几微秒的竞态由读侧已存在的 500 ms 有界重试吸收——与回合边界那条铃依赖的是同一个机制，不新增协议。
- **铃名的是写入命中的那一轮**：`Conversation::note_round_state` 因此返回它写下的 `round_id`（无轮可写时返回 `None`，此时也不打铃）。不能用 `active_round_id()`：它读的是写者侧跟踪，只有在写者处理过本轮第一条行之后才认识这一轮，而"回合刚开、问答先到"恰好早于它。
- **收回也要打铃**：否则 Chat 会留着按钮指向一个已经答过的问题——一个事实的第二份、而且是错的表示。

**D9 的纪律不变**：门铃不建立 liveness，也不作为"进行中"的证据位——它只触发一次读，读回来的 `terminal = false` 才是判据。这条区分必须写明，否则本事件会被读成它刚刚废掉的东西。

落点：[mqtt_payload.proto](../../../core/acowork-core/proto/mqtt_payload.proto)（字段 34）、[loop_.rs](../../../core/acowork-runtime/src/agent/loop_.rs)（`RoundState` / `close_turn_round` / `publish_round_state`）、[loop_interaction.rs](../../../core/acowork-runtime/src/agent/loop_interaction.rs)（`publish_round_card_state`——活回合内卡片开关的第二个触发点）、[subsystems.rs](../../../core/acowork-runtime/src/startup/subsystems.rs)（relay 分支）、[client.rs](../../../core/acowork-runtime/src/mqtt/client.rs)（发布 + 线格式单测）、Desktop 的 [chat_mqtt.rs](../../../apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs) → [chatStore.ts](../../../apps/acowork-desktop/src/stores/chatStore.ts) → [chatBlockStore.ts](../../../apps/acowork-desktop/src/stores/chatBlockStore.ts)，Mobile 的 [proto-wire.ts](../../../apps/acowork-mobile/src/lib/proto-wire.ts) → [presence.ts](../../../apps/acowork-mobile/src/lib/presence.ts) → [chatBlockStore.ts](../../../apps/acowork-mobile/src/stores/chatBlockStore.ts)。一个刻意的设计：`round_state` **不**进 Desktop 的 `CONTENT_EVENT_TYPES`——它的 `session_id` 是执行会话，不得被当成一个对话会话写进消息窗口。同批同步：设计文档 27 的呈现契约 §6.2（门铃与三个约）与 §6.5（Mobile 的门铃不走 `realtime.ts`），以及 MQTT 协议主题树 [protocols/zh/mqtt.md](../../protocols/zh/mqtt.md)。

## 修订（2026-11-04）：呈现契约按 `opened_new` 分叉；act 空间更名 `exec_session_*` 族并新增只读 `exec_session_read_round`

**修订一：折叠规则按 `opened_new` 分叉（呈现契约）**。缺陷形状：新开执行会话的那条用户消息被折叠进块后，独立气泡消失——新会话恰好出现在创造它的那句话旁边却没有可见的成对关系，块读起来像自说自话的孤儿，而不是"一问一答、有前因后果"的交换；续投轮没有这个问题，它的前因就是同一会话上一轮的输出。

因此：引用行 `meta.opened_new === true` 且有触发行时，其 `user_message` **不**被块认领，保留为独立用户气泡，与块构成"问题 → 块"的成对布局；续投轮（`opened_new` 为假或缺省）维持折叠，"一句话只出现在一个地方"的纪律两个分支都不破——opener 轮在块内的行摘要改以 `round_name`（D13：它本就是呈现提示）领头，不再画问题面板重涂触发行原文；无 brief 时回落轮次投影的 `user.body` 截断。块头任务名派生照旧用触发行正文（布局层仍保留 `triggerRow` 引用，只供标题派生）；定位映射里 opener 触发行的 seq 指向它自己的气泡而非块。数据层零改动：`user_message` 行本就由 Gateway 在路由前置盘，分叉只发生在呈现层。实现落点：Desktop 与 Mobile 的 `chatTranscriptGroups` 新增显式标记 `questionOnTranscript`（与 fan-out 的 `showUserLine = false` 区分：两者都是"不在块内重画"，但理由不同），认领集合、块 seq 表与定位映射随之前者；pinned 窗口逻辑（"永不隐藏携带本 chat 问题的轮"）对 opener 不再依赖折叠——原句已在转录独立可见。两端同一规则，防止契约漂移。

**修订二：D6 的 act 空间更名为 `exec_session_*` 族**。`new_task` → `exec_session_create`，`propose_existing` → `exec_session_propose_continue`：名字描述的是操作对象（执行会话），不是模型的主观判断；两个 act 的授权边界一字不变（前者仍是唯一允许的投递且只能新建，后者仍只提议、确认卡、`why` 必填、级别 5 廉价出口照旧）。旧名连同初代 `dispatch` 进 `RETIRED_ACT_NAMES` 漂移守卫，内核 prompt 不得再出现它们；确认卡的 wire 字段 `allow_new_task` 是协议形状不是工具名，不改。

同批新增只读 act `exec_session_read_round(target, rounds)`，补上"那个任务结果如何"此前没有的合法信息源：`target` 取本轮别名表里的候选（权限过滤后的集合，不新增授权面），`rounds` 钳 1–3；读的就是 D4 的轮次投影（侧表 fold + 两行有界读 + 附件窗背书），disk-only 故 Closed 会话照样可读；结果只进本轮 tool_result，**不进 durable**（引用行依旧无正文，Block B 照旧不投影轮次内容）；一切失败都是显式空洞，绝不整轮降级。Safety tail 相应放宽：由"永不声称执行会话做了什么"改为"只可转述 `exec_session_read_round` 本轮读回的内容，不得声称其进行中或将要做什么"。**不提供 `exec_session_send_msg`**：寻址归人，本修订不打开 D6 的静默投递禁令。改名与 prompt/tail 字面量变化会让 provider 前缀缓存 miss 一次，属预期；上下文与缓存的硬约束见设计 29 §11（执行会话信息进上下文只允许 Block C 动态区或 Block B 按 seq 确定性投影两条路，呈现层的视觉重排永不反馈到投影顺序）。

## 修订（2026-11-04）：`@` 执行会话定死为引用语义；续投移交执行块就地输入框

**动机**。起因是一个真实场景：人给同一 agent 派了两个并行执行的会话，两边跑完要把结果汇总、开一个新会话。现有 wire 表达不了它——`target_sessions` 是 `HashMap<agent, session_id>`，每个 agent 只能被指名**一个**会话，N 个引用在级别 2 里没有立足点。最初的方案是"@ 一个 = 转发、@ 多个 = 引用"的数量分叉，被否决：`@` 一个符号承载两种意图是错误的建模——只 @ 一个会话时，人同样可能只是想引用它的结论再开新会话，数量不是意图的判据。终版用**媒介**区分意图，三个正交槽位：这话对谁说（`@agent` / 单聊默认 / 群 `default_responder`，不变）、话说到哪去（**块内就地输入框** → 阶梯级别 1）、这话带什么材料（**`@session`** → `meta.refs`）。`@` 只剩一种语义；级别 2 的投递职责整体移交级别 1——后者本就在架构里（`in_reply_to` 指向 `execution_round` ⇒ 确定性续投），块输入框不是新语义，是把它从"借道主 composer 填充"的两步压成一步的物理化。

**决策一：`@session` = 引用材料（1..N 同一意图）**。落盘的人话行只带坐标 `meta.refs = [{agent_instance_id, session_id, round_id}]`，发送时由 Runtime 在该会话的权限候选集合内解析其**当时最新的终态轮**并钉住——坐标是事实，内容是投影；被引会话之后继续跑，这条引用的快照不腐烂（终态轮不可变），这正是"引用而非副本"的延伸而非背叛。durable 里**不落任何轮次正文**。呈现：引用画在人话气泡上的 chips，点击定位到源块（"备注说明信息来源"由坐标+chip 承担，不需要一段拼接文字）。Router 组装时进程内按坐标读投影，执行臂**整体复用** `exec_session_read_round` 的读（侧表 fold + 两行有界读、2KB/512B 钳、一切失败显式空洞、绝不整轮降级），渲染进 **Block C 动态区**——落在设计 29 §11 缓存硬约束的合法两条路上，不进缓存前缀；每引用钳之外另过一条引用集合总量上限（Block C 膨胀直接吃 Router token 预算，引用数钳 1–5）。被引轮若仍在跑（无终态轮可钉时取当时进行中轮并标注 `terminal=false`，"以下为当时进展"），读不到就是空洞。交付时**物化为 ADR-107 参照 part**（kind=`referenced_round`，经 `params.context_refs` 随消息投递，源坐标写进 markdown 体）——执行面读不到 chat 也读不到其他会话的轮次，材料必须随消息走；这与 `chat_background` brief 同一条通道，`user` 行保持人话原文（ADR-046 §2.1）、自动标题不被材料污染，是一次性投递物化而非驻留副本；寻址续投投递（含卡应答重投递）同样携参。**接收者不变**：refs 只是随行材料，不改变"这话对谁说"——单聊天然发给对面 agent，群里照常落 `default_responder`；实现不得把 refs 当寻址。带 refs 的消息**跳过阶梯的续投分支**（级别 3 的自动转投与选择卡都不触发，与 `force_new_for` 同形）——材料意味着"新工作或闲聊"，不是续投；要续投走块输入框。`target_sessions` 从人 `@` 路径退役、由 `refs` 取代；**寻址卡应答保留该通道**——级别 3 的多候选选择卡与 `confirm_continue` 确认卡的答复本就是"人选出的投递地址"（机器发起问题、人给出答案），与"寻址归人"一致，本修订退役的是人打字指名的投递路径，不是这个决定；`force_new_for` 仍是 per-target 列表不变。校验比照 `validate_mention_addresses` 的"不猜、直接 400"纪律：refs 内每个 `agent_instance_id` 必须在本条 `targets` 内（引用必须挂在寻址上，孤立引用不合法）。内核 prompt 新增 `[referenced rounds]` 段的解读规则（"人附上的材料，用于判断是否 `exec_session_create` 做汇总"），漂移守卫同步钉住；prompt 字节变化 ⇒ provider 前缀缓存 miss 一次，属预期。

**决策二：块内就地输入框 = 级别 1 的物理化**。块输入框发送写一条带 `in_reply_to`（锚定该块 `execution_round` 行的 `message_id`）的 `user_message`，走同一条 POST——阶梯级别 1、幂等、等待行全部现成，零新语义。显隐纪律（Desktop）：光标进入块（hover/focus）浮现，离开消失；铁律是**草稿非空或输入框持焦时绝不收起**（否则打字丢字），只有"空 + 失焦 + 指针离开"三条件齐备才隐藏。Mobile 无光标，替代形状：**块展开时底部常驻一行轻量输入条**，收起时绝不出现——两端同一规则、呈现密度允许 delta（与 D3/D4 呈现契约的 Mobile 段同纪律）。发送侧沿用 2026-11-03 等待行修订：块输入框的发送同样欠一个**本设备 claim**（per-chat 落点），等待行与 composer 闸门按 claim 结算，不与主 composer 的在途状态混用。权限（D10 第三层的同一形状）：**仅该会话本轮的 `answerable_by` 可用**，其余成员看到禁用态加原因——不加这层，群成员就能借输入框替别人续投，等于把 D6 刚收掉的权力从 UI 侧放回来。

**决策三：窗口外会话不再有直达投递——公开取舍**。级别 2 原先兜着"寻址集合大于转录窗口"：D7 那句"翻不到某条会话时它仍在转录里，靠回复它的块抵达"的前提是块总能翻到，而块锚点只存在于已加载窗口，长历史要分页。引用重划后，给一条已翻出窗口的旧会话续投只剩"翻到它的块"一条路。本修订公开采纳两步方案：其一，接受之（长命会话的块靠 recency 锚点通常可达；实在找不到，说明人本来也该先靠转录/`exec_session_read_round` 找回目标）；其二，picker 二级列表每行两个动作**「引用 / 去续投」**——后者不投递，只 locate 到该会话的块锚点（定位机制现成）再聚焦块输入框，把缺口变成两步而不是新开通道。被否决的第三种：保留 `target_sessions` 作投递通道、`refs` 另做新增——等于回到双通道，违背本次重划的初衷。

落点：Gateway 的 `POST /api/intents/mentions` / `chat-message` wire（`target_sessions` 从 @ 路径退役、仅寻址卡应答保留；`refs` 新增 + 400 校验 + 发送时钉轮）、Runtime 的 `rounds?latest` 读端点、prompt 段与漂移守卫、Block C 装配、交付物化（ADR-107 `referenced_round` 参照 part）、两级 `@` picker 的引用模式，Desktop 与 Mobile 的块输入框组件（reveal/focus-trap 纪律、claim、D10 禁用态）与转录 chips 呈现。同批同步：本 ADR 的 D6 阶梯表与 D7 第二级条目注记、设计文档 27 的呈现段、设计文档 29 §11 的引用材料装配规则、[HTTP 协议](../../protocols/zh/http.md) 的 mentions 契约。

## 修订（2026-11-05）：索引先于引用——"引用可见即可答"，空轮读作 pending 而非空洞

**动机**。真实故障：人在执行块底部的就地输入框里发了一条消息，消息确实进了执行会话、agent 也正常回复，但 chat 里的这个块显示"本轮执行已不可用"（`chatView.block.notFound`）。经日志与落盘数据核对，D3/D4 之间存在一个**顺序缺口**：

- 01:48:43.095 网关受理 `chat-message`，Runtime 在轮边界按 D3 写 chat 的 `execution_round` 引用行；
- 01:48:43.113 桌面端对 `refs=…|r:42590fc9…` 发起块投影读（**正是引用行出现触发的**）；
- 01:48:43.151 读返回 `found:false`；
- 01:48:43.170 该轮的侧表第一条记录才落盘（`{"round_id":"r:42590fc9…","user":309,"terminal":false}`），同毫秒 Runtime 日志 `Idle→LlmAwaitingFirstChunk`；
- 01:48:47.160 该轮 `terminal:true, status:"completed"`——回复正常到达；
- 此后该 ref **再未被请求过**（全日志仅一次）。

机制：侧表的记录原先要等该轮**第一条带 `round_id` stamp 的行提交后**才产生（`ActiveRound::snapshot()` 用 `self.user.unwrap_or_default()`，因此 writer 在 `user.is_none()` 时直接拒绝快照），而引用行在这之前就已可见。于是读请求落在约 60ms 的窗口内被判成 `Hole`，前端按 D5 的"终态不重读"把它当成块的永久内容——一个已经完成的轮被判了死刑。这不是偶发：D4 的读者恰恰由引用行触发，所以读必然落在窗口内。

**决策一：轮次索引先于引用行（`announce`）**。轮边界在写引用行**之前**先把该轮登记进侧表（`rounds::announce_round`，经写者线程的同步命令 `WriterCommand::AnnounceRound` 落盘并 flush 后才返回）。于是获得新的不变量：**Chat 能看见的轮，索引一定能答**。登记条目不带坐标（`user` 缺省）、不带 `status`：本轮的运行态是回合的事实，不是索引的事实；登记只陈述"这一轮存在"。已有记录时登记是空操作且绝不改写坐标（重投递或续跑不得擦掉转录已经写下的东西）。

**决策二：`RoundRecord.user` 可缺省，记录过的轮永不判空洞**。D4 的记录形状修订为 `{round_id, user?, answer?, card?, terminal, status?, ts}`——`user` 缺省表示"已登记、尚未写 opener"。`project()` 对此返回 `{user:null, answer:null, terminal, status}`，即 `terminal=false` 的进行中投影（Gateway 侧为 `pending`，呈现层照 D9 重读，块随之填入内容），而不是空洞。空洞自此专属于两类事实：转录不在本机，或坐标已漂移、不再指向本轮的行走（D2 的 stamp 校验不变）。同时收紧反向边界：writer 不再需要坐标即可以写终态快照（`RoundSnapshot` 只校验轮次身份），因此"回合在写 opener 之前就失败"也能落 `terminal:true, status:"failed"`；被后来的轮抢占的无内容轮则按 `interrupted` 收口（`forced_closure` 不再要求坐标），不会永远停在 pending——永远 pending 的块是另一种谎。

**不做的事（边界）**。不给前端加"not_found 重读"：那等于承认服务端可以产出假空洞，把不变量放在错误的层。缓存语义不变（D5：终态不可变、无失效路径），改动只在"终态"这一判断的事实来源上。

落点：`core/acowork-runtime/src/rounds.rs`（`RoundRecord.user: Option<u64>`、`announce_round`、`project` 的无线索分支、`forced_closure`、`fold_attachments` 签名）、`core/acowork-runtime/src/conversation.rs`（`WriterCommand::AnnounceRound` 同步命令、`ConversationSession::announce_round`、`RoundSnapshot` 守卫放宽、writer 侧 `announce_round` 的跟踪器交接）、`core/acowork-runtime/src/agent/loop_.rs`（`open_round` 次序 `announce → 引用行 → stamp`）。验收以测试固定：`an_announced_round_projects_as_open_instead_of_a_hole`、`announcing_a_known_round_keeps_its_coordinate`、`a_round_closed_before_its_opener_is_not_a_hole`、`an_announced_round_reads_as_open_not_as_a_hole`、`an_announced_round_is_answerable_before_its_first_row`、`a_pre_empted_announcement_is_closed_not_left_open`。Desktop 与 Mobile 的块渲染器无需改动（`projection.user` 两侧本就可空，空 user 走既有的"无提问 / 进行中"回退）。

## 修订（2026-11-06）：投递失败不再是 chat 行——内存态实时告警（D12 / D14 顺带修订）

**动机**。真实故障：一条消息投递失败后，chat 里留下一条黄色 `system_notice`（`notice_type: "delivery_failed"`），此后**永远在**——重试成功了它在，agent 装好了它在，刷新、换设备、旧备份全都把它刷回来。而且它还顺带被 `acowork_core::intent::chat_kinds::is_answer_row` 算作"这条消息已被回应"，于是一次早已作废的失败至今仍在替那条消息宣布结局。日志该记的是"谁对谁说了什么"，一条链路的瞬时不通不是对话的内容。

**决策**。`delivery_failed` **不再有写者**（ADR-105 D5 修订）：失败改为挂在**读模型**上的内存态告警——`acowork-user` 的 `ChatAlertStore`（`(chat_session_id, row_id) → {target, error_code, inserted_at}`，TTL 600 s，随进程消失，无任何持久化），Gateway 的三处投递失败分支与 sweeper 的超时兜底改为 `POST /internal/chat-sessions/{sid}/alerts`，读侧把它作为 `ChatRowView.delivery.alert`（`{target, error_code, expires_in}`）装饰在欠投递的那条人类行上，各面在自己的时钟上排到期删除。Dashboard 与 Mobile 由 `DeliveryAlertLine` 渲染，`RETIRED_NOTICES` 纳入 `delivery_failed` 让旧行在渲染期消失。

**对 D12 的影响**。D12 的词表结论不变（chat 只记对话），只是"通道事实"一类里少了一个成员：`system_notice` 现存的活写者只剩预算 / 截断 / 不可达三类限制回执与两类寻址问题。`THREADED_NOTICES` 仍保留 `delivery_failed`——不是因为它还有写者，而是因为**旧转录里的行就是它**，词表若忘掉它，那些行就成了无法解释的数据。

**对 D14 的影响**。D14「旧行留盘、渲染期隐藏」的机制原样适用，隐藏集合多了 `delivery_failed`：失败一旦过去就不该再占据版面，这与"里程碑不是历史"是同一条规则。row 级 `delivery.alert` **不参与**结算的样式推断——它是一份有时限的状态，不是收据行。

**边界**。不给 alert 加持久化、也不加"已读"状态：它的全部价值在于"此刻"。进程重启即归零是正确行为——重启之后那份"刚刚失败"已经不新了。

落点：`core/acowork-user/src/chat_alerts.rs`（新）、`core/acowork-user/src/chat_view.rs`、`core/acowork-user/src/http/chat_sessions_api.rs`（内部路由）、`core/acowork-gateway/src/intent/chat_client.rs`、`core/acowork-gateway/src/http/intent_api.rs`、`core/acowork-gateway/src/intent/sweeper.rs`、`core/acowork-core/src/intent.rs`（`RETIRED_NOTICES`）、Desktop 与 Mobile 的 `DeliveryAlertLine` / `ChatNoticeRow` / `chatTurnInflight`。验收以测试固定：`chat_view::tests::a_live_failure_is_a_decoration_of_the_row_it_concerns`、`chat_sessions_e2e::unaddressed_group_message_reaches_the_default_responder`（断言无收据行、行上带 `delivery.alert`）、`DeliveryAlertLine` 两侧的到期自删、`turnInflight` 的 alert 结算。

## 修订（2026-11-06）：终态轮不携带非 waiting 的卡；块卡回答后强制重读

**动机**。真实故障：执行会话弹出的 `ask_question` 卡 300 s 超时后，chat 侧投影的同一张卡**点了没反应、也不消失**，直到新消息把它覆盖；同时在执行会话里点选是正常的。同一个 bug 还带着一个排版问题：chat 侧的选项是横向排开的一行，选项文字一长就挤成一团。

**根因（经日志与 `rounds.sqlite` 核实）**。三层各自留了一个口子：

1. **写侧**：Runtime 300 s 超时后 `forget_interaction` 把该轮的 `card` 置 `Null` 并写 `terminal=true, status=in_progress`，但**被抢占路径**（`forced_closure`）与**崩溃修复**（`repair_on_open`）保留旧的 `card` 值，于是终态快照里仍带着一张不是 `waiting` 的卡。
2. **读侧**：Gateway 的 `chat_blocks` 缓存 / 直读两条路径把这份投影原样返回，不做任何一致性收敛——缓存一旦写入还永久有效（D5：终态不可变），错值就此固化。
3. **前端**：`BlockCardActions` 的 POST 成功后只做本地 `resolveQuestion`，不触发块重读；而 `noteRoundState` 的 500 ms 有界重试窗口内块可能仍是 `pending`，之后没有新信号再触发重读，卡片就永久停在投影里。

**决策一（写侧不变量）**：**终态的轮不得携带非 `waiting` 的卡**。`forced_closure` 在轮已带卡且 `status != waiting` 时清空 `card`；`repair_on_open` 对 open 轮同样清空。卡片是"这一轮在等人回答"的证据，一轮既然已经收口，它就必然不再等人。

**决策二（读侧收敛）**：Gateway 在 `assemble` 对 `Cached` 与 `Live` 两条路径都过一遍 `served_projection`——`terminal=true` 且 `status != "waiting"` 时强制 `card: null`。缓存的价值是省一次读，不是替服务端保留一个错误值；写侧不变量是第一道，读侧收敛是第二道，二者不是冗余而是"旧数据/外部写入"的兜底。

**决策三（前端不猜）**：回答卡片的 POST 成功后，除了本地清除，还必须 `useChatBlockStore.getState().reload(chatSessionId, [ref])`——一次回答是一个事实变化，呈现层必须去读服务端的事实，而不是相信自己的乐观状态。测试固定：`answering re-reads the round, whatever the POST said`。

**决策四（排版）**：`ask_question` 的选项在 chat 侧的块里纵向排列（`flex flex-col items-stretch`），审批的两个按钮仍横向——选项是"句子"，长度不可控，横向排布必然在长文案上崩掉。

落点：`core/acowork-runtime/src/rounds.rs`（`forced_closure` / `repair_on_open`）、`core/acowork-gateway/src/http/chat_blocks.rs`（`served_projection`）、`apps/acowork-desktop/src/components/chat/ChatExecutionBlock.tsx`。验收以测试固定：`crash_repair_drops_the_card_of_the_open_round`、`a_waiting_round_keeps_its_state_when_pre_empted`、`a_cached_terminal_card_is_not_served`、`answering re-reads the round, whatever the POST said`。

## 修订（2026-11-06）：执行块动作行与续投抽屉（呈现层）

**动机**。v3.2 修订把续投放到了块的**底部**，而"查看执行"仍留在块**上方**的 pill——一个对象两端各挂一个交互带，读起来像两个东西；并且底部输入行即使一次都不用，也一直占着高度把后续转录往下推。对照消息动作行的通行做法后确认：动作应当贴着它作用的对象排成一行，输入只在被使用时展开。

**决策一：四个动作收成块上方一行图标**——查看执行（跳该执行会话）/ `@` 引用 / 复制整块 / 续投。纯图标是为了不让动作行比块头还宽；代价是可发现性，所以每个控件保留 `aria-label` + `title`，且"查看执行"的 tooltip 带该执行会话名（沿用"固定标签重复图标等于没说"的既有规则）。动作行常驻，不做 hover 才出现——只靠指针才能找到的控件等于没有。

**决策二：续投抽屉**。发消息按钮自成胶囊：hover / 聚焦 / 草稿非空 / locate 上升沿任一成立即向右展开输入框，图标留在胶囊左端，视觉上"输入框把按钮包住"。三条机械约束不可省：① 行高固定（`h-6`），只有 `max-width` 过渡——抽屉开合不许推动下方转录；② 胶囊内的输入框宽度固定、由胶囊裁剪——动画期间子元素重排正是 CSS 抽屉抖动的来源；③ 收起态 `inert`，被裁掉的输入框不得留在 Tab 序列里。收起条件是"指针离开 + 无焦点 + 草稿为空"，指针离开后再延迟约 100 ms（跨行扫过不许频闪）。`@` picker 的"去续投"与来源 chip 的 locate 仍走同一上升沿：先展开、下一帧再聚焦（0 宽元素直接聚焦会先横向滚动一帧）。

**决策三：`@` 引用复用引用材料通道**。块上的 `@` 不新造语义：它把 `{agent_instance_id, session_id}` 推进主 composer 已有的地址状态（`ChatRefPick` → 可删 chip），上限仍是服务端的 1–5（第 6 条给提示而非静默丢弃），重复引用幂等。因为级别 1 的回复自带会话坐标，点击 `@` 先释放回复目标——两道指令不能同乘一次发送（发送侧本就拒绝，呈现层不该先让它看起来能发）。

**决策四：复制是 Markdown 导出**。内容 = 会话名 + 每轮的"问 / 答"，**含块内折叠的轮次**（按钮说的是"这个执行会话"，少导几轮会让文档读起来像从第六轮开始）；不可读的轮次写一行"无法读取"，绝不编造答案（D3 / D5）；答案正文原样粘贴，保住它自己的代码块与表格。成功反馈是图标短暂变对勾（`CodeBlock` 的既有做法），失败绝不报成功。

**范围**。Desktop 两个聊天主面（1:1 `AgentChatView`、群聊 `GroupChatPanel`）共用同一个动作行组件；只读会话（`canAnswer=false`）不显示 `@` 与续投，复制仍可用（读动作）。Mobile 无 hover，保持块内底部输入行不变，本修订不涉及。

## 修订（2026-11-06）：块锚定首次出现处；寻址卡落在块尾（呈现层，D4 呈现契约）

**动机**。D4 的呈现契约要求"一个执行会话在转录里只出现一次"，v3.1.1 落成"锚在该会话已加载窗口里的**最新**引用行"。这条锚点与它的初衷（最新答复贴着最新问题）在真实交互下相反：一个会话比打开它的那一轮活得久，人在这个 Chat 里每续投一次就多一条引用行，块于是被拖着跨过中间所有行——几次往来之后，人的消息全堆在上面、回答它们的块掉到最下面，做成对的"一来一回"彻底消失。而它换来的"最新可见"本来就由块内窗口提供（默认展开最近 2 轮；2026-11-10 起块与轮均默认折叠，最新答复改画在块下方，见文末修订）。

**决策一：锚定该会话在本窗口里最早的引用行。**位置属于这次往来，不属于它最新那一轮；续投只增加块内轮次与轮数，块不动。不损失可读性：块内展开最近 2 轮且本身是滚动容器，`keyOfSeq` 仍把每个被折叠的 seq 映射到块（locate 一条新轮次照样落在块上），虚拟列表的 item 身份也从"最新行"变成"首行"，块在会话生命周期内不再重建（旧实现每轮换 key，连高度缓存一起丢）。

**决策二：映射先算，再出行。**旧布局在遍历行的过程中边走边写 `keyOfSeq`，被折叠的行若排在它所属块之后，会把块自身的映射覆盖成该行的 key，locate 于是指向一个不在 DOM 里的行——只因块总在最新行（即最后）才侥幸成立。锚点前移暴露了它。改为遍历前先算好"哪些 seq 由哪一块画"，遍历只负责产出条目。顺序是布局选择，映射不许继承它。

**决策三：`confirm_continue` 落在它所指会话的块尾。**router 的提议卡问的是**这一个**会话（`options` 只有一个目标），把它画在转录最新处，等于让人回答一个看不见主语的问题，而锚点固定后这个距离只会变大。改为挂到该块的尾部（`options` 恰有一个目标、且该块在本窗口唯一时才挂；`session_choice` 多候选没有唯一目标，留在原位），块收起时同样可见（D8：藏在折叠后的可答项不可答），可答性沿用既有判定（谁发起这一轮，`mayAnswerCard`）。

**决策四：更新靠移动视口，不靠移动块。**卡片落位的那一帧，surface 把视口带到该块——仅在块不在视口时、只做一次、居中对齐，复用既有 `scrollToKey` 通道；挂载时已经存在的卡片是历史而非新闻，不触发移动（不与"打开即到底部"打架）。**不做常驻角标、不做闪烁、不发 toast**：滚动本身是反馈，读者被直接带到问题面前并就地回答。原先那种"弹框问要不要续投"的模态路径不再需要。

**决策五：执行面输入不切块。**一个被执行会话的轮次有两种来源，二者在布局里都必须并入同一个块：Chat 起源的轮（引用行带 `in_reply_to`，被折叠进块）和执行面起源的轮（`session_task` 把绑定 chat 回填进 `TurnSurface`，但 `TurnReference` 为空，因此引用行不 thread 到任何 chat 行）。后者在 chat 转录里没有人类消息，切块无从谈起，也印证"块 = 一个会话"仍然成立。

**范围**。Desktop 两主面 + Mobile 1:1 面同批接入锚点与映射修订（Mobile 的寻址卡仍以独立行呈现，块尾挂载与视口带位在其卡片渲染器上另行落地）。不做历史迁移：位置是读时计算的，旧转录同样受益。

## 修订（2026-11-07）：引用行的义务由"授予"决定——agent 转发的 Intent 不授予发起方 Chat（D3 收缩）

**触发**：Intent 转发排查（2026-11-07）。用户 Nicholas 在 1:1 会话里让 SSE 给 PM 发 `create_task`，PM 的 turn 死在 `open_round`：chat 存储以 403 `writer is not a participant of the chat session` 拒绝它写引用行，人类侧只剩一句 `Unexpected error. See details.`，并留下一个 transcript 0 字节、`.rounds` 里 `terminal:false` 永不闭合的执行会话。

**根因不是守卫，是义务挂错了坐标。** D3 的引用行义务挂在**信封带来的 chat 坐标**上（`TurnSurface::from_envelope(env.source_chat_session_id)`），而那个坐标记录的是**这条对话从哪里来**，不是**应答方对哪个 Chat 有问责面**。1:1 Chat 按定义只有一个 agent 参与者，所以"从 1:1 Chat 绑定的执行会话向任何非该 Chat 成员的 agent 发 Intent"是**永远**失败的——不是 corner case，是整条口子。守卫（`chat_append` 参与者检查）自始至终是对的，错的是有人被要求去做一次它做不了的写。

**决策**：

1. **来源与义务分成两个事实**。一次投递携带：来源 Chat（信箱记录、回执地址、sweeper 告警锚点、发起方报告的面）与**授予应答方的 surface**。应答方的信封只携带授予值；未授予即为空，其 turn 自然无 chat 义务（`echo_chat()` / `card_target()` 早已按空 surface 收敛，Runtime 零改动）。
2. **授予只按成员资格算**，由 Gateway 在服务端 roster 上算，发起方无法伪造：应答方 ∈ 该 Chat 参与者 ⇒ 授予；否则不授予。
3. **未授予 ⇒ 该 turn 没有任何 Chat 写入义务，且 `has_human_participant = Some(false)`**——没有 surface 就没有可问的人类。需要澄清时以 `needs_input` 应答回发起方，由发起方去问人类：人类界面是发起方，不是应答方。
4. **无 Chat 是第三态**（cron 触发）：不授予，也**不**断言人类存在与否（`None`），保留人类侧工具。这一态与第 3 态必须可区分，否则"这里没有人类"与"这里没有面"会被混成一句话。
5. **人类发起的投递必须授予，否则拒绝而不是静默降级**。群聊 `@`、单聊回执行块、PM 任务派发这三条路径上，应答的可见性**就**依赖应答方在该 Chat 的轮次块（设计 27 §9-8：task 应答是 relay，不唤醒任何人，人类读的是该群投影的那块）。目标不是成员时，运行是成功的、答案是没人能读的——那正是本平台已经判过一次的黑盒入口。故改为投递前拒绝：`@` 点名非成员 → 400；回块目标非成员 → 400；PM 派发目标不在项目群 → `target_not_in_project_group`（PM 侧可重试失败）。

**不变量**：

- 引用行的写入者永远是**被授予该 surface 的那个 agent**，而它必然是该 Chat 的参与者——`chat_append` 的参与者守卫因此在这条链路上永不触发。守卫保留，作为纵深防御，不作为常规拒绝路径。
- **授予与 `has_human` 永不分置**：二者是同一个 `ChatGrant` 值的两个字段。被要求写不进的面，不能同时被告知"那里有人类可以答你的卡"——一张写不进去的卡会让 turn 永远 park，与写不进去的引用行是同一个缺陷的两个方向。

**实现**：`acowork-gateway`——`intent::delivery::ChatGrant`（surface + answerability 成对）、`http::intent_api::chat_grant`（授予判据，唯一生产者）、`intent_payload`（从 `deliver_inner` 抽出，使"来源 vs 授予"两个事实可在无 broker 下并排断言）；七处投递点显式表态：`deliver_intent`（agent 转发路径，按 roster 判成员）、`deliver_response`（回发起方，成员资格在请求时已验）、sweeper 兜底回执（同上）、`cron`（无 Chat 态）、`deliver_mentions`（先拒绝非成员再授予）、`deliver_chat_message`（同）、`dispatch_task`（同）。**Runtime 与 Desktop 零改动**。

**测试**：gateway lib 6 条——授予三态各一（1:1 转发不授予且断言无人类、成员保留且沿用来源事实、无 Chat 不授予亦不断言），线上传播两条（未授予 ⇒ 信封 chat 为空且 `has_human_participant=false`；授予 ⇒ 携带该 Chat），信箱一条（未授予时来源仍留在信箱条目里，回执轮因此仍能在发起方自己的 Chat 里问人类）。

**遗留（另单，不夹带）**：① 发起方气泡里指向应答方执行会话的**跳转链接**（快捷方式，不是绑定；不得复用块左侧的执行图标，避免与"绑定了执行会话"混淆）；② 错误呈现分流——路由/权限类拒绝不再压成 `Unexpected error. See details.`；③ 非终态 round 的收敛（模型错误死在半路仍会留下 `terminal:false` 残骸，本修订只消除了它的触发主因）；④ agent↔agent pair chat 的人类入口尚未设计，本修订不依赖它。

## 修订（2026-11-08·轮次头）：claim 窗口的等待行升级为预期响应者的头像头（呈现层，D9 修订）

**动机**。执行面与 chat 面对"我发出去了，正在被处理"的反馈不对称：执行 session 发送后**立即**渲染头像 + 名字 + 阶段词（live `session_state` 订阅，ADR-049），chat 面在同一时刻只有一条居中的通用等待行，身份要等轮次引用行与块投影落地才出现。这个滞后是结构性的——chat 的内容面走磁盘投影 + 门铃（D3/D5），而等待窗口被 D9 刻意做成"平台自己的声音"。但"平台的声音"这个理由只覆盖了它的一半：文案确实不该伪装成 agent 的话，**身份**却不必因此缺席——发送那一刻，"这句话会被交给谁"已经是投递决策的输入（单聊对端 / `@` 提及 / 群默认 responder，ADR-105），不是需要从转录形状反推的猜测。

**决策一：等待行在 claim 窗口内升级为轮次头**。判据、窗口与收口机制不变（仍是 D9 的本设备发送 claim：起于 `delivered` 回执，止于第一条应答行或截止；`archived` 与全失败 fan-out 不 arm，故无头；重载无第一人称 claim，故无头）。变的只是形状：转录尾部渲染预期响应者的头像 + 名字 + 一行状态词，复用 `ChatEventText` 的 live shimmer 语法。

**决策二：命名依据是投递决策输入，不是转录反推**。D9 的"不猜响应者"禁令（群聊一条消息 fan-out 到多个 agent，点名其一是猜测）收窄为：**不从转录形状反推响应者**。发送时刻可用的四个事实——单聊对端、被引用轮次的 agent（D6 级别 1，服务端命中轮次时忽略正文里的 `@`）、`@` 提及列表、群默认 responder——是路由的输入本身；改判与投递失败由服务端收口行表达，而收口行即终止 claim，所以一个错误名字的存活窗口被封顶在"发送→收口行"，与现等待行同窗。responder 读取失败或身份解析不出（目录未就绪）时退化为通用居中行，绝不渲染无名头像；被引用的行落在本面已加载窗口之外时同样按"未解析"处理——未解析不等于不存在，拿 `@` 列表去顶替会点名一个根本收不到这条发送的 agent。

**决策三：状态词复用 `getProcessingPhase()`，不新建 chat 侧枚举**（ADR-049 单一真相）。claim 窗口内、轮次坐标尚未到达时，唯一诚实的词是通用的"处理中"——claim 是"窗口存在"的事实，不是阶段的事实。坐标到达且该执行会话本机驻留时，可订阅其 live 状态显示真实阶段词，规则与块头一致（live 词优先、读派生词兜底、仅呈现层附加、不入库不进上下文不进未读，D9 第 4 条原样适用）；跨机不可得时保持通用词，这是常态不是异常。

**决策四：覆盖 = claim 结束，不新造折叠逻辑**。第一条应答行（轮次引用行 / agent 消息 / 回执）到达时 claim 收口，轮次头随窗口快照消失；轮次内容由 `ChatExecutionBlock` 自有的头像头（或 agent 气泡头）在同一头像锚点接管（`avatarAnchor` 机制既有），视觉连续、无跳动。多目标 fan-out 渲染一个堆叠头（头像堆 + 名字列表）：一次发送是一个事实，不拆成 N 个头。

**修订（2026-11-08b·角色行）**：轮次头在名字**下面**补一行 `role`（取自 agent 目录的同一份 meta，与块头 `ChatExecutionBlock` / 常规 agent 消息头同层级同字段），于是它是"头像 + 名字 + role + 状态词"。理由是决策四的接管不变式：块头渲染名字 + role，若 claim 窗口只有名字，接管时就要在名字下**插入**一行、整列下移；先摆好 role，接管只消失状态词，位置不动。多目标 fan-out **不渲染** role 行——拼接出的 role 列表不对应任何单一响应者，头像堆本身已经表达"多人"。

**修订（2026-11-08c·状态词位置）**：轮次头的状态词从身份列（名字 + role 那一列）**移出**，落到块的**内容列**——与块体开头的 40px 空槽同行，也就是 `ChatExecutionBlock` 里"已探索"行所在的那一列。原实现把状态词当成身份列的第三行，两处都违反决策四：`items-center` 的行因该列多一行而变高，名字与 role 被顶离头像轴线；块落地时状态词还要横向跳进内容列，正是"接管只消失状态词，位置不动"所禁止的重排。身份行改用块头逐字相同的类（`mb-2 mt-1 flex items-center gap-2`），于是接管前后头像 / 名字 / role 的几何完全一致，被替换的只有内容列里那一行词。

**不变量保留**：终态收口（`aborted` / `failed` / `timeout` / `waiting`）仍是居中平台行，不套头像——停止不是 agent 的话；`block.notFound` 等空洞类文案同理。三条 D9 不变量（终态居中行、live 仅呈现层、不可达 ≠ 进行中）原样成立。

**范围**。Desktop 两个主面（`AgentChatView`、`GroupChatPanel`）与 Mobile 1:1 面同批切换；新增共享组件与单点响应者解析函数（`resolveExpectedResponders`），claim 元数据（预期响应者实例 id 列表）由 `armTurn` 在发送时携带、随 claim 清除。纯呈现层：无新网络接口、无 proto 字段、无存储写入。设计细节与交付分期见设计 33（`docs/design/zh/33-chat-turn-header.md`）。

## 修订（2026-11-10·默认折叠与块外答复气泡）：块与轮默认收起，最新已答复的一轮画在块下方（呈现层）

**动机**。块默认展开、最新一轮默认打开自己的问答对，是执行面的读法（"我正在看它干活"），不是对话的读法。放进 chat 转录后，一段长跑会先把整屏高度占掉，读者要越过执行日志才看到 agent 说的话——而这个面本来是在聊天。

**决策一：块与块内每一轮默认折叠**。读者点开才看工作细节。呈现契约里的可见窗口不变（展开后仍是最新 2 轮 + 常驻轮），改变的只是"默认打开"这件事本身；`ExploreBlock` 那套"最新一条时自动展开、被后文推走就收起"的状态机随之退役——它存在的理由是让最新的工作在屏幕上，而这件事现在由决策二承担。

**决策二：最新答复画在块下方，形状是转录自己的 agent 气泡**。内容取块内**最新一个已答复轮次**的投影 `answer`（`data-run-answer`）——不新增读取、不改投影契约、不写任何 chat 行；运行中的那一轮还没说话，所以保持上一条答复在屏幕上而不是空一个洞。于是折叠不再牺牲可读性：agent 最后说的话恒在屏幕上，工作细节点一下才展开。

**不变量保留**：块与轮自己打开的唯一情形仍是停在未答卡片上的轮（D8：藏在折叠后的可答项不可答）；`pending` 仍是"进行中"的唯一正证据（D9），其重读由轮次门铃驱动并驱动气泡更新；空洞 / 不可达仍是显式占位。纯呈现层：Desktop 两个主面（`AgentChatView`、`GroupChatPanel`）同批切换，Mobile 本轮不动。设计 27 v3.5 §5.5.5 为准。

## 修订（2026-11-11·寻址卡停靠底部浮层）：`confirm_continue` 不再落在块尾，改为停靠在聊天面底部（呈现层，D4 呈现契约）

**动机**。上一条修订（2026-11-06）把 `confirm_continue` 挂在它所指会话的块尾，理由是"卡片要贴着它的主语"。锚点前移之后这个理由反了：块锚在这次往来的**第一**行，于是一张画在块尾的卡可能落在合成器上方几百行——而转录不会自己滚动。人在底部发完消息，看到的是"什么都没发生"。卡片被画在他看不见的地方，和它离开主语是同一件事的两面。

**决策一：单目标的 `confirm_continue` 停靠在聊天面底部浮层。**卡片浮在合成器上沿（留出安全间距，不遮输入框），随聊天面存在，不随转录滚动。画的是同一张卡（`ChatNoticeEventRow`）、同一套措辞、同一条应答路径（重投递；可答性仍由 `mayAnswerCard` 判），只有位置变了。多候选的 `session_choice` 没有唯一主语，留在原位不变。

**决策二：被问的那个块带轻微高亮与角标。**卡片与主语分居两处，视觉关联由块承担：淡 accent 底色 + "待选择"角标（`chatView.block.unanswered`）。角标是状态，不是第二个可点控件。

**决策三：块尾不再重复画这张卡。**宿主告诉块"你的问题已停靠"（`pendingAskDocked`），块尾留空——一张卡只画一处。

**不变量保留**：应答协议不变（答卡 = 原文本 + 选定地址的重投递，`message_id` 取原行）；布局映射不变（`keyOfSeq` / `blockSeqs` 仍把卡片的 seq 解析到它所指的块，locate 落点不漂）；视口带位（`useRevealPendingAsk`）保留——它把读者带到被问的那个块，而块上现在带着角标。

**范围**。Desktop 两个聊天主面（1:1 `AgentChatView`、群聊 `GroupChatPanel`）同批接入；Mobile 1:1 面的寻址卡仍以独立行呈现（其卡片渲染器另有分支），本次不动。2026-11-06 那条修订的"块尾挂载"仅对 Desktop 失效。

## 修订（2026-11-11·路由确认工具化）：`exec_session_route_confirm`——点击结果回到模型，纯文本不再是续投轮的合法答案（D6 修订）

**缺陷形状**。级别 5 的确认卡弹出后，人点"开新任务"：下一轮 router 带 `continuation_refused=true` 进入、候选为空，模型（实测 MiniMax-M3）这一轮没调用任何 act，直接写了纯文本。runner 的无-act 分支照旧发布 `agent_message` 行——卡片与回复并存。卡片的消失判据（`question_answered`）要求执行轮次回挂到所询问的行，纯文本不是轮次，卡片于是永远"待选择"。这违反 D6 的二选一契约（要么路由、卡收起；要么直接回复、不出卡）；也证明上下文提示对模型的约束力不够——模型对**工具返回**的遵循度明显强于对提示词段落的遵循度。

**决策一：确认做成挂起式工具**。`exec_session_propose_continue(session, why)` 更名为 `exec_session_route_confirm(target, why)`（旧名进 `RETIRED_ACT_NAMES` 漂移守卫）。模型调用它之后本轮挂起：写卡片行（形状不变，`system_notice` + `notice_type: "confirm_continue"`，幂等键不变），向 `ChatSessionManager` 的卡片注册表登记 oneshot 应答通道，等待上限 120s。三种结局：

- **continue**：alias→session_id 是 runtime 已知数据，直接投递目标会话，不再回模型——多一次模型往返就多一次不遵循的机会；
- **new task**：拒绝作为 tool result 回到模型，期望模型接着调 `exec_session_create(background)` 写出新会话的简报；若仍不调，degrade 到确定性新建路由——丢 `background`，不丢人的消息；
- **超时/取消**：卡片从注册表摘除，本轮落回既有阶梯（慢路径），此后人的点击按普通寻址处理。

挂起占用该 chat 的 drain task（per-chat 串行不变量本就一轮一任务），由 120s 封顶；超时走慢路径，后续消息不被一张没人答的卡阻塞。级别 5 表格行的语义随之收窄：`exec_session_route_confirm` 不再是"只提案不投递"——提案被采纳后**由本轮自己投递**，只是投递的触发从模型的第二次调用变成了人的那一次点击。

**决策二：点击在入队前被注册表认领**。inbound 环（`route_intent_inbound`）在进阶梯之前先查 `ChatSessionManager::claim_card_answer`：信封的 `chat_row_id` + `source_chat_session_id` 命中活卡即通过 oneshot 唤醒挂起的轮次并丢弃该信封（不进 ladder，杜绝双重投递）；未命中照常走现有 ladder——重启后注册表为空、卡片已超时、陈旧点击，全部落回修订（2026-11-11·寻址卡停靠底部浮层）与既有的重投递语义。前端零改动：卡片行形状、重投递 transport（`forceNew`/`targetSession`）、`question_answered` 投影、wire 字段 `allow_new_task` 都不动。

**决策三：硬拦截是防御纵深**。`continuation_refused=true` 且模型无-act 时：注入一条反馈消息重试一次；重试仍无-act 则 `degrade()`，绝不写 `agent_message` 行。它守护工具化的残余路径（超时、进程重启、陈旧卡片），与工具化互补而非替代——"卡片与 agent 消息并存"这个状态从此在代码层面不可表示。

**可观测性**。决策日志新增可选列 `confirm_wait_ms`（挂起时长）与 `confirm_result`（`continue`/`new_task`/`timeout`）；`act` 列出现 `exec_session_route_confirm:S_x` 即代表本轮写过卡。

## 参考

- [ADR-097](./ADR-097-in-conversation-history-search-disposable-index.md)（可弃侧表的完整模式：写时增量、失败只 warn、水位/首触重建、Windows 句柄顺序——本 ADR 的 D5 逐条沿用）
- [ADR-096](./ADR-096-no-full-session-reads.md)（写时派生 + 有界读——D4 的索引是它的直接兑现）
- [ADR-100](./ADR-100-chat-event-log.md)（被废止与被保留的清单见 D14；D1 的边界重述见背景 §4）
- [ADR-099](./ADR-099-chat-session-llm-router.md)（Router 分层与候选隔离保留，投递权收缩）
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（§6.3 / §6.4 / §6.17：跨机可达性与 `X-Error-Origin` 是 D5 成立的前提）
- [ADR-076](./ADR-076-multi-user-account-system.md)（单发起人与 `authorize_read`/`authorize_write` 分层——D10 的第三层）
- [ADR-084](./ADR-084-user-standalone-process.md) / [ADR-092](./ADR-092-user-message-identity-and-bounded-reads.md)（chat 数据归属与 per-session 单写者——D5 的落点依据）
- 设计文档 27 §5.5 / §6、设计文档 29（Chat Router 上下文编排）
- [HTTP 协议 §4.17 / §5.7 / §5.8](../../protocols/zh/http.md)（已录的线上契约：`GET /api/chat-blocks/{chat_id}` 的 refs 形状与四态、`/internal/chat-sessions/{id}/rounds` 缓存端点、Runtime 轮次投影读、第二级 `@` 的可寻址会话读）
