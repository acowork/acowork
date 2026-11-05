# Session 双层数据模型设计（持久化终态 + 运行时超集）

> 版本：v1.5 | 更新日期：2026-11-09
>
> 状态：模型基线（评审中）。本文只定"事实归哪一层、由谁拥有、谁可以推导"，不定 API 与实现形状。
>
> 修订记录
> - v1.5（P4 落地）：**卡片的"回答"也归服务端**。`session_choice` / `confirm_continue` 的回答是"人类自己那句话、换了个地址重投"，以前两个前端各写一份 `buildCardAnswer`，靠自己的已加载窗口找原行——窗口外的卡片因此成了**活按钮、点了没反应**（`if (!answer) return` 静默返回）。现在 `acowork-user` 新增 `chat_card::answer`（按坐标取原行、校验卡未答、校验操作者就是被问的那个人），`GET /api/chat-sessions/{id}/messages/{message_id}/answer` 下发"这句话 + 附件 + 已钉住的 `refs` + 原行 id"，前端只把点击（投给哪个 session / 开新活）折进去。同时修掉上一轮遗留的两处形状缺陷：`delivery.target` 改 `targets`（群 `@` 扇出一个目标一条 round，`find` 只取第一条把"投给谁"讲漏了）、sidecar 追加路径补写 `#v2` 头（首次写入不带头的文件每次重启都被判 v1 而全量重扫）。
> - v1.4（P3 落地）：红线进 CI——`dev/ci.sh run_chat_projection_redline` 查四件事：写入口（注册表兜底 + 唯一写入口内的契约校验）没被拆掉、公共页仍走投影而 Agent 侧页仍是裸行（两者方向都查，避免有人"统一"成一种）、surface 不得对 `in_reply_to` 做聚合推导（`some/filter/every/new Set/includes`；按坐标 `.find` 取一条已知行仍允许——§6 末段留给展示层的正是这个）、写者发出的 `notice_type` 必须已在 `chat_kinds` 注册。落地时发现词汇表缺一类：`budget_exhausted` / `truncated` / `unavailable` 报告的是**通道极限**，所指的请求从未成为行，因此不可能有坐标——新增 `LIMIT_NOTICES` 把它显式化（`delivery_failed` 不在此列，它欠的消息存在）。同时用 `chat_notice_write_classes_are_disjoint` 钉住"写契约"轴上的互斥，并记录 `RETIRED_NOTICES` 是另一条正交的轴（管渲染与结算）。
> - v1.3（P2 落地）：唯一推导点已实现——`acowork-user` 新增 `chat_view` 读模型，公共 `GET /api/chat-sessions/{id}/messages` 下发的每一行都带 `delivery`（`answered` / `answered_by` / `target` / `question`），两份前端的全部状态推导被删除。实现中确认三处边界，写进 §6.1：①推导必须能"只读被问的那几行"，因此存储层新增 `in_reply_to` 反向边（sidecar 升 v2，可按字节跨度取答复行，不做全量扫描）；②`question.answered` 的判据是"该消息出现了 round 引用行"，卡片自身不是答案（否则卡片一落盘就自我宣告已答）；③面向 Agent 的 `chat_history_read` 仍返回裸行——它把行喂给模型、按行计字节预算，派生状态在那里既无人渲染也挤占预算。另修一处升级缺陷（由测试逼出）：旧 v1 sidecar 被丢弃后，对账走的是"增量追加"分支，于是新写的行没有格式标记，进程每次启动都得全量重扫；现在"侧车不可信"会强制走"全量重扫 + 重写文件"。
> - v1.2（P1 落地）：写入口契约已实现——`IntentMeta` 成为 payload 注册表（`options` / `allow_new_task` / `refs` / `error_code`），未注册键用 `serde(flatten)` 兜住（读侧原样保留、不再二次丢弃），写侧在唯一写入口 `check_write_contract` 拒绝并返回 422；回复类行的线程坐标（`agent_message` / `interaction_request` / 线程型 notice）由写入口强制，卡片另需 `options` 与 `allow_new_task`。落地时按事实修正一处：`execution_round` **不是**回复类行——它是"回答行或起源行"，从 PM 任务 / 群 `@` 扇出 / 执行界面对话发起的 turn 在 chat 里没有上文，空坐标即该事实，强行要求坐标会把 block 挂到幻影消息下。实测另发现两个同类字段丢失：`refs`（ADR-106 修订的四元组，写入方一直在写，存储一直在丢，chips 因此不显示）与 `error_code`（`delivery_failed` 回执的原因码），已一并注册。
> - v1.1（评审后）：撤回 v1.0 的两处越界结论——①**不再把 `turn` / `question` 的终态结算进 `rounds.sqlite`**（该文件按 ADR-106 D5 是**可丢弃缓存**，`messages.jsonl` 是 chat 的唯一真相，把权威状态放进去会直接破坏 R1）；②**不再新增 `message.address` 字段**（投递地址已经由"round 引用行"承载，新增即为第二份表示）。相应地把"缺存储"重新定性为"缺写入契约 + 缺唯一推导点"。同时按评审补齐：Chat 数据归属、运行时状态宿主、与 ADR-106 D3 判据的关系、受影响的写入调用点。
>
> 关联：[ADR-106](../../adr/zh/ADR-106-chat-execution-reference-projection.md)（Chat/执行引用投影）、[ADR-084](../../adr/zh/ADR-084-user-standalone-process.md)（用户域含 chat 持久化与 API 归属）、[ADR-082](../../adr/zh/ADR-082-memory-storage-sqlite-vector-fts.md)（会话元数据与索引 sidecar）、[ADR-099](../../adr/zh/ADR-099-chat-session-llm-router.md)（Chat Router）、设计 15（对话持久化）、设计 27（Intent 消息与群聊）、设计 29（Chat Session 上下文编排）

## 1. 目的与范围

本文只解决一类问题：**前端要显示的状态，在后端数据模型里有没有一个显式的、可复算的事实来回答它。**

架构铁律：后端是唯一真实数据源，前端只负责交互与显示。在数据模型上这意味着两条判据：

1. **完备性**：前端需要显示的每个状态，后端必须能给出答案，前端不得自行推导业务状态。
2. **单一语义**：同一条业务事实只有一种表示。两处表示必然漂移，且漂移时无法判定谁对。

**归属**：Chat Session 的持久化与 API 归 `acowork-user`（[ADR-084](../../adr/zh/ADR-084-user-standalone-process.md)），执行 Session 的持久化与 API 归其 Runtime（ADR-082 的元数据 sidecar）。本文只讨论这两处内部模型与它们之间的引用关系。

范围外：具体 API 形状、UI 组件、路由算法、payload 键清单。

## 2. 事实起点

### 2.1 两类 session 已是同构的，只是未成为规则

| 角色 | Chat Session（`acowork-user`） | 执行 Session（Runtime） |
|---|---|---|
| 不可变事件日志（唯一真相） | `messages.jsonl`（+ `lines.index` 行偏移） | `conversations/{sid}.jsonl` |
| round 状态投影 | `rounds.sqlite`（**可丢弃缓存**，ADR-106 D5） | `{sid}.jsonl.rounds`（round 终态标记） |
| 会话元数据 | `session.json`（participants / kind / title / config / 计数） | 会话 meta 存储（initiator / visibility / chat 绑定） |
| 可丢弃索引 | `search.sqlite` | — |
| 附件 | `files/` | workspace 文件树 |

结论：两者都是"**追加日志 + 状态投影 + 元数据**"，差别在内容与写者，不在形状。因此**两层划分对两者同样适用**，不应各自长出一套。

### 2.2 现有模型的缺口（按"事实/推导"重新定性）

| 界面问题 | 事实应落在哪 | 现状 |
|---|---|---|
| 这条消息是否已被回答 | 转录（回答行指向它）——**已是持久事实** | 事实存在，但前端在客户端做匹配推导 |
| 这张卡片问哪条消息、能否答、是否已答 | 问句行 + 回答行——**已是持久事实** | 事实存在，但坐标由写入方填写不全，且前端各自推导语义 |
| 这张卡片是否过期 | 不存在此语义 | chat 卡片无过期；只有投递层有 response deadline |
| 消息投递到哪个执行 session | round 引用行的坐标——**已是持久事实** | 事实存在，前端按 `in_reply_to` 反推 |
| 消息内容里带的 payload | 日志行的一次性终态 | **无注册表、无边界校验**，未声明键被静默丢弃 |

据此，缺陷不是"缺存储"，而是三件事：**写入契约不完整**（坐标/载荷没写全）、**推导点分散在消费者**、**payload 无权威形状**。

### 2.3 运行时状态的宿主

- 执行 Session：有常驻宿主（SessionTask），运行期状态在内存，文件是最后一次检查点。
- Chat Session：**无常驻 session 宿主**。一轮路由是无状态的；其运行时状态由 **Gateway** 持有——投递表（mailbox：delivered / answered / accepted）、进行中的路由轮次、待发送草稿。Gateway 的 mailbox 有自己的持久化文件，但它按 `message_id` 索引，**不属于任何 session 模型**。

这条差异正是 Chat 侧更容易出问题的原因：中间状态没有"会话内的落脚点"，一旦不在读者侧集中推导，就会散落到每个消费者。

## 3. 第一原则：双层模型

### 3.1 定义

- **持久化数据模型（终态层）**：只保存终态。终态 = 一旦成立不再变化的事实，或最后一次结算的结论。
- **运行时数据模型（运行时层）**：终态层的**超集**，另含实时数据（存活、进度）与中间状态（pending / 已投递未答复 / 待结算）。

### 3.2 三条规则

**R1 可重建性。** `运行时 = f(持久化) + 输入`。判据：进程在任意时刻被杀，重启后前端要显示的每个状态都能从磁盘答出来。不满足此判据的状态即模型缺项。

**R2 结算规则。** 能由其他持久事实**唯一推导**的，不落盘；**不能推导**的，必须在状态转移点结算落盘。**反向约束同样成立：不得为派生视图新建存储**——那会造出第二份真相（v1.0 正是踩在这里）。

**R3 引用不拥有。** 运行时模型可引用外部真相（别的节点的 round、投递层状态），但只持坐标，不持副本。

### 3.3 判断某事实该落哪层

1. 重启后前端还需要它吗？——不需要 ⇒ 纯运行时数据。
2. 能由其他已落盘事实唯一推出吗？——能 ⇒ 不落盘，由唯一推导点现算。
3. 它属于别的服务或节点吗？——是 ⇒ 只存坐标。

其余一律结算进终态层。

## 4. 模型形状：四个概念，两种性质

| 概念 | 性质 | 终态载体（持久） | 运行时部分 | 唯一写者 |
|---|---|---|---|---|
| `message` | 持久实体 | 日志行：`kind` / `body` / `from` / `to` / `mentions` / `refs` / payload | 发送中、上传中、重试 | Chat 内的两类写者（见 §5.2） |
| `round_ref` | 持久实体 | 日志行：`{agent_instance_id, agent_session_id, round_id}` + `opened_new` | 该 round 是否仍在跑（执行侧回答） | Runtime（经 relay） |
| `turn`（一次投递） | **派生视图，不是实体** | 无独立载体：终态由"回答行"承载（`round_ref` / `agent_message` / 回执 `system_notice`，三者都以 `in_reply_to` 指向人类行） | pending / routed / 存活 | 投递层（Gateway mailbox）持有过程，不写转录 |
| `question`（一次问询） | **派生视图，不是实体** | 无独立载体：问句内容一次写入所在日志行即为终态；"已答"由指向被问消息的 `round_ref` 回答 | 开着的问句 | 提问方（agent 经 relay）写问句行；作答方写回答行 |

推论（四条硬约束）：

- **不为 `turn` / `question` 新建存储或侧车。** 它们是"投递层过程 × 转录层终态"的合成视图，落在唯一推导点（§6）。
- **日志行不可变。** 所有"当前状态"由终态行或合成视图给出，任何消费者都不得扫描日志自行判断。
- **每条回答行必须线程到被答复行**（`in_reply_to` = 被答复行的 `message_id`）。这是让派生可复算的前提，也是本轮缺陷的直接成因。
- **payload 是终态**：一次写死、原样持久化；未注册键在写入边界**报错**，不得静默丢弃。

**关于"投递地址"**：地址不是人类行上的字段。人类行落库先于投递（落库先于投递，§5.1），而"投给哪个 session"要到路由/重投递时才确定——因此地址的终态载体就是 `round_ref` 的 `agent_session_id`（配 `opened_new` 区分新建/续投）。既有 `to` 只表示"投给哪个 agent"（1:1 场景为目标实例，群聊场景为空并由 `mentions` 表达），`meta.agent_session_id` 不出现在人类行上。**新增 `address` 字段会构成第二份表示**，故不采纳。

## 5. 一致性规则

1. **一条事实一种表示。** 地址只由 `round_ref` 承载；"问的是哪条消息"只由问句行的线程字段承载；不得再引入 id 前缀编码等并行表示。
2. **写者权限属于模型。** Chat 多写者、执行 Session 单写者。当前 Chat 的写者共 6 处调用点、三类身份：①Gateway 自己的人话语写入口（`deliver_mentions` / `deliver_chat_message`）；②Gateway 的投递回执（`delivery_failed`、`budget_exhausted`）；③Runtime 经 `/api/intents/chat-append` relay 写 `execution_round` / `agent_message` / `system_notice`（含问句行）。准入规则（participant 校验 + kind 白名单）是模型的一部分，不得散落在入口。
3. **跨层不产生第二份真相。** 投递层（mailbox）与转录层不是同一事实的两份副本：前者是 liveness，后者是 outcome。转录**不得**复制投递层内部字段（`delivered_at` / `accepted` 等）；投递层**不得**承载会话内容。

## 6. 读模型：唯一推导点

推导只允许发生在**一处**：后端为界面组装的投影（Gateway 侧合成，chat block 的读取是既有样例）。这是写模型/读模型的分工——日志是写模型，投影是读模型（CQRS 的 read side）：读模型可以从日志派生，但派生只在服务端、只有一处、对消费者不可见。投影内部包含消息、每个块的 `ok / pending / not_found / unreachable`、每条人类消息的投递终态（是否已答、投给哪个 session）与开着的问句。

- **ADR-106 D3 的"已答判据"不变**：判据仍为"转录里存在指向该消息的引用行"，只是**执行点从表面搬到投影**。故本文不修订 ADR-106。（若实现期改为把 `answered` 收敛成状态实体，则必须同提交修订 ADR-106 中英文。）
- **消费者禁止推导清单**（出现即视为模型缺项，而非前端 bug）：按 `in_reply_to` 匹配转录行判断是否已答；按 id 前缀还原语义；按 `notice_type` / `kind` 推断实体状态；按"最后一条人类消息之后是否还有行"推断存活。

### 6.1 投影的形状（已落地）

读模型在 `acowork-user::chat_view`，公共消息页 `GET /api/chat-sessions/{id}/messages` 的每一行是"原样的一行 + `delivery`"（`#[serde(flatten)]`，行本身的字段一个不改，老消费方照旧可读）：

| 字段 | 含义 | 出现在 |
|---|---|---|
| `delivery.answered` | 有一行回答了本行 | `user_message`（显式 `false`：等待是状态，不是缺字段） |
| `delivery.answered_by` | 回答本行的那一行（日志序第一个） | 已答的 `user_message` |
| `delivery.targets` | 本 turn 落到的每个执行会话（`agent_instance_id` / `agent_session_id` / `round_id` / `opened_new`）——只有坐标，没有 round 内容。是列表：群 `@` 扇出一个目标一条 round | 已有 round 的 `user_message` |
| `delivery.question.answered` | 该卡片所问的消息现在有 round 了 → 问题已答 | 卡片（`session_choice` / `confirm_continue`） |
| `delivery.question.options` / `allow_new_task` | 卡片自己的答案与出口（payload 原样透出） | 卡片 |

三条实现约束：

1. **推导必须能只读被问的行。** 反向边 `in_reply_to → 答复行` 由存储层维护（`LineIndex.answers`，sidecar 升 v2 记录每行的线程坐标），查询按字节跨度只读命中行；全程不扫转录。它仍是**可丢弃派生索引**：丢了就重建，权威仍是 `messages.jsonl`。
2. **卡片不是自己的答案。** `question.answered` 只认"该消息出现 `execution_round` 引用行"；若按"任何回答行"计，卡片一落盘就自我宣告已答（这正是把"是否已答"交给通用 `is_answer_row` 会踩的坑）。
3. **Agent 侧读仍是裸行。** `chat_history_read` 走 `/internal/.../messages`，返回不带 `delivery` 的行：那份页按行计字节预算喂给模型，派生状态无人渲染且挤占预算。要状态就向服务要，不许自己推。

**仍然留给展示层的（不是状态推导）**：按 `in_reply_to` 把 round 引用行折叠到它所属的那条人类消息（`chatTranscriptGroups`）、按坐标取被回答行的正文/发送者（引用、标题种子）。这些是"按身份取一条已知行"，判据本身在投影里；只要没有第二处判断"答没答、投给谁、还在不在"，就不算违反 §6。

## 7. 分期与验收

验收口径固定为"**前端删掉一条推导逻辑**"或"**写入方补齐一条终态事实**"，不是"界面看起来对了"。

| 期 | 内容 | 验收 |
|---|---|---|
| P1 写入契约 **（已落地）** | 每条回答/回执行线程到被答复行（问句行亦同）；payload 按注册表原样持久化，未注册键报错。列出并改完 §5.2 的 6 个调用点 | 任何一条回复类行都无法在"没有坐标"的情况下写成功；payload 字段不再丢失 |
| P2 唯一推导点 **（已落地）** | 投影下发投递终态与开着的问句；前端删除全部推导 | 前端零推导；重启后仍能答"答没答、投给哪个 session" |
| P3 红线 **（已落地）** | 禁推导 / 未注册键与未注册通知类型进 CI（照既有红线脚本的做法） | 新出现的跨行匹配、未注册通知类型、被拆掉的写入口被 CI 拦下（id 前缀解析改由类型系统挡，见 §7.3 末段） |
| P4 卡片回答 **（已落地）** | 卡片的"回答"由服务端解析（原行内容 + 附件 + 已钉住的 refs），前端只折进点击 | 两个前端的 `buildCardAnswer` 与其窗口查找被删除；窗口外的卡片也能答 |

### 7.1 P1 的落地位置

| 约束 | 落地 | 测试 |
|---|---|---|
| payload 注册表 | `IntentMeta` 字段即注册表；未声明的键进 `IntentMeta::unregistered`（`flatten`，读时原样保留） | `unregistered_keys_in_history_survive_the_read` |
| 未注册键报错 | `check_write_contract`（`acowork-user`），`append_message` 内调用；内部 HTTP 边界映射 422 | `write_contract_refuses_rows_no_reader_could_resolve`、`system_endpoints_…`（422 断言） |
| 回复类行必须有坐标 | 同上；notice 的类型分类放 `acowork_core::intent::chat_kinds`（`THREADED_NOTICES` / `CARD_NOTICES`），三个 crate 共用一份 | `chat_notice_classes_are_registered_once` |
| 卡片必须有答案与出口 | 同上（`options` 非空、`allow_new_task` 必须声明） | 同上 |
| 卡片坐标正确 | Runtime 侧合并为一个坐标参数：`session_choice_meta(notice_type, request_message_id, options)`，`in_reply_to` 由它派生 | `a_session_choice_card_answers_the_message_it_asks_about` |

### 7.2 P2 的落地位置

| 约束 | 落地 | 测试 |
|---|---|---|
| 投影是唯一推导点 | `chat_view::read_page` + `DeliveryView` / `QuestionState`；公共消息页返回 `Vec<ChatRowView>` | `chat_view` 单测（人类行未答/已答/目标、卡片自问、旧里程碑不算答案）、`message_page_projects_delivery_state`（HTTP 端到端形状） |
| 只读被问的行，不扫转录 | `LineIndex.answers` 反向边 + sidecar v2（每行一列 `in_reply_to`）；`answering_rows` 按字节跨度取行，指纹不符（v1 sidecar）时全量重扫一次 | `answering_rows_reads_only_the_hit_lines`、`a_v1_sidecar_is_rebuilt_into_v2_instead_of_trusted` |
| 前端零推导 | Desktop：`turnSettled` 改读 `delivery.answered`，卡片选项/已答改读 `delivery.question`，删除 `roundTriggers` 集合与 `cardAnswered` 参数；Mobile 同构删除 | `chatTurnInflight.test` / `AgentChatView.test` / `GroupChatPanel.waitLine.test` / mobile `chat-turn-inflight` / `chat-notice-row` / `chat-turn-status` |
| Agent 侧读不带派生 | `RawMessagePage`（`/internal/.../messages`） | 既有 `chat_history_read` 用例 |

### 7.3 P3 的落地位置

`dev/ci.sh run_chat_projection_redline`（`check` 与 `all` 两个入口都跑）只查**形状**，不查风格——两个原始故障都是形状故障：

| 约束 | 落地 | 反例 |
|---|---|---|
| 写入口不被拆掉 | `IntentMeta::unregistered` 与 `check_write_contract` 必须在，且后者在 `append_message` 内被调用 | 把注册表兜底或契约校验删掉，payload 又会静默丢键 |
| 一个推导点、两份页 | 公共页必须是 `Vec<chat_view::ChatRowView>`，内部页必须仍是 `RawMessagePage` | 有人把 Agent 侧页也改成投影（按行计预算的页被派生字段挤占）或把公共页改回裸行 |
| 禁聚合推导 | surface 里 `in_reply_to` 不得与 `some(` / `filter(` / `every(` / `new Set` / `.includes(` 同现（注释行除外） | 原故障：`rows.some(r => r.meta.message_id === x.meta.in_reply_to)` 判"答没答"。按坐标 `.find(` 取一条已知行仍然允许（§6 末段） |
| 通知类型词汇表封闭 | 写者（runtime / gateway）发出的 `notice_type` 字面量必须出现在 `chat_kinds` 里 | 新造一个通知类型却不进 `THREADED_NOTICES` / `CARD_NOTICES` / `LIMIT_NOTICES`，它就绕过了坐标强制，落成孤儿行 |

第四条要求词汇表有第三类：`LIMIT_NOTICES`（`budget_exhausted` / `truncated` / `unavailable`）——它们报告的是**通道的极限**，所指的请求从未成为行，因此不可能有坐标。`delivery_failed` 不在其中：它欠的那条消息是存在的，所以照常线程。`chat_notice_write_classes_are_disjoint` 钉住这三类在"写契约"这条轴上的互斥（`RETIRED_NOTICES` 是另一条轴：管渲染与结算，与写契约正交，`interaction_resolved` 同时在两条轴上都是正确的）。

未做：**id 前缀解析**没有单独的红线。P2 删掉的那批（`kind` 从 `message_id` 前缀还原）已无残留，而"禁止某个字符串出现在任何地方"这类规则的误报率远高于收益；若要防它复发，正确做法是把 `ChatRowView` 的类型收窄到无前缀可解析（`kind` 必填），让类型系统而不是 grep 来拒绝。

### 7.4 P4 的落地位置

| 约束 | 落地 | 测试 |
|---|---|---|
| 回答的内容归服务端 | `chat_card::answer`（按 `in_reply_to` 取原行 → 校验卡类、未答、操作者是被问的人 → 组装 `text` / `attachment_ids` / `refs`）；`GET /api/chat-sessions/{id}/messages/{message_id}/answer` | `chat_card` 单测（原样回传、已答拒绝、旁观者拒绝、原行不存在、附件携带）、`a_card_is_answered_from_the_log_not_the_loaded_window`（HTTP：200 / 404 隐藏 / 422 非卡 / 409 已答） |
| 地址仍是点击 | 前端 `answerWithChoice(facts, choice)` 只把"投给哪个 session / 开新活"折进去；`directCardAddress` / `groupCardAddress` 不变 | `chatCardAnswer.test`（desktop / mobile）、`AgentChatView.test`（D6 重投断言） |
| 状态码即语义 | 409 = 卡片已被 round 结算（刷新即可，不报错）；404 / 422 = 这张卡再也不可能答（要说给人类听） | `chatSessionStore.cardAnswer.test`（desktop） |
| 群扇出多目标 | `DeliveryView.targets: Vec<RoundTarget>`（一个目标一条 round，各占一个 slot） | `chat_sessions_api` 端到端断言 + 前端类型 |
| 侧车出生即带版本 | `sidecar_append` 首次写入先写 `#v2` 头；`load_sidecar` 容忍撕裂的末行 | `a_sidecar_created_by_an_append_carries_the_marker` |

**仍然留给展示层的那一条**：按坐标取"一条已知行"仍允许（如 `chatTranscriptGroups` 用 `in_reply_to` 定位 trigger 行来分组、乐观标题用原行 body 播种）——它们只读一行、不判状态，且都有权威来源兜底。**状态**（答没答、投给谁、卡片开不开）一律只从 `delivery` 读。

## 8. 非目标

- 不新增持久实体、侧车或数据库：`turn` / `question` 是派生视图（§4）。
- 不把权威状态放进 `rounds.sqlite`（它是可丢弃缓存，ADR-106 D5）。
- 不改执行 Session 的日志语义与 round 引用方式（ADR-106 不变）。
- 不回填历史损坏行（payload 已丢的旧问询永久只读）。
- 不引入跨端代码生成。

## 9. 待决

1. payload 注册表的边界：注册的键集合、拒绝语义（422 还是 warn+拒绝写入）、以及是否区分"存储必需"与"仅展示"两类键。
2. chat 卡片是否需要"过期"语义（现状：无过期，卡片永久可答）。若要，则它成为**第一个不可推导的终态**，需要一个结算记录（写入新的事件行，而不是侧车）。
3. 投递失败时"人类预期地址"未留痕（因为地址由 `round_ref` 承载，失败则无该行）。是否需要在回执行补一个"预期地址"字段，留待产品确认。
4. 投影里 `delivery` 是否要带"投给谁但还没开 round"（当前只有 round 存在后才有 `targets`）：这关系到"投递成功但执行未起"的可视化，产品语义未定，暂由 `answered=false` + 等待态覆盖。
5. `system_notice` 的归位：建议收敛为"无状态提示"（回执、限额），有状态的问询在展示层单列一类；是否合并 kind 待实现期定。
