# ADR-106：chat 与 execute 的边界重划——引用而非副本

- **编号**：ADR-106
- **状态**：提议（待架构评审）——**地基部分已落地**：P1（`round_id` 铸造 + 轮次侧表 + 区间闭合）、P2（投影读接口）、P3（`rounds.sqlite` 缓存 + `GET /api/chat-blocks`）。P4–P6（chat 写侧切换为引用行、寻址阶梯与两级 `@`、卡片归块与前端块渲染器）**未实施**，因此本 ADR 列出的各"废止 / 退役"项目前仍然在册。
- **日期**：2026-11-02
- **决策者**：ACowork 架构组
- **关联**：ADR-100（本 ADR 废止其 D2/D8/D9/D10/D11/D12，重述 D1/D3 的边界）、ADR-099（收缩其 `dispatch` 权力）、ADR-105（废止其 D4/D5 的镜像判据）、ADR-097（可弃侧表模式的直接先例）、ADR-096（写时派生 / 禁止全量读）、ADR-092（per-session 单写者与有界读取）、ADR-084（chat 存储归属 acowork-user）、ADR-055（跨机可达性是本设计的支柱）、ADR-076（单发起人）、ADR-046（附件判别联合）、ADR-102（TurnOrigin 策略表仍生效）

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
- **`TurnSurface` 随之收缩**为 `{ chat_session_id, round_id, answerable_by, has_human }`——ADR-100 Rev.6 的"运行时从会话绑定补齐 chat 坐标"这段补丁逻辑保留，但 `trigger_message_id` 从 surface 上消失（引用行自己就是锚点，呈现层不需要反向指向触发行）。**落地修正**：这个事实并没有从系统里消失——D3 的行形状一直要求 `in_reply_to`，所以它改由 `TurnReference` 供应（与 `opened_new` / `assignment_brief` 同一位）。消失的是**镜像写入用它做幂等键 / 做线程指向**的用途，不是它本身。

### D3 — `execution_round` 是 chat 唯一的"工作"落盘单位

```json
{ "kind": "execution_round",
  "in_reply_to": "<触发它的 user_message id>",
  "meta": { "round_id": "…", "agent_session_id": "…", "agent_instance_id": "…",
            "opened_new": true, "assignment_brief": "…(可空，仅 new)" } }
```

- **无 `body`**：引用行不承载执行内容。呈现层的任务名来自 D4 投影（+ 会话标题）。
- **幂等键就是自然键**：`meta.message_id = "round:{round_id}"`。`round_id` 由铸造者保证唯一，因此重投递天然去重——**前缀键族约定（`exec:` / `tr:` / `card:` / `resp-`）整体退役，只剩这一条规则**。
- **一致性等级显式化**：引用行是**前置型**（写成功才允许投递；失败即返回错误，人类消息退回未投递状态，不产生"投了但看不见"的轮）。通道回执与人类消息行同为前置型。exec→chat 方向不再有第三种，因为不再有写。
- **`turn_routed` 退役**：它的对话事实（"你的消息去了哪里 / 新任务还是继续"）由引用行本身表达（有引用行 = 去了某会话；`opened_new` = 新/续）。ADR-100 Rev.3 恢复这一行的动机（"有来有回"）由 D9 的块呈现满足，而且比一条空正文通知更诚实。
- **写入者是 Runtime**，走 Gateway 的唯一 chat 写入口；不新增写入口，不触 ADR-009 §5 的 fs 红线（`dev/ci.sh` 的 `run_gateway_fs_redline` 仍绿）。
- **唯一写入点在 `AgentLoop::run` 的轮边界，不在传输层**（落地确认，"六站点收敛为一站"的确切落点）：引用行必须写在这一轮即将使用的 `round_id` 之下，而 D2 的答卡续跑规则让那个 id **不再是本轮入站消息的函数**——只有轮边界知道自己正要开哪一轮，传输层写就必然在续跑分支上指错轮。传输层因此只供事实（`TurnReference{trigger_message_id, opened_new, assignment_brief}`），不写行。Runtime 侧 `append_chat_row` 的调用点全仓只有这一处，这是"写入站点从 6 个变 1 个"的可验证形式。
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
| 5 | 无 `@`（单聊 / 兜底后） | **默认闲聊**；仅当明显是新任务 → `new_task(brief)`；仅当像接续某旧会话 → `propose_existing` + **确认卡** | 只能提议 |
| 6 | PM `task-dispatch` | 机器形式的显式寻址（`conv:task` 键） | 无 |

- **级别 1 是最高频连续性场景的正解**：「agent 刚汇报完进展，我回一句」不需要 LLM 猜——回复那个块就是寻址。这让级别 5 的"猜"用例大幅减少，确认的点击成本不摊到每条消息上。
- **级别 5 的确认必须有廉价出口**：只有存在**可投候选**（D7 的可见集合非空）时才提议；候选为空则静默走 `new_task` 或闲聊。确认卡上"新建任务"永远是并列选项，一次点击即可逃逸。
- **`dispatch(target=existing)` 的静默投递权删除**，Block C 的 `S1..S10` alias 表随之退役（寻址从"LLM 引用别名"变成"人指名"）。Router 的 act 空间：`answer` / `new_task(brief)` / `propose_existing(session, why)` / `ask_human(question)`。
- **`route_intent_legacy` 的 `routing_key` 猜测链**（`conv:task` > `chat:{csid}:{initiator}` > `conv` > `msg`）只保留 `task:` 与"引用行/ @ 指名"两支，`chat:csid:initiator` 的"默认回到上一个会话"一支删除——它就是"高概率出错"的那一类。
- ADR-099 的分层（D1）、无状态 Router + per-chat 队列（D2）、候选隔离（D4）、上下文三段式（D5）、chat/execute prompt 严格分离（D6）、失败降级（D8）**全部保留**；降级不再需要机器拼接 chat 摘要（级别 5 的默认目标是闲聊，不是某个会话）。

### D7 — `@` 的两级语义：agent 寻址 → session 选址

- **第一级（agent）**：`@` 的候选是 chat 的 agent 参与者。单聊只有一个 agent ⇒ 第一级退化为一，UI 不显示这一级，直接到第二级。这是"群与单聊同一逻辑"的落点：**单聊 = 群的特例（agent 数固定为 1）**。
- **第二级（session）**：候选集合 = **权限**（[chat_candidates.rs](../../../core/acowork-runtime/src/agent/session/chat_candidates.rs) 原样复用：1:1 只列绑定本 chat + 本人手起；群只列"我发起的"；跨 kind 互斥；`has_human=false` 的 agent↔agent pair 永不入列）。展示集合 = 候选 ∩ (`working` ∪ `idle` 且 `last_active < T`，默认 `T = 7d`)。`Errored` 保留（"需要你回去看一眼"是有效信号），终态/`Closed` 不入列。
- **`@` 是选址不是绑定**：会话归属仍由 D11 的单一真相决定；seed 只在**新建**时生效这一规则不变，且因为指名不再依赖映射表，"seed 静默失效"的分支消失。
- **两级 @ 都走同一个入口**（`POST /api/intents/mentions`，`targets` + 新增 `target_sessions`），客户端不再有"有 @ / 无 @"分叉——ADR-105 已消灭第一种分叉，这里消灭第二种。

### D8 — 交互卡片归入块，回答直投执行会话

- `ask_user_question` / 权限审批**不再往 chat 写 `interaction_request` 行**。卡片是"那一轮在等什么"的执行面事实，D4 的投影把它作为 `card` 字段吐出来，渲染在块内。
- **回答走执行会话自己的写入口**（`POST /api/agents/{id}/sessions/{sid}/…`，ADR-076 §决策 4 第二波的身份校验），Gateway 校验调用者是 `answerable_by`（发起人）且是该 chat 的参与者。`resolve_card_answer` / `mark_answered` / 回合末安全关卡 / `chatTurnState.closedCards` 全部退役。
- **群内可见性变化已被论证为不劣于现状**（D10）：今天 D9 的 echo 已把该轮输出吐给全群；新模型下参与者仍能看见块与卡片（读），只有发起人能答（写）。
- **`has_human == Some(false)`**（agent↔agent）不再需要"跳过写卡片"的特例——本来就不写 chat。

### D9 — "进行中"是呈现态，判据是正向证据

- 块显示"进行中"的**唯一 durable 判据**：存在 `execution_round` 引用行，且该 `round_id` 的**实时投影**（`source=runtime`）报 `terminal=false`。缓存里不可能出现 `terminal=false`（D5 的强制拒写），因此"进行中"永远不会来自一份陈旧的侧表快照。
- **不可达 ≠ 进行中**：Runtime 问不到且无缓存时该引用返回 `unreachable`（带重试入口），渲染为"暂不可达"而不是"还在跑"——把 outage 显示成进行中，等于让一个停掉的节点改写"发生了什么"。
- 这是**正证据**（有引用行 ⇒ 有一轮在跑），不是 ADR-100 D10 的"缺终态行 ⇒ 还在跑"的反证据。等待行的缺席派生逻辑（`opensAgentTurn` 认 `to` / `auto_routed_to` / `mentions` 三种证据 + `answeredAfter` 认 `delivery_failed` 收口）整体退役。
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
- PM `task-dispatch` 不再写 `user_message` 行（那条消息在执行会话里，投影自然可见），改写引用行。task 键路由保留（D6 级别 6）。
- 于是"谁往 chat 写东西"只剩两个生产者：**Gateway（人类 `user_message`）** 与 **Runtime（`execution_round` / `agent_message` / `system_notice`）**，且 exec→chat 方向没有任何生产者。

### D13 — 搜索、预览、未读、标题的重新归属

- **列表预览 / 乐观标题**：引用行的 `assignment_brief`（或首轮的一行摘要，由 Runtime 在写引用行时一并给出，一次性、不改写）足够。禁止为渲染列表去读投影（ADR-096）。
- **全局搜索 / chat 内搜索**（ADR-097 / ADR-081）：`search.sqlite` 索引 chat 自有行 + 引用行的 `assignment_brief`；**不索引投影缓存**——那会让一份可弃拷贝进入另一个可弃索引，删除语义变成两跳，且 exec 侧本就有 conversation index（ADR-082 / ADR-095）负责正文检索。跨会话正文搜索走 exec 索引，命中的是"某会话的某轮"，跳转路径已存在（D4 的"查看执行"）。
- **未读**：只算 chat 自有行；块的"等待人类"态作为独立的徽标位（来自投影的 `card != null`），不混入未读计数。

### D14 — 废止与保留清单

| 处置 | 条目 |
| --- | --- |
| **废止** | ADR-100 D2（两类指针行）、D8（`turn_mode` 与两态文案的 durable 来源）、D9（执行会话本地回合的 chat 回写）、D10（停写中间状态 + 前端派生等待行 + 本地 stop 落 `aborted` 行）、D11（`turn_routed` 唯一反应行）、D12（pin / `for_execution_session` 及其全部 Rev.4–7 修订）；ADR-105 D4（`auto_routed_to` 作为等待行证据位）、D5 的 chat 行形态（`delivery_failed` 改为 `system_notice`，保留"失败必须可见"的语义）；ADR-099 D3 中 `dispatch(target=existing)` 的静默投递权 |
| **保留** | ADR-100 D1（重述边界，见背景 §4）、D3（执行细节不进 chat/投影）、D4（"查看执行"跳转）、D5（卡片就地可答——落点改为块内）、D6（发送侧瞬态占位）、D7（turn 指针按 agent 归属化反应呈现——现由块承担）、Rev.3 头像不变量、Rev.8 的"执行面消息不进 chat 通道"不变量；ADR-099 D1/D2/D4/D5/D6/D8；ADR-102 TurnOrigin 策略表；ADR-105 D1/D2/D3/D6/D7（默认接收方本身、写入四道门、生命周期、不引入群级 LLM 路由） |
| **改写** | ADR-097 的搜索范围表述（D13）；ADR-096 的"写时派生"清单里新增轮次索引（D4） |

## 呈现契约（块 = 执行会话 explore 块的对话版）

执行会话里的 explore 块折叠了一次"多工具调用 + 思考"的过程；chat 里的块折叠的是一轮"用户消息 + 回复 + 等待中的卡片"。同一套折叠语法，不同的载荷密度。

- **折叠头**：agent 头像 + 名字 + 任务名（优先会话标题，空则 `assignment_brief`，再无则触发消息正文截断——ADR-100 D8 的任务名解析逻辑原样搬到呈现层）+ 状态词（进行中 / 完成 / 中断 / 失败 / 等待你）。
- **展开态**：默认最近 **2** 轮（长命会话取消轮转后必然几十轮，全展开不可读），其余按"更早轮次"分页拉投影。
- **一轮一气泡**：同一块的同一轮只有一个头像/名字/跳转（D7 的"一轮一气泡"不变，只是承载从"两行折叠"变成"块的一节"）。
- **跳转**：块头与每轮都保留"查看执行"（D4 of ADR-100），best-effort 打开该会话。
- Mobile 折叠为单行 + 跳转 + 底部 sheet 处理卡片，与 ADR-100 D5 的分工一致。

## 被拒绝的备选

| 备选 | 拒绝理由 |
| --- | --- |
| 继续做卫生工程（类型化 `ChatWrite`、统一幂等键、best-effort 写进 outbox 重试） | 治的是症状：写入站点从 6 个变 1 个才是根因。outbox 重试仍然要处理"跨进程写一个不存在于自己存储里的东西"这一整类失败模式 |
| 每个 exec session 在 chat 里只有**一行**引用（session 级而非轮级） | 时间线说谎：会话跨天推进后，块停在第一次出现的位置。"session 作为整体"是**渲染**目标（按 session 分组），不是**存储**单位（必须 per-round） |
| 引用行里冗余一份最终答案摘要（"轻量镜像"） | 立刻回到镜像：摘要需要更新，更新就是跨进程写，写失败就是谎。列表预览改用一次性 `assignment_brief` 满足，摘要不进 transcript |
| 缓存放 Gateway 自己的数据目录 | 生命周期与 chat 删除脱钩（隐私残留），且诱导 Gateway 持有转录数据。ADR-084 已把 chat 数据面归 acowork-user |
| 缓存放前端（IndexedDB / localStorage） | 违反"Desktop 仅呈现、无状态持久化"约定，且每端各存一份 |
| 不缓存，每次渲染远程读 | 首屏 N 个块 = N 次跨机 RTT；Runtime 重启后所有块变成空洞；离线/弱网不可读。用户明确要求落盘 |
| 轮次索引冗余 answer 全文而 JSONL 为真相 | 不采用：侧表只存 data_index 坐标，正文取 JSONL 主存。冗余正文会多出一份可弃拷贝，而投影只读两行，性能上并无收益；更重要的是它会把 ADR-096 的“写时派生”变成立一份可写的第二内容源 |
| LLM 判断连贯即静默续投旧会话 | Rev.4–7 与"每发一条多一个空会话"的缺陷族都源于此。高错误率的选择必须可撤销 = 确认 |
| 给投影开一个新的实时通道 | 已有 `session_state` / chunk 推送；新增通道只会造出第二块 live 真相（正是 D1 要杀的） |

## 影响

- **数据**：exec 侧新增 `round_id` metadata + `{sid}.jsonl.rounds` 可重建侧表（**随会话一起消亡**：`delete_session` 删掉它，`prune_excess_sessions` 把它转录一起归档，不留孤儿索引）；chat 侧新增 `execution_round` kind + `rounds.sqlite` 可弃侧表；`intent-sessions.json` / `terminal-sessions.json` 退役（**P5 已删除**：唯一真相是 `sessions` 表的 `intent_key` / `intent_closed` 两列（SCHEMA_VERSION 10）加上执行会话 meta 的 `chat_session_id`；两列不进 `SessionMeta` 结构体，因此 `build_meta()` 的全量 upsert 擦不掉路由决策，`upsert_row` 的显式列清单保证两个写入者互不干扰）。**不考虑兼容**：旧行按现状渲染（历史 `agent_message` echo / `interaction_request` 保留在盘上，渲染层按既有 `RETIRED_NOTICES` 风格隐藏或降级为块外的静态行，二选一在实施时定），不做迁移。旧会话无 `round_id` ⇒ 无轮次索引 ⇒ 投影 404 ⇒ 块降级为空洞（而非谎言）。
- **协议**：新增 `GET /sessions/{sid}/rounds/{round_id}`（Runtime）、`GET /api/chat-blocks/{chat_id}`（Gateway，**新顶层前缀**）、`GET`/`PUT /internal/chat-sessions/{id}/rounds`（user internal）；`/api/intents/chat-append` 的 kind 白名单从 `interaction_request|system_notice|agent_message` 改为 `execution_round|system_notice|agent_message`（`agent_message` 仅剩 Router 自答与 agent↔agent 的对话回答）；`mentions` 新增 `target_sessions`（agent → session 的第二级 `@`）与 `force_new`。三级寻址事实以 delivery `params` 键跨越 Gateway→Runtime（`target_session_id` / `explicit_mention` / `force_new`，两枚旗标只在为真时写入），因此阶梯的前三级零 proto 变更。Router 的提问卡走 `system_notice`：`notice_type` 为 `session_choice`（级别 3 多候选）与 `confirm_continue`（级别 5 提议），meta 携 `options[]` + `allow_new_task: true`，幂等键 `{notice_type}:{message_id}`——复用白名单已允许的 kind，写入口不新增。**`round_id` 不需要 proto 变更**（D2 的确定性派生以入站 `message_id` 为唯一输入）；`Intent.for_execution_session` 的删除仍按 Rev.8 已做的形状；`trigger_message_id` 作为 `TurnSurface` 字段与镜像幂等键在 P4 退役，但同一事实作为引用行的 `in_reply_to` 保留（由 `TurnReference` 供应，见 D2/D3）。
- **删除**：Runtime 的 `exec_echo_meta` / `turn_routed_meta` / `router_fallback_meta` / 三站点 `echo_terminal_row` / `write_interaction_card` / `close_chat_card*` / surface 补齐补丁（**P4 已全部落地**：Runtime 侧 chat 写入口只剩引用行一处）；Gateway 的 `resolve_card_answer` / `mark_answered` / 答案行写入 / sweeper 的 chat 失败行（改为通知）与前端整个 `chatTurnState.ts`、`ChatNoticeEventRow` 的 `RETIRED_NOTICES`、`closedCards` 簿记（**P6 删**——卡片归块与回答直投同批才有可答路径）。
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
| P6 | 卡片归块 + 回答直投 exec；前端 `chatTurnState` 退役；块渲染器（Desktop 两视图 + Mobile） | 等待态只由引用行 + `terminal=false` 决定；旧派生逻辑无引用 |

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

## 参考

- [ADR-097](./ADR-097-in-conversation-history-search-disposable-index.md)（可弃侧表的完整模式：写时增量、失败只 warn、水位/首触重建、Windows 句柄顺序——本 ADR 的 D5 逐条沿用）
- [ADR-096](./ADR-096-no-full-session-reads.md)（写时派生 + 有界读——D4 的索引是它的直接兑现）
- [ADR-100](./ADR-100-chat-event-log.md)（被废止与被保留的清单见 D14；D1 的边界重述见背景 §4）
- [ADR-099](./ADR-099-chat-session-llm-router.md)（Router 分层与候选隔离保留，投递权收缩）
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（§6.3 / §6.4 / §6.17：跨机可达性与 `X-Error-Origin` 是 D5 成立的前提）
- [ADR-076](./ADR-076-multi-user-account-system.md)（单发起人与 `authorize_read`/`authorize_write` 分层——D10 的第三层）
- [ADR-084](./ADR-084-user-standalone-process.md) / [ADR-092](./ADR-092-user-message-identity-and-bounded-reads.md)（chat 数据归属与 per-session 单写者——D5 的落点依据）
- 设计文档 27 §5.5 / §6、设计文档 29（Chat Router 上下文编排）
- [HTTP 协议 §4.17 / §5.7](../../protocols/zh/http.md)（已录的线上契约：`GET /api/chat-blocks/{chat_id}` 的 refs 形状与四态、`/internal/chat-sessions/{id}/rounds` 缓存端点、Runtime 轮次投影读）
