# ADR-102：TurnOrigin 轮次来源词汇表与 PM 派发链路治理

- **编号**：ADR-102
- **状态**：已接受
- **日期**：2026-10-25
- **决策者**：ACowork 架构组
- **关联**：设计 27（v2.8，§4.5 / §5.4.2 / §5.4.5 / §6.4 / §13）、ADR-099（Chat Router）、ADR-100（chat 事件日志）、ADR-060（系统块字节稳定）、ADR-076（PM 服务）、[修复计划](../../plan/zh/pm-dispatch-review-fix-plan.md)、runbook §7（混部版本偏差）

## 摘要

Intent 信封的 `kind` 字段此前只是各构造点随手填写的字符串，接收侧（Runtime 的路由、渲染、角色裁剪、写回）各自用不同启发式**重新推断**"这一轮为什么存在"，PM 任务派发因此被误当 agent-to-agent 应答轮处理。本 ADR 把"轮次来源"确立为一等概念：`acowork-core::intent::TurnOrigin` 枚举 + 策略表，生产端显式盖章、消费端一律查表；并顺带确立 PM 派发链路的三条规则——人读/机读文本拆分（playbook 走系统块 `assignment_brief`，是高信任字段）、task 坐标常量收口（禁止裸前缀匹配）、派发触发与重试幂等归 PM 扫描器（持久化 `dispatch_message_id` 按代次复用）。

## 背景

PM 派发重构（设计 27 v2.8 的前身）暴露出三类错位，根因同一个：**wire 协议里没有"这一轮为什么存在"的一等概念**，Gateway 的 7 个构造点与 Runtime 的 10 余个决策点各自即兴推断，且互相矛盾。

1. **角色裁剪错位（R1）**：PM 派发的信封 `kind` 硬编码为 `"request"`（与 agent-to-agent 共用），Runtime 的 `turn_kind_semantics` 把 `"request"` 映射为 responder——于是人类创建的任务轮被裁掉 `ask_user_question` 与 `intent_send`：执行任务的 agent 既不能向人提问，也不能请求其他 agent 协作。
2. **渲染错位**：`render_intent_message` 对 `"request"` 原样打印 `[intent:{from}:{action}] {params}` 信封 JSON。同一条派发消息在群聊里是人读文本（chat 行存的是正文），在执行 session transcript 里却是整段 JSON；会话标题从 transcript 首条 user 消息派生，JSON 又经"启动新任务：{标题}"的 ADR-100 反应行**二次泄漏**回群聊。三个表面互相污染。
3. **坐标潜规则**：区分"PM 任务轮"与"群聊轮"依赖 `conversation_id.starts_with("task:")` 字符串匹配，跨 Gateway / Runtime 散布 5 处，无类型、无文档，改格式即静默碎裂。

review（修复计划）另发现两个实现级缺陷印证了缺口的存在：唤醒 playbook 拆成信封字段 `assignment_brief` 后，Runtime 消费端**不看来源**——任何能构造信封的路径都能往执行会话系统块注入指令（信任边界缺失）；PM 派发重试每次铸新 `message_id`，chat 行与 Gateway 信箱预算按新消息重复计（幂等身份缺失）。

## 决策

### D1 — TurnOrigin 词汇表与策略表

`core/acowork-core/src/intent.rs` 定义 `TurnOrigin { Chat, PmTask, AgentRequest, AgentResponse }`，wire 上以 `kind` 字符串承载（`"user_message"` / `"pm_task"` / `"request"` / `"response"`），`TurnOrigin::parse` 对空值与未知值兜底 `AgentRequest`（混部旧生产者落在保守侧：宁可多一层 responder 约束，不可把 agent 应答当人类轮写回）。每个 origin 携带一张 `TurnPolicy`，四个决策点全部查表，禁止就地启发式：

| 字段 | 语义 | Chat | PmTask | AgentRequest | AgentResponse |
|---|---|---|---|---|---|
| `router_eligible` | 允许 Chat Router 选执行会话 | ✓ | ✗（task 坐标即路由） | ✗ | ✗ |
| `render_envelope` | transcript 渲染信封结构 | ✗（人读正文） | ✗（人读正文） | ✓ | ✓ |
| `responder` | 应对方角色：裁 `intent_send`/`ask_user_question` | ✗ | ✗ | ✓ | ✗ |
| `write_back` | 终态回写来源 chat | ✓（有 `message_id` 时） | ✓（群聊行） | ✓ | ✗ |

新增来源 = 加枚举变体 + 策略行，编译器强制所有决策点表态——这是"以模块设计替代逐点打补丁"的落点。

### D2 — task 坐标常量收口

`task:` 前缀格式（conversation_id `task:{project}:{task}`、routing key）的唯一实现收进 `intent.rs`：`TASK_CONVERSATION_PREFIX` / `task_conversation_id()` / `is_task_conversation()` / `task_routing_key()` / `is_task_routing_key()`。feature 代码禁止裸 `starts_with("task:")`（原 5 处全部替换）。

### D3 — 派发消息拆分：人读文本进 chat，playbook 进系统块

`build_dispatch_message` 拆为 `build_human_text`（标题+描述，进群聊 chat 行正文与信封 `message` 字段）与 `build_assignment_brief`（claim/submit playbook，信封独立字段 `assignment_brief`）。Runtime 仅在 `PmTask` 轮把 brief 注入执行会话**系统块**（ADR-060：每代次常量，字节稳定），不进渲染文本、不进任何聊天表面。chat 行 meta 只留 `task_id`（前端渲染任务胶囊，点击复制），`project_id` 移除——行本就活在该项目的群里。**该键必须在 chat 存储 `IntentMeta` 结构上声明**：此边界未声明的 meta 键静默丢弃（决策 29 的已知陷阱，本次真实踩中），两端各有测试钉住（生产端 pin 键集合、消费端 JSON 往返）。

### D4 — `assignment_brief` 是高信任字段，来源门控 + 出口剥离

brief 会进系统块（等同指令注入），因此：Runtime 侧提取（`extract_assignment_brief`）与消费（`session_task`）两道都按 origin 门控，非 `PmTask` 恒空；Gateway 侧对 agent 来源（`/api/intents/send`）的 params 在投递前剥离 `assignment_brief` 键——纵深防御，同时保护混部下不认识该字段的旧 Runtime 不被第三方注入。PM 服务是唯一合法的 brief 生产者（task-dispatch 端点）。

### D5 — 派发触发源收敛为 PM 扫描器

PM 侧不再在状态变更时同步调 Gateway，改由扫描器周期拉取到期任务（`start_at` 到期 + 审批闸门 + 依赖满足三条件）批量派发。事件钩子被否决：它把"何时派发"的领域知识（依赖树、审批态）漏给调用现场，且重启丢事件；扫描器天然幂等、崩溃自愈。

### D6 — 重试幂等：代次身份 + 有界退避

`Task.dispatch_message_id` 在**首次尝试前**落盘（serde default 兼容旧数据），重试复用同一 id——chat 行按 `meta.message_id` 去重、Gateway 信箱预算按 message_id 去重（每代次各占一份）；任务状态跃迁（重开/重派）时清除，退避计数按代次重计。退避 60s → 5m → 15m → 30m，连续 5 次失败放弃并 `tracing::error!`。Gateway `TaskDispatchRequest.message_id` 从"服务端铸造"改为"接受外部可选 id"（缺省仍自铸）。

### D7 — 版本偏差纪律

新 Gateway → 旧 Runtime 窗口内 `pm_task` 按 `AgentRequest` 兜底（功能性回退，不丢数据）。多节点部署 **Runtime 先行或同批**；回滚只 revert Gateway 侧即可，Runtime 无需同步回滚。表格见 runbook §7。

## 后果

- 修复：R1 角色裁剪（人类任务轮恢复 `ask_user_question` / `intent_send`）、transcript JSON 泄漏、标题二次污染、群聊事件弹窗缺失（PM 派发轮纳入 ADR-100 反应行）。
- 协议面收窄：`kind` 从自由字符串变为四值词汇表，新构造点必须表态；`assignment_brief` / `message_id` 进入 task-dispatch 契约（设计 27 §4.5 字段表为准，内部端点不重复进 protocols/http）。
- 遗留债务（记录于修复计划 §8，本 ADR 不覆盖）：Mobile 任务胶囊、扫描器 metrics / 优雅停机、派发放弃后任务面可见状态、TaskEditDialog date-only 时区漂移、`dispatch_due_ids` 万级任务性能。

## 备选与否决

- **Gateway 侧把派发信封 kind 改成 `"user_message"`**（最小补丁）：否决——绕过了"来源是一等概念"的根因，responder 裁剪修不掉，且 `starts_with("task:")` 潜规则继续扩散。
- **`source_service == "acowork-pm"` 字符串判来源**：否决——仍是跨进程无类型约定，与 D1 同病。
- **brief 作为 system-role 消息条目落会话历史**：否决——ADR-060 要求系统块字节稳定，且历史里存运行时拼接文本正是本次要消灭的冗余；改为每代次注入、不落 transcript。
- **保留事件钩子 + 扫描器双触发**：否决——两个真相源，幂等身份（D6）无法在两条路径上同时成立。
