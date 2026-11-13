# Intent 通信与聊天 Session 设计

> 状态：设计定稿（框架、需求、数据模型、协议、前端、模块边界已确认；API 与 schema 细节待实现设计）
> 版本：v3.6 | 更新日期：2026-11-13
>
> v3.6 为**派发闸门里的 `start_at` 改为 fail-closed**（2026-11-13）：扫描器的第三道闸门此前是「`start_at` 到期（缺省=立即）」——**没有开始时间的任务照样被派发**，于是「人忘了填时间」直接等于「agent 立刻开工」，而开始时间本该是人（或 PM agent）显式给出的排期。改为：创建时**没给** `start_at` 就落成 `now`（= 立即，与旧缺省同义）；`start_at` **为空**的任务 = **未排期**，扫描器**不派发**（fail-closed），只有显式排期后才走。存量待办在 PM 启动时一次性回填成 `now`（旧缺省值的同一个含义），不冻结。桌面端创建表单的开始时间预填「今天」，未排期的任务在任务详情里自己说出来。逐项见 §5.4.2。
>
> v3.5 为**执行块默认折叠与块外最新答复气泡**（ADR-106 呈现契约补正，2026-11-10）：块和块内每一轮改为**默认折叠**——转录读的是对话，不是执行日志；同时把这一会话**最新一条 assistant 答复**画在块的**下方**，用转录自己的 agent 气泡样式渲染（`data-run-answer`）。文本取自折叠着的轮次本来就带的投影 `answer`：取最新一个**已答复**的轮次——运行中的那一轮还没说话，所以屏幕上留着 agent 上一句话，而不是空出一个洞。于是折叠不再牺牲可读性：agent 最后说的话始终在屏幕上，工作细节点一下才展开；块自己打开的唯一情形仍是停在未答卡片上的轮（D8：藏在折叠后的可答项不可答），`hasFollowUp` 那套"最新一条时自动展开、被后文推走就收起"的状态机随之退役（桌面端；移动端保持原样）。数据模型、读取路径与后端零变更。逐项见 §5.5.5。
>
> v3.4 为**执行块位置锚定与寻址卡落地**（ADR-106 修订四 2026-11-06）：块此前锚在会话在本窗口的**最新**引用行，于是每次续投都把块拖到转录末尾、跨过中间所有行——几次往来后人的消息全在上、回答它们的块在最下，"一来一回"消失。改为锚定**最早**引用行：续投只增轮次与轮数、不动位置；块内展开最近 2 轮 + locate 映射保证不滞后，item 身份也随之稳定（不再每轮重建）。配套两件：①布局先算"哪些 seq 由哪块画"再出行（旧实现边走边写，越过后面的被折叠行会覆盖块自身映射，只靠"块总在最后"侥幸成立）；②`confirm_continue` 询问卡从"停在最新处的独立行"改为挂在其所指会话**块的尾部**（`session_choice` 多候选无唯一目标，留原位），并在卡片落位那一帧把视口带到该块（仅当块不在视口、一次、居中），取代原先把块拉到底部的做法。逐项契约见 §5.5。
>
> v3.3 为**执行块动作行与续投抽屉**（ADR-106 修订三 2026-11-06）：块的两端此前各挂一个交互带（上方"查看执行" pill、下方就地输入行），读起来像两个对象，且下方输入行即使不用也一直占着高度。改为：块内容上方一行纯图标动作——查看执行 / `@` 引用 / 复制整块 / 续投；发消息按钮自成胶囊，hover / 聚焦 / 草稿非空 / locate 上升沿时输入框向右抽屉式展开（图标留在胶囊左端），块底部不再保留任何输入行。`@` 复用第二级 picker 的引用材料通道（`ChatRefPick` → 主 composer 的可删 chip，1–5 上限、重复幂等），复制为整块的 Markdown 导出（会话名 + 每轮问/答，含折叠轮次；不可读的轮次写一行"无法读取"而非编造）。逐项契约见 §5.5.5。
>
> v3.2 为 **`@session` 引用重划与块内就地续投**（ADR-106 修订二 2026-11-04）：① 两级 `@` 的第二级 picker 从"这条消息投递到哪个执行会话"改为"引用哪几个执行会话轮作为材料"——行变**多选**（1–5 条），每行两个动作「引用 / 去续投」；"去续投"是导航（locate 该会话块 → 聚焦其块内输入框），不投递、不改消息承载；`target_sessions`/`target_session` 从人 @ 路径**窄退役**（仅寻址卡应答保留）。② 落库的消息带钉好的 `meta.refs`（四元组坐标，发送时由 Gateway 经 `rounds?latest=1` 钉轮；钉失败 400/502、行不落库、前端保草稿），渲染为用户气泡的来源 chips（点击 = 同 locate）；带 refs 的消息**跳过级别 3 续投分支**。③ 块展开态底部常驻一个轻量输入行（desktop 为 hover/focus reveal、草稿非空或持焦绝不收起；mobile 无 hover，展开即在 open 体底部、收起绝不出现），发送走级别 1（`sendGroupText(chatId, text, [], rowId)` / `deliver(text, rowId)`），与 pause 卡的"继续执行"（借道主 composer）并存不动。交付物化走 ADR-107 参照 part（新 kind=`referenced_round`），装配规则见设计 29 §11。不做兼容层：wire 直接改形，历史行无 `meta.refs` 天然为空、不迁移。逐项见 §5.5.5、§6.5。
>
> v3.1.4 为**块头主语与可见窗口修订**（ADR-106 呈现契约补正）：v3.1.1 把"一个执行会话"折成一块、块内默认只展开最近 2 轮之后，出现两种把话说错的情形。①**状态词的主语错位**：块头把 `session_state`（会话此刻在干什么）与"本块最新一轮的终态"混在一句话里，且"是否在跑"按整个会话任一轮 `pending` 投票——于是块里最后一轮的答复已经显示、状态词却还是"执行中"（会话正在跑一条本聊天还没有引用行的新轮），停在未答卡片上的轮也被说成"执行中"（它其实在等人），读成空洞的轮什么都不说。改为：状态词只有一个主语——**这个会话此刻在干什么**：会话在跑就照会话状态词说（哪怕块内最新一轮已答复，会话可能已在跑一条本聊天还没有引用行的新轮），会话空闲才落到本块最新一轮的投影（停在未答卡片上说等待你、终态说终态、空洞与故障说结果未知）。与此**独立**的一条断言是"在跑"标记（转圈图标、自动展开）：它只认本块最新一轮的 `pending`，会话另有新轮不许冒充本块在跑。②**触发行被窗口藏掉**：用户那句话的 `user_message` 行已折进块（一句话只出现一个地方），而承载它的那一轮可能落在"显示更早 N 轮"后面——于是整条转录里这句话消失，可见的几行反倒像 agent 在回答没人问过的问题。改为：承载触发行的那一轮与停在未答卡片上的轮常驻可见，只有块自己长出来的轮留在揭示之后。逐项见 §5.5.5。
>
> v3.1.3 为 **轮次门铃事件与重读装配修复**（ADR-106 修订 2026-11-03）：v3.1 把 exec→chat 降为零写时，连带删掉了通知面——镜像时代"写终态行成功"本身就是通知，零写之后没有任何东西替它说话，而轮次缓存按设计无失效路径（D5），于是块的唯一恢复路径是重读，却没人触发它。现场形状：停在 chat 界面，执行会话早已跑完，块一直显示 running；切进执行视图再切回才变 Done（因为那是一次重挂载）。修两处：① Runtime 在关闭轮次后经既有 chunk 通道发一枚 `round_state` 门铃（`SessionMessage` oneof 字段 34，topic `messages/round_state`，QoS 1、非 retained，载荷只有坐标 `round_id` / `chat_session_id`）；② 前端的 pending 重读从一次性 `setTimeout` 改为 `setInterval`（零写让重装配依赖永不变，定时器跑完就再也装不回来），周期从 4 s 放宽到 15 s 并降级为丢帧兜底。事件语义是"值得现在重读"而非"这是最终内容"：终态记录是会话单写者的另一次 append（实测晚约 8 ms），所以读侧带一次 500 ms 有界重试。逐项见 §6.2、§6.5。
>
> v2.8 为**轮次来源词汇表与 PM 派发链路修订**（ADR-102）：① Intent 信封 `kind` 定型为四值词汇表（`user_message` / `pm_task` / `request` / `response`，未知与空值按 `request` 保守兜底），接收侧的路由、渲染、角色、写回四个决策点一律由 `TurnOrigin` 策略表决定，废除以 `conversation_id` 前缀字符串匹配猜场景的潜规则；② PM 派发的唤醒 playbook 从用户消息正文拆为信封字段 `assignment_brief`，由 Runtime 注入执行会话的系统块，不进任何聊天表面、不随会话历史持久化；派发 chat 行只带人读文本与 `meta.task_id`（渲染为任务胶囊；`project_id` 移除——行本就活在该项目的群里）；③ 派发触发源收敛为 PM 侧扫描器（`start_at` 到期 + 审批闸门 + 依赖闸门），撤销事件钩子；投递重试以持久化的 `dispatch_message_id` 为代次身份幂等（chat 行与信箱预算每代次各只占一份），退避 60s→5m→15m→30m、五次失败放弃。逐项见 §4.5 字段表、§5.4.2、§5.4.5、§13。
>
> v2.7 为**执行会话轮转机制删除**（ADR-099 2026-10-24 修订）：T1（空闲间隔）/ T2（任务终态或发生过 compaction）与已取消的 T3（配置冲突）一并从设计中删除，连 successor / predecessor / handoff 与两个配置项。理由：execute session 本就是可一直追加的会话，压缩过的历史会话同样能继续接 dispatch与迭代；T1/T2 是「没有 LLM 做路由判断」时代用物理信号近似「任务该结束了」的产物，语义决策上提到 Chat 层后它们不再承担任何路由含义，而 T2 的 handoff 摘要正是本设计要消灭的污染。收敛后的规则：执行会话**长命**，上下文卫生只有 compaction 阈值，文件规模只有 JSONL 硬上限，开新会话只有三个显式来源（router `dispatch(new)` / PM task 终态 close / 用户手动新建）。逐项见 §2.4 术语表、§3.3 非功能需求、§3.4 决策 3 / 4 / 23、§4.1、§4.5.2、§6.2。
>
> v3.1.2 为 **等待行判据窗口延长**（ADR-106 修订 2026-11-03）：v3.1 把等待行的判据收缩为"本地在途的那次 POST"，而 POST 约 150 ms 就返回，它只证明投递已落地——该轮第一条 durable 行实测落在 0.008 s – 81.3 s 之后，那段窗口里转录一片空白、composer 又可发，用户读到的是"没发成功"。现行判据是本设备自己那次发送的 claim：`inflight[chatSessionId] = { messageId, at }` 由发送响应写入（1:1 `status === "delivered"`；群任一 target delivered、记**顶层** row id；`archived` / 全失败不建），收口于第一条 `meta.in_reply_to` 命中该 id 的行（`execution_round` / `agent_message` / 可见的 `system_notice`），300 s deadline 兜底。判据形状不变——仍是正证据、仍不读转录排列、刷新后仍无等待行——但同一个 claim 同时锁住 composer：上一句未落点前发不出下一句（系统里没有队列，所以不假装排队）。`sending` 退回原义（POST 在途），后端零协议变更。逐项见 §2.3、§3.3、§3.4 决策 7 / 17、§5.3.1、§5.5.5、§6.2。
>
> v3.1 为 **chat 引用行与读时投影修订**（ADR-106）：chat 转录每一轮只留一条**空正文引用行** `execution_round`，自然键 `round:{instance}|{session}|{round}` 即幂等 id，它指向的一切（答案、卡片、状态词）都在读时从执行侧投影、渲染成一个块。`interaction_request` 退出 kind 词表：卡片是执行面事实、在块内呈现，答它 = 写执行会话。`turn_routed` 反应行废止——同一事实由块头承载，不再可能与执行侧对不上。等待行的唯一判据改成本地在途的 POST（该窗口在 v3.1.2 延长为发送 claim），所以"进行中"是正证据，不是从一次写失败反推出来的结论（`meta.auto_routed_to` 一类的证据位随之退出）。寻址成为显式阶梯（回复目标 → 两级 `@` → 候选 → 新建），答寻址卡 = 把人类那句话重投递一次，转录里不会同一句出现两次。会话绑定不再有侧文件：`intent-sessions.json` / `terminal-sessions.json` 退役，真相是 `sessions` 表的 `intent_key` / `intent_closed` 两列。exec→chat 方向的跨进程写降为 0，Runtime 侧 chat 写入口只剩引用行一处。不做兼容层、不迁移历史：被废止的行类型留在盘上、在渲染期隐藏。逐项见 §3.4 决策 12、15、17、18、20 / §4.2 / §4.2.2 / §5.5.4 / §5.5.5 / §6.1 / §6.2 / §6.3 / §6.4 / §6.5 / §13。
>
> v3.0 为**取消执行面消息的 chat 镜像**（ADR-100 Rev. 8）：执行会话里敲的消息只写执行会话自己，不再落 chat 的 `user_message` 行；随之整体删除 pin（`for_execution_session` / `pinned_agent` / chat 入口的 `command`）与它为落空兜底而生的机制。理由：镜像把执行面的消息变成了 Router 的输入，pin 与全部兜底都是在抵消自己造出来的问题；取消需求后，"执行面消息新建执行会话"在构造上不可能。逐项见 §2.2 决策 29、§5.5.5。

> v2.9 为 **pinned 回合落点修订**（ADR-100 D12 Rev. 7）：执行视图发出的消息，其 pin 不再是"目标未加载就丢弃"的提示，而是一次**投递指名**——只是 `Closed` 的会话（进程重启后每个会话都是这个状态）经 ADR-038 的开机状态机**重新打开**并由它接住这一轮，而不是让 pin 落空、转交 Chat Router 后新开一个空会话（实测：重启后在一个历史执行会话里继续说话，每发一条消息就多一个执行会话，一次现场留下 4~5 个只含一条用户消息的会话）。pin 兑现不了（会话已删除 / 不属于本次发送的 chat / 发起人不是发送者）时该轮由**确定性路由**收尾，**永不进入 Chat Router**——新开执行会话是 Chat 会话的权限，执行会话自己发出的用户消息只能回到它自己（除非它发的是 intent）。pinned 回合因此也进同一条 per-chat 队列（`PendingTurn::pinned_session_id` 区分投递作业与路由作业）：重新打开会话是磁盘工作，不能压在 MQTT 入站循环上。逐项见 §2.2 决策 29、§5.5.5。
>
> v2.6 为**执行视图输入入口修订**（ADR-100 D12）：绑定 chat 的 execution session，其 composer 的发送改走 `POST /api/intents/chat-message` 并携带 `for_execution_session` 指名自己——Gateway 以**调用者身份**落一条 `user_message`（补上 v2.5 只回写尾、不落头的半轮对话），Runtime 收到带指名的消息后**跳过 Chat Router** 直投该会话，以 `SessionMessage::ChatMessage`（不是 `Intent`）进入与执行面原路径逐项等价的流程，因此 slash 技能、附件、JSONL 用户行**零能力回退**。pin 是提示不是闸门（目标未加载 → 清空 hint 落回正常路由）；`message_id` 由客户端 mint，在 chat 行 / 执行会话 JSONL 行 / 乐观气泡三处对齐。逐项见 §2.2 决策 29、§4.2 kind 表、§5.5.5。
>
> v2.5 为**路由反应行与降级落点修订**（ADR-100 D11 / ADR-099 D8 修订）：① 恢复且仅恢复一条 durable 反应行 `turn_routed`——路由结论得出时写入，渲染为 agent 头像 + 名字 + 气泡（"启动新任务：X" / "继续之前的任务：X"），补上 v2.4 连带取消的即时对话反馈（用户消息之后必须紧跟 agent 消息的交互感不变量）；② chat 来源的人类消息在 router 降级时**新建**执行会话而非复用 intent_map 映射；③ 确立头像不变量：居中终态行不折叠其后 agent 气泡的头像。逐项见 §2.2 决策 20 / 28、§4.2 kind 表、§5.5.5。
>
> v2.1 为 **user-agent 单聊形态修订（微信模型）**：每个 (user, agent) 恰好一条 Chat session，作为 Agent 页标签栏的**固定 tab**（标题 = Agent 名），不可关闭、不建列表；execution session 是同一标签栏里可随意开关的普通 tab，与 Chat tab **共存**而非互斥。撤销 v2.0 §5.4.4「点 `+` 新建第二条同 pair 会话」与 §5.5.2「Chat history 列表按钮」。逐项见 §13 v2.1 修订。
>
> v2.0 为**结构重排**：内容按「整体框架 → 需求分析 → 数据模型 → 通信协议 → 前端交互 → 模块分解 → 安全权限 → 落地计划」重组，新增框架图、术语表、非功能需求、数据结构与对外接口、三形态时序图、前端交互设计、权限模型与 ADR 一致性核对。章节编号全部变更，新旧对照与已失效引用清单见 §12。
>
> 关联设计：[06-communication.md](./06-communication.md)、[12-tool-system.md](./12-tool-system.md)、[15-conversation-persistence.md](./15-conversation-persistence.md)、[21-pm-project-management.md](./21-pm-project-management.md)
> 关联协议：[MQTT 协议](../../protocols/zh/mqtt.md)、[HTTP 协议](../../protocols/zh/http.md)

## 1. 概述


本文定义 ACowork 的 Intent 通信与聊天 Session 设计，覆盖用户、Agent、群聊及 PM 之间的消息语义、存储边界、接收 Agent session 选择、对话所有权、回复送达与历史读取。

**Intent 是一条发给 Agent 的消息，不是任务编排协议。**对接收 Agent 而言，Intent 等价于一条触发 Agent 执行的用户消息。发起方可以是真实用户、另一个 Agent，或代表项目 owner 行事的 PM。接收 Agent 在自己的 Agent execution session 中处理消息，处理过程和思考留在该 Agent session；请求与回复的方向语义住在**投递信封**与 Gateway mailbox 里，共享 chat session 里只留下这一轮的引用行（v3.1，ADR-106 D12）。

**核心原则：Chat session 记录交流；Agent execution session 记录执行。**二者是不同的会话模型，有各自的数据格式、写入方、生命周期和前端入口。Intent 将 chat session 中的一条消息路由为目标 Agent execution session 的输入；Agent 那一轮的答复留在执行侧，chat 只在轮次边界留一条引用行、读时把答复投影进来（v3.1，ADR-106 D3 / D12）。

项目群聊是 `acowork-user` 聊天域中的一种特殊 chat session。未来的普通用户群聊、user-agent 群聊和 Agent-Agent 一对一聊天使用同一聊天模型，不为每种参与者组合另建一套会话存储格式。

**本设计关闭的一个结构性缺口：user-agent 单聊此前没有 Chat session。**类型表里列了 User-Agent chat，但落地路径把它排除在 Chat session 之外（用户与 Agent 的普通对话只走 execution session + Session history），于是"打开一个 Agent"和"打开一个聊天对象"是两件不同的事，Agent 页面无法与群聊共用同一套会话 UI。本版把 user-agent 单聊定义为 Agent 页面的默认 Chat session（§5.4.4）：用户打开 Agent 即进入与它的**唯一**默认会话（每 pair 恰好一条，像微信里与联系人的对话），在其中发消息触发该 Agent 的 execution session 执行任务。群聊、单聊、agent-agent 从此是同一个模式的三种实例。

## 2. 整体框架

本章是全篇的心智模型：三种会话形态、两层 session 职责、以及它们之间的消息流。后续各章展开细节；遇到具体规则时，回到本章定位它在整体中的位置。

### 2.1 三形态框图

ACowork 的会话只有三种形态，全部共用同一套 Chat session 存储、同一消息词汇表、同一唯一写入口。区别只在**参与者构成**与**触发规则**。

#### 2.1.1 User-Agent（单聊）

```mermaid
graph LR
    U["user-A"] -->|"user_message + 附件"| CS["Chat session<br/>kind=user_agent"]
    CS -->|"投递（发起方角色）"| ES["Execution session<br/>agent-A"]
    ES -->|"轮次边界写一条引用行"| CS
    CS -->|"块的读时投影（答案 / 卡片 / 状态）"| U
    ES -.->|"需要决策"| CARD["块内卡片（投影，不是 chat 行）"]
    CARD -.->|"答复直投执行会话"| ES
```

打开 Agent 即进入该 pair 唯一的默认 Chat session（固定 tab，标题 = Agent 名，v2.1）；每条人类消息都触发 Agent 处理，不需要 @。

#### 2.1.2 Group-Agent（群聊 / 项目群聊）

```mermaid
graph LR
    U1["user-A"] --> MSG["user_message"]
    U2["user-B"] --> MSG
    MSG --> GC["Chat session<br/>kind=user_group / project_group"]
    GC -->|"仅显式 @Agent（每目标一份）"| R1["Chat Router<br/>agent-X"]
    GC -->|"@ 另一个 Agent"| R2["Chat Router<br/>agent-Y"]
    R1 -->|"复用 / 新建"| ES1["Execution session<br/>agent-X"]
    R2 -->|"复用 / 新建"| ES2["Execution session<br/>agent-Y"]
    ES1 -->|"引用行"| GC
    ES2 -->|"引用行"| GC
    GC -->|"push 给全体参与者"| U1
    GC --> U2
```

普通群消息只存档不触发；@ 才定向投递，且 @ 只决定"唤醒哪个 Agent"，不决定"进入哪个执行会话"。每个被 @ 的 Agent 各自跑一次 Chat Router（ADR-099），在本群候选集（仅 `meta.chat_session_id` 指向本群聊的执行会话，含该群的 PM 任务会话）内按语义复用或新建；因此同一群里的多个话题可以并行到不同执行会话，而不是全部串进一条 `chat:{group}` 链。多 Agent 被 @ 时各自独立执行、独立写回。

#### 2.1.3 Agent-Agent

```mermaid
graph LR
    AES["agent-A execution session"] -->|"Intent request（投递，不落 chat 行）"| BES["agent-B execution session"]
    BES -->|"轮次边界：一条引用行"| ACS["Chat session<br/>kind=agent_agent"]
    BES -->|"Intent response（投递，不落 chat 行）"| AES
    ACS -->|"块的读时投影"| V["参与者的转录视图"]
```

无人类参与者，因此不产生决策卡片；v3.1 起 A-B 的 request / response 都不再落 chat 行（ADR-106 D12）——pair chat 仍是转录容器，只承载应答方那一轮的引用行。A-B 交流**永不**写入 A 与用户的单聊会话。

### 2.2 两层会话模型

```mermaid
graph TB
    subgraph CHAT["Chat session 层（acowork-user 聊天域）"]
        A["user_message / agent_message / execution_round（引用行→块）/ system_notice"]
    end
    subgraph ROUTER["Chat Router 层（Runtime，每轮无状态 LLM turn）"]
        R["短期历史投影 + 候选执行会话 + draft config<br/>→ 回答 / 新建 / 提议续投（需人确认）"]
    end
    subgraph EXEC["Execution session 层（Agent home）"]
        B["思考 · 工具调用 · 文件修改 · 完整上下文"]
    end
    A -->|"逐条投递（保留 message_id 与信封坐标）"| R
    R -->|"dispatch：语义上该由谁继续"| B
    R -->|"纯文本回答（不调用工具）"| A
    B -->|"轮次边界：一条空正文引用行"| A
```

边界不变量见 §2.3：Chat session 记录交流，Execution session 记录执行，Chat Router 决定"这条交流该落到哪个执行会话"。Router 是**无状态 turn**——它读 Chat session 的投影，不拥有持久会话、不写执行轨迹；产出只有两种：一条面向用户的回答，或一条带目标的投递决定。


### 2.3 Session 边界不变量

| 维度 | Agent execution session | Chat session |
|---|---|---|
| 职责 | Agent 执行上下文 | 用户与 Agent / Agent 之间的消息历史 |
| 权威存储 | 对应 Agent home / Runtime，沿用现有格式 | `acowork-user` 聊天域 |
| 内容 | 用户输入、模型输出、思考、工具调用等现有运行历史 | 固定类型的消息：`user_message` / `agent_message` / `execution_round`（空正文引用行）/ `system_notice`，含附件 |
| 写入者 | 对应 Agent Runtime / 现有 session API | `acowork-user` 的唯一聊天写入口 |
| 是否有 LLM | 是（执行 turn，工具集完整） | 是（Chat Router：每轮无状态、只读投影，工具集仅 `dispatch`） |
| 前端入口 | Agent 页统一标签栏里的 execution session tab + Session history | Agent 页固定 Chat tab（每 pair 唯一会话，v2.1） |

禁止把 Chat session 的消息复制成 Agent 思考或工具调用；也禁止把 Agent 内部思考、工具调用写进任何 chat session。Chat 消息与执行历史是两个不同的视图域（v2.1：user-agent 单聊的 Chat 视图是固定 tab，execution 历史经 Session history 与 exec tab）。

**v2.2：Chat session 成为 LLM 的输入，但不是 LLM 的存储。**引入 Chat Router 后，chat 层确实有一个 LLM 在读这段历史，但它是**每轮重建的无状态投影**：不落 router 会话、不写思考、不累积自己的上下文，chat 历史仍是唯一权威（ADR-099）。因此上表的分工不变，新增的只是"谁来决定消息落到哪个执行会话"——由语义判断承担，而不是由 `routing_key` 的键运算承担。

**引用行不是例外，它是边界本身的一个指针（v3.1 修订，取代 v2.2–v2.8 的"卡片是唯一例外"）。**一轮的执行内容从不进 Chat session；进 Chat session 的只有一条空正文的 `execution_round`，坐标 `round:{instance}|{session}|{round}` 指向执行侧。它携带的不是正文、不是工具 payload、不是思考，而是"这一轮存在、由哪条消息触发"这一交流事实；答案、卡片与状态词在读时从执行侧投影，渲染成块（§5.5.5）。**卡片因此不再是一种 chat 行**：`ask_user_question`、工具审批这些"需要有人做决定"的东西以非终态 `card` 快照进入那一轮的块，答复直接回到执行会话（§5.5.4 / ADR-106 D8）。区分标准不变：**这条消息的内容是否对会话的其它参与者有意义**。有 → 进 Chat session；只是 Agent 自己的进度与执行正文 → 留在 execution session，靠引用行被读到。

轮次进度同样遵守这条：**"已派发 / 已开轮"这类运行时状态不进 Chat session**，它只在当前窗口以一条前端派生的等待行呈现（判据是本设备自己那次发送的 claim——从 POST 在途起，到那句话在转录里有落点为止）；进 Chat session 的是对话本身、指向该轮的引用行，以及"为何没有回复"这类通道事实（`system_notice`）；**处理到哪一步、正在调哪个工具，不进 Chat session**，靠块头"查看执行"跳转进 execution 视图（§5.5.5）。

### 2.4 术语表

| 术语 | 定义 | 不是什么 |
|---|---|---|
| **Chat session** | 持久对话记录，`acowork-user` 聊天域拥有，五种 kind 共用同一物理结构 | 不是执行容器，不含思考与工具调用 |
| **Execution session** | Agent 的执行上下文，Agent home 内的 JSONL + meta | 不是对话记录，不跨 Agent 共享 |
| **kind** | Chat session 的参与者构成类型：`user_user` / `user_group` / `project_group` / `user_agent` / `agent_agent` | 不是物理存储差异 |
| **Intent** | 消息上的一组寻址与回复**字段**（`conversation_id`、`in_reply_to`、`agent_session_id`、`repo`、`status`） | 不是一种消息类型，不是任务编排协议 |
| **发起方 / 应对方** | 一轮 turn 的角色，由**入站消息类型**决定，不由模型声明 | 不是模型可自选的属性 |
| **routing key** | Runtime 侧逻辑执行映射的键：`task:` > `chat:` > `conv:` > `msg:` | 不是物理 session id；它指向一个长命的 execution session |
| **Chat Router** | 人类消息的语义路由：每轮无状态 LLM turn，只产出回答或 `dispatch` 决定（v2.2） | 不是常驻会话，不是执行者，不拥有上下文 |
| **候选集** | router 可选的目标执行会话清单，按 chat 绑定与 owner 硬过滤（≤ 10） | 不是全量 session 列表，不是权限授予 |
| **执行轮引用行** | `execution_round`：空正文，只报坐标，渲染时读执行侧投影成一个块 | 不是执行内容的副本，不是状态镜像 |
| **决策卡** | 执行侧产生、随轮次投影进入块内，阻塞，需要人答（v3.1：不再是 chat 行） | 不是执行进度展示，不落 chat |
| **在途等待行** | 运行期由前端派生、不落盘的居中状态行（仅本设备持有未结算的发送 claim 时），不阻塞，但锁住本面 composer | 不是持久化事件行，不是 live 镜像卡，不代表任何轮次 |
| **唯一写入口** | 每个 Chat session 的单写者串行队列，`seq` 单调、`message_id` 幂等 | 不是全局串行 |
| **可见性** | 读权限作用域：owner / Chat session 参与者 / 平台级 | 不是「群聊 = Public」 |

## 3. 需求分析

### 3.1 设计目标

1. 提供统一的来源会话 / 回复地址模型，支持 user-user、user-agent、Agent-Agent、PM 派单和群聊中 @Agent。
2. 让 Agent 保留现有 Agent session 存储格式和模型；Intent 只触发目标 Agent 在其 execution session 中处理用户消息。
3. 在聊天域中统一保存聊天消息；聊天记录绝不包含 Agent 的思考过程、工具调用或工具执行细节。
4. 由明确的业务入口决定目标 Agent execution session 的创建或复用，不要求发送方或模型推断 thread 边界。
5. `repo`（代码仓库地址）是 Intent 的可选字段，项目有仓库时由系统在派发时自动注入；授权用户之后可以修改 session 配置，包括工作目录。
6. 单个 chat session 的消息由唯一后端写入口按序串行写入，避免并发写入和顺序不确定。
7. 结果通过 push 送达：应答方的回复由系统路由回发起方那一轮（v3.1 起不再写回 Chat 行，答复由那一轮的投影呈现，ADR-106 D12），发起方不轮询取结果。
8. 一次对话有唯一 owner（发起方），由 owner 决定何时结束；应对方没有发起新对话的能力。
9. `chat_history_read` 按 offset/count 渐进读取 Chat session 中"不属于本次对话"的消息，并由服务端实施硬性读取上限。
10. Agent 在群聊中默认不发言；明确 @Agent 才向其发送 Intent 并触发处理。

### 3.2 非目标

- 不为 Intent 引入复杂任务生命周期、回调编排或通用状态机。
- 不把 Agent execution session 改造成群聊存储，也不将群聊消息注入 Agent 的执行历史文件。
- 不允许普通聊天消息隐式广播给群内 Agent。
- 本文不要求实现外部 ACP 兼容 API；ACP 仅作为能力描述、消息结构等方面的参考。

### 3.3 非功能需求

| 维度 | 要求 | 依据 |
|---|---|---|
| **成本** | 决策卡片默认不结束本轮；只有无人类可答或超时才以 `waiting` / `aborted` 收尾。一次任务的审批次数不得与 turn 重放次数线性相关 | §5.5.4 |
| **延迟** | 发送侧即时反馈由前端派生的等待行承担（判据是本设备自己那次发送的 claim，v3.1.2 起它覆盖"发出 → 落点"整段，不再在 POST 返回时撤行；v3.1 起不再叠加"该 chat 有 agent 参与者"这类推断，也不靠落库） | §5.5.5 |
| **可靠性** | 任何属于某个 chat 的轮次无论成败都必须把终态交给投影（v3.1：写回 = 引用行 + 投影，不再是回答行），含崩溃兜底；不允许存在「发完消息永久无响应」的状态 | §5.5.1 |
| **幂等** | `message_id` 全 kind 必填，重复投递不产生第二行；`seq` 单调 | §4.6 |
| **可回滚** | 不迁移历史；Chat surface 不可达（无账号 / 用户服务异常）时 Agent 页退回旧 exec 形态 | §9.1 |
| **安全** | 无人类可答的会话不得创建需要人的东西；越权读执行记录返回 hidden 404；卡片不得成为枚举探针 | §8 |
| **可观测** | 对话预算、路由决策、写回失败均须可查，且 Gateway 重启不得丢失预算计数 | §5.7 |
| **边界纪律** | Gateway 不承载业务逻辑，不直接访问 Node-local Agent home；业务事实由 Runtime 经唯一写入口落库 | §7.1 |

### 3.4 决策结论

以下为讨论中已确认的决策，作为本设计的结论性内容。

1. **Chat session 只存固定类型消息。**不包含 Agent 思考过程、工具调用或工具执行细节。（v3.1 修订，ADR-106 D8：卡片不再是语义上的例外——它根本不进 chat，随那一轮的投影呈现；留在 chat 里的只有两类寻址问题 `session_choice` / `confirm_continue`，它们说的是"这条消息没人接"这类通道事实，不是执行事件本身。）

2. **Chat 消息与执行记录是两个独立视图域。**user-agent 单聊的 Chat 视图是 Agent 页的固定 tab（不靠列表入口进入）；execution 记录经 Session history 与 exec tab 可达。群聊 Chat session 从项目等业务视图进入（v2.1 修订，原「两个入口」表述已失效）。

3. **Intent execution session 不轮转。**Intent 执行结束、该轮终态进入投影之后，映射仍指向同一个 execution session，后续消息在同一上下文里继续迭代；Chat session 在此过程中始终不变。开新执行会话是**语义决定**而非物理事件：router 的 `dispatch(new)` 新建一条执行链（无 predecessor、不携带旧话题摘要），PM task 终态则 close 映射（§2.1 / §4.1 / §6.2）。

4. **执行会话没有代码级轮转点（v2.7 修订）。**v2.2 曾保留 T1（空闲间隔）与 T2（PM task 终态或发生过 compaction）并取消 T3（`session_config` 与映射 session 自身配置冲突即新建 successor）；现在三态全部删除——配置差异与物理信号都不再具有路由含义。开新执行会话只有三个显式来源：router 的 `dispatch(new)`、PM task 终态 close、用户手动新建；上下文卫生由 compaction 阈值单独负责（§4.1 / §4.5.2 / ADR-099 2026-10-24 修订）。

5. **前端交互 = 语义自动分配 + 显式新建（v2.2）。**默认由 Chat Router 判断复用或新建；用户仍可点会话栏的 `+` 直接创建 execution session——创建请求带上当前 `chat_session_id`，由 Runtime 建立与 `dispatch(new)` 相同的绑定对。这条确定性逃生舱不经过 router：创建后切到 exec 视图，消息直达该会话。自动创建的会话不绑定（ADR-099 D9）。

6. **Agent 页面默认打开 user-agent Chat session。**用户打开某个 Agent 时进入其与它的**唯一** Chat session（每个 (user, agent) 恰好一条，首次打开时幂等创建，v2.1）；群聊、单聊、agent-agent 使用同一套会话模型、消息词汇表与写入口。Agent 的 execution session 不迁移，仍由 Session history 与 exec tab 承载（§5.4.4）。

7. **Chat session 是持久对话记录，在途轮次只是当前窗口的运行期状态。**（v3.1 修订，ADR-106 D9；v3.1.2 重述判据）前端 = 历史渲染自 Chat session 的行 + 引用行的读时投影，在途只剩本面自己发起、尚未在转录里落点的那次发送（claim）；轮次进入终态后答案与终态由投影读出、块接管"进行到哪了"的叙事，等待行随 claim 结算消失——POST 返回不再撤行，因为它只证明投递落地。"最新消息尚未被终态回答"式的反推已废止——它把读不到当成还在跑。

8. **对话所有权采用 C / S 式模型。**发起方拥有并负责结束对话；应对方只有应答权，无发起权；单个对话的 request 次数设服务端可配置上限（默认 6）；需要第三方信息时把问题写回发起方，不由应对方转问。

9. **应对方 turn 的工具集合裁剪。**应对方 turn 的工具集合中移除 `intent_send` 与 `ask_user_question`，靠工具可见性强制而非运行时报错。

10. **repo 是可选字段，且是地址不是路径。**规范化 remote URL；项目有仓库时由服务端注入，发送方不填路径；接收侧只在自身已注册 / 已授权的工作目录集合内读写，不自动 clone、不建 worktree；格式非法与越权访问均为明确拒绝型错误。

11. **每个 chat session 由后端唯一写入口串行写入。**消息按时间顺序追加，`message_id` 幂等去重。

12. **Chat 词表收口为四类（v3.1 修订，ADR-106 D1）。**`user_message` / `agent_message` / `execution_round` / `system_notice`；`conversation_id`、`in_reply_to`、`agent_session_id`、`status` 等 Intent 信息降为字段，不再作为独立消息类型存在（§4.2）。`interaction_request` 从词表退出——需要人决策的东西不再是 chat 里的一行，而是那一轮执行投影内的卡片（ADR-106 D8）。

13. **人类消息是 `user_message`，不是 Intent request。**用户单聊消息若被实现成 request 语义，Runtime 会把该轮判成应对方并摘掉 `ask_user_question` / `intent_send`，单聊即失效。传输可复用 Intent 投递管线，入站类型必须是人类输入、本轮角色是发起方（§5.4.4 / §5.4.5）。

14. **本轮对 chat 的义务与它的角色无关。**（v3.1 修订，ADR-106 D3 / D12）只要入站消息携带 `source_chat_session_id`，本轮就必须在**轮次边界**落一条 `execution_round` 引用行，并在结束时把终态交给投影——终态不再是 chat 的 `agent_message` 行。早期版本只在应对方 turn 结束时写回，会使单聊会话永远为空（§5.5.1）。

15. **决策卡不进 chat 转录，只有寻址问题进（v3.1 修订，取代 v2.4 的卡片统一类型，ADR-106 D6 / D8 / D10）。**ask question、工具审批、暂停续跑、重试这些"要人做决定"的东西随轮次投影呈现，答复直接投回执行会话，chat 不再持有一张可点的卡。留在 chat 里的只有两类寻址问题 `session_choice` / `confirm_continue`，它们走 `system_notice`；答卡 = 把人类那句话带地址重投递一次（原行 `message_id` 做幂等），因此转录里始终只有一条人类消息而不是两句。可答性按"谁发起这一轮"判定：旁观者看得到问题，看不到按钮。

16. **Chat session 是 Agent 页固定 tab，与 execution session tab 共存。**打开 Agent 必然落在 Chat tab 上，它不可关闭、不可删除；execution session 是同一标签栏里的普通 tab，可随意开关，关掉最后一个也只是回到 Chat tab。`+`、Session history 都是 exec tab 的入口——后台不等于隐藏（§5.4.4 / §6.1，v2.1 修订）。

17. **chat 是对话历史，轮次进度与执行内容都不进历史（v3.1 修订，ADR-106 D1/D3）。**落库的是人类的话、Router 自己的回答、通道回执，再加一条**空正文的引用行** `execution_round`——它只报坐标（`round:{instance}|{session}|{round}`），正文与卡片都从执行侧投影读。"已派发 / 已开轮"这类中间状态**既不落 Chat session、也不在 chat 侧读时投影**；发送→回复之间的空窗只由两件事派生：本设备未结算的发送 claim（v3.1.2，取代"本地在途的 POST"）与块自己的 `pending` 读（ADR-106 D9）。任何状态落库都会在一轮内产生几十次改写，破坏 §4.6 的 append-only 单写入口。

18. **一轮是否结束不由 chat 的行决定，也不由 `Idle` 决定（v3.1 修订，ADR-106 D5/D9）。**判据是投影的 `terminal`：引用行只说明"这里有轮次要读"，块读回来的 `pending` / `ok` / `not_found` / `unreachable` 才说明它到哪一步。呈现层不得把"读不到"画成"还在跑"——不可达带重试，是一次故障，不是一个进行中的轮次；旧转录里的行也不能被反推成等待证据。

19. **群聊触发的 execution session 对该群聊全体参与者可读。**判据由 routing key `chat:{chat_session_id}` 派生，不建独立 ACL；**不得复用 `SessionVisibility::Public`**，它是平台级可见，会让非群成员读到群执行历史。agent-agent 会话的执行记录不对界面外的人类开放（§5.5.5）。

20. **路由结论不再是 durable 反应行（v3.1 修订，废止 v2.5 的 `turn_routed`，ADR-106 D14）。**v2.5 恢复 `turn_routed` 是为了补"人类消息之后到回复之前没有任何 agent 的话"这个空窗，但它的代价是 chat 持有一份执行侧事实的拷贝：写失败就是谎，写成功就是镜像，而每个写入口都多一宗丢消息的责任。现在同一件事由引用行的坐标 + 块的投影回答（块头就是"哪个任务、到什么状态"，且永远与执行侧一致）；头像不变量搬到块头上（一个执行会话一个块，块挂一次头像 + 名字 + 跳转，块内一轮一行）。旧转录里的 `turn_dispatched` / `turn_started` / `turn_routed` / `router_fallback` / `interaction_resolved` 行不迁移、不删除，只在渲染期隐藏（§5.5.5）。

21. **不迁移历史。**既有 execution session 留在 Session history，新会话才建 Chat session；好处是该步完全可回滚。（v2.1：两个列表并存的代价表述已失效——Agent 页不再有 chat 列表，并存的是固定 Chat tab 与 exec tab。）

22. **Chat 层引入 LLM 语义路由（v2.2，ADR-099）。**每条人类消息（1:1、群聊 @Agent）先经过目标 Agent 的 Chat Router：一次无状态 LLM turn，工具集只有 `dispatch`，产出"回答 / 转交已有执行会话 / 新建执行会话"三种决策。PM 派单、agent-agent 与卡片回答不经过 router。

23. **Router 不拥有会话。**不建隐藏常驻 session，上下文每轮从 chat 历史 + 候选清单 + draft config 重建；chat 历史是唯一权威，避免双历史分叉与额外的会话生命周期维护。

24. **候选集按 chat 绑定与 owner 硬过滤。**1:1 只列当前 chat 映射、该 chat 的历史 predecessor、以及该用户 owner 且未绑定 chat 的会话；群聊只列 `meta.chat_session_id` 指向本群的会话（含 PM 任务会话）。候选上限 10，按最近活动倒序，只暴露 title / 状态 / 活动时间 / model / workspace / 来源，不读执行正文（ADR-096）。

25. **群聊 @ 只决定 agent，不决定执行会话（v2.2）。**同一群里不同话题可并行到不同执行会话；群聊 router 不创建决策卡片，澄清只用文本，且不得复述私聊内容或其他 chat 的会话标题（§5.3.1）。

26. **PM 派单保持确定性映射。**`conversation_id = task:{project}:{task}` 是任务身份与生命周期边界，不经 router；task 会话创建时绑定项目群聊（`chat_session_id` + Members 可见）从而进入该群候选，`close_intent_context` 记入终态清单并在候选中排除，避免终态任务被"复活"（§5.4.2）。

27. **Chat prompt 与 execute system prompt 分离（v2.2）。**chat prompt = 内置基座 + manifest 人格回退 + 1:1 / 群聊场景段；三者可被 `chat.md` **整段替换**（群聊会话级 `chat-sessions/{id}/chat.md` 优先于包级 `prompts/chat.md`，缺省时用内置默认）。`# Safety` 段是一段不可裁剪的尾块，Runtime 无论如何都会重新追加；群聊的 `chat.md` 仅群主或管理员可写（ADR-098；设计 29 §3.2、ADR-099 D6）。不加载 `system.md` / `constraints.md` / 其他 prompts / skills / workspace prompt file / retrieved memory。包级 `chat.md` 纳入 ADR-063 override 清单，因而从 execute prompt 拼接中排除；长度上限 16KB（设计 29、ADR-099 D6）。

28. **Router 失败降级，可用性不退步（v2.5 修订降级落点）。**超时（默认 8 s）、provider 报错或连续两次非法工具参数时降级；**chat 来源的人类消息降级时新建执行会话**（`chat_dispatch(Target::New)`，绑定同一 chat、映射重指向），不再复用 `routing_key` 映射——降级意味着没有语义判断，映射也不是语义判断，把可能开新话题的消息猜塞进不相关的历史会话比新建更糟；`PassThrough`（PM / agent-agent / response / kill switch 关闭态）的确定性复用不变，那些链路上 `conversation_id` / `msg:` 键就是任务身份（ADR-099 D8 修订）。**不再为降级写 `system_notice(router_fallback)`**（旧行渲染期隐藏），v3.1 起降级新建也不落任何反应行——路由结论由引用行的块头呈现，与正常派发走同一条路径（决策 20）；消息已落库，不因路由失败丢失（§5.7）。降级不是唯一需要约束的失败面：router 链路上的每个 Gateway HTTP 都有界（历史读 `chat_router.timeout_ms`，写回 15 s，附件拉取 60 s），且 router turn 跑在该 chat 的 drain task 上而不是 MQTT 入站循环上，一个挂起的 Gateway 或慢模型只能放慢这一条 chat 的分配，不能停住该 Agent 的接收面（含 `Stop`）。`chat_router.enabled=false` 时不排队，Chat 消息内联走确定性路由（ADR-099 D8）。

29. **执行视图的输入只写执行会话自己（v3.0，废止 v2.6 / v2.9 的镜像与定向直投）。**绑定 chat 的 execution session，其 composer 的发送走 `POST /api/agents/{id}/sessions/{sid}/messages`，**不**在 chat 里落 `user_message` 行：执行面那句话只存在于执行会话自己的 JSONL。不变量由构造保证——**新开执行会话是 Chat 会话的权限**（Router 的 `dispatch(new)`，或人自己在 chat 面点 `+`），另一个来源是 agent 调 `intent_send`；除此以外没有任何一条用户消息能创建执行会话。v2.6 曾把执行面的消息镜像成 chat 的 `user_message`（补"只回写尾、不落头"的半轮对话），而 chat 里的 `user_message` 恰是 Router 的输入，于是不得不发明 pin（`for_execution_session`）叫 Router 闭嘴，再为 pin 的每一次落空补兜底：`Closed` 目标经开机状态机重开（v2.9）、兑现不了时走确定性路由、`pinned_agent` 参与者校验、`command` / `attached_items` 上限、`PendingTurn::pinned_session_id` 的 per-chat 投递队列。v3.0 取消需求本身，上述机制整体删除（ADR-100 Rev. 8）。回合对 chat 的义务只剩**一条引用行**（v3.1 修订，ADR-106 D3 / D8 / D12）：轮次边界落一条空正文 `execution_round`；standalone 形状仍成立（`trigger_message_id` 为空，即这一轮不由某条人类消息触发，D9 / D13 / Rev. 6 已定形），但**卡片与终态 echo 不再落 chat**——卡片进那一轮的读时投影，答复直投执行会话；谁能应答卡片由 Gateway 的 `x-user-id` scope 给出（`answerable_by`），那是关于调用者的事实，不是关于绑定的提示。`attached_items` 与 `meta.exec_attachments` 投影保留：它们服务 chat 面 composer 自己的 workspace 引用。代价：chat 转录里看不见执行面那句提问，那一轮读作"卡片 + 答案"。发布：Desktop / Mobile / Gateway / Runtime 同批——旧前端仍带的 `for_execution_session` 会被 prost 当未知字段丢弃，那次发送于是退化为一条普通 chat 消息并进入 Router（可能新建会话），不提供平滑层。

### 3.5 现有实现缺陷

按严重度排列，是 §10 开放问题的输入。

| 级别 | 缺陷 | 位置 |
|---|---|---|
| **P0** | `SessionKind::UserAgent` 被创建入口的参与者校验拒绝，单聊 Chat session 完全不可达 | [chat_sessions_api.rs](../../../core/acowork-user/src/http/chat_sessions_api.rs) |
| **P0** | Gateway 客户端把 pair 会话 kind 写死为 `agent_agent`（**存储层已支持 `kind` 参数，非阻塞**） | [chat_client.rs:68](../../../core/acowork-gateway/src/intent/chat_client.rs#L68) |
| **P0** | Agent 页面 Chat history 以 Agent id 作 participant 查询，非管理员必 403（**鉴权规则本身正确，病根在前端传参；不得靠放宽鉴权修**。v2.1：Chat history 按钮撤销，缺陷随入口一并消失） | ChatHistoryDropdown（已删除） |
| **P1** | 回复写回只在应对方 turn 结束时发生，单聊（发起方角色）永不写回，会话恒空 | §5.5.1 |
| **P1** | `user_message` 不要求 `message_id`，而 `in_reply_to` 必须指向它，卡片回答无处可指 | §4.2 |
| **P1** | participants 无身份类型校验，Agent 可冒充人类写 `user_message` | §8.1 |
| **P1** | 代码仍是旧三分类 kind 与旧 status 枚举，未含 `waiting` | [chat_sessions.rs](../../../core/acowork-user/src/chat_sessions.rs) |
| **P2** | `list_for` 全目录扫描，缺 per-participant index | §4.6 |
| **P2** | Agent 重装换 instance id 后旧会话成孤儿 | §10.19 |

> 本表已按代码复核修正三处。逐项 gap 对齐、可交付切片与验收标准见 [`docs/plan/zh/intent-chat-session-unification-plan.md`](../../plan/zh/intent-chat-session-unification-plan.md)。

## 4. 数据模型

### 4.1 Agent execution session

这是现有 Agent session 模型，保存单个 Agent 执行任务所需的上下文和运行历史，包括其普通用户输入、模型输出、思考过程和工具调用等。它继续由对应 Agent Runtime 在 Agent home 中按现有格式管理。

执行 session 可以由用户消息或 Intent 触发。无论触发源是什么，接收 Agent 都在自己的 execution session 中工作。Intent 的发起方、回复地址及聊天消息历史不会使发送方 Agent 的执行上下文与接收方共享。

**兼容原则：保留现有 Agent session 的存储格式和模型。**新聊天模型通过关联 ID 引用 Agent session，不把 chat session 变成新的 Agent 执行记录格式。

#### Intent execution session 的上下文压缩

Intent 驱动的 execution session 沿用现有 Agent session 上下文压缩机制，不为 Intent 单独发明 token compaction 方案。首版沿用当前阈值：上下文使用率 70% 仅记录告警，80% 触发 LLM compaction，95% 触发 `emergency_trim` 安全网；计算分母使用该 session 的有效 context window（遵循现有 per-session 覆盖规则）。压缩只控制送入模型的上下文，不会缩小 JSONL 完整历史文件；执行会话不因压缩而更换——它是长命的对话对象（见下）。

**执行会话不轮转（v2.7 修订）。**逻辑映射（例如群聊与 Agent 的对应关系）指向一个**长命**的物理 execution session：Intent 执行结束、该轮终态交给来源 Chat 的投影之后，映射仍指向同一个 session，后续消息继续在同一上下文里追加与迭代。自 v2.1 起，用户在 Agent 页面的单聊也走 `chat:{chat_session_id}` 映射，同样适用本策略；未绑定 Chat session 的会话（自动补建与历史遗留）保持常规生命周期。历史 session 的完整记录一直留在 Agent home，供现有 Session history 查阅；不得把执行细节写进 Chat session。

v2.2 及之前的三态物理轮转（T1 空闲间隔 / T2 任务终态或本次执行发生过 compaction / T3 配置冲突）连同 successor、predecessor、handoff 与 `intent_idle_rotate_hours` / `intent_handoff_max_tokens` 配置面已全部删除（ADR-099 2026-10-24 修订）：

- T1/T2 是"没有 LLM 做路由判断"时代用物理信号近似"任务该结束了"的机制。语义决策上提到 Chat Router 之后，闲置时长与压缩事件不再承担任何路由含义；
- T2 的 handoff 摘要（向 successor 注入旧话题的蒸馏摘要）与"新任务不被旧摘要污染"的目标直接冲突；
- 每一轮都重新读全量 JSONL（ADR-032），**发生过压缩的会话同样可以继续接 dispatch、继续迭代**——压缩是它的常规生命周期事件，不是更换窗口的理由。一次执行内多次压缩（小 context window 或长程任务）是正常行为，中途也不切换 session。

收敛后的规则只有一条：**开新执行会话必须是显式决定**，来源只有三个——

| 来源 | 判据 | 定义处 |
|---|---|---|
| router `dispatch(target="new")` | 模型判定这条消息开启新工作 | §3.4 决策 22–23 / ADR-099 D3 |
| PM task 进入终态 | `Done` / `Rejected` / `Cancelled` → close 该任务的执行上下文：关闭 session、删除映射、记入终态清单（候选排除） | §6.2 |
| 用户经会话栏 `+` 新建 | 显式点击，不经 router；绑定与 `dispatch(new)` 相同的会话对 | ADR-099 D9 |

上下文卫生由本节的 compaction 阈值单独负责，文件规模由既有 JSONL 硬上限负责。**已废止**：T1/T2/T3 触发器表、successor/handoff 生成与注入、`rotation_handoff` 标记的新增写入。

**`dispatch(new)` 是新建一条执行链，不是更换上下文。**它没有 predecessor、不携带任何旧话题摘要：新会话只有所属 Chat session 的对话记录作上下文。这条区分正是"新任务不被旧摘要污染"的保证——轮转删除后，携带旧摘要的路径已不存在。

不设执行次数上限的理由：次数不是可度量的物理指标，无法判断多大合理；而"上下文被不相干 Intent 污染"这一问题已由 compaction 阈值直接覆盖——不相干内容积累到一定程度必然触发压缩。文件与磁盘规模由既有 JSONL 硬上限保护。两个边界因此闭合，不存在无限膨胀路径。

PM task 终态是一个**业务边界**而非上下文切换技巧：它关闭该任务的执行上下文（关闭 session、删除 `conv:task:` 映射、记入终态清单），后续新任务使用新映射。1:1 与群聊的 `chat:` 映射描述的是对话、不是已完成的任务，因此永不因时长或压缩而关闭——只有 router 判定"开启新工作"时才另建一条链。

#### 新建执行会话的上下文

`dispatch(target="new")` 与用户手动新建的执行链不携带任何旧话题摘要：新会话的上下文只有所属 Chat session 的对话记录（按设计 29 的三段式编排注入）与 Agent 自身的 system prompt / skills。

v2.2 及之前的「Handoff context 分级」表（T1 空闲间隔只带未决事项与关键决定、上限 500 tokens；T2 compaction 判据带当前目标 / 关键决定 / 未决事项与引用 ID、目标 1,500 tokens、可配置 500–4,000）随轮转一并废止：`intent_handoff_max_tokens` 配置项、Agent 设置页的「Intent 会话轮转」卡片与 `handoff_note` 通道均已删除。历史 JSONL 里带 `rotation_handoff` 标记的行不迁移、不清理，仍按"模型的上下文而非用户可见消息"从聊天渲染中过滤。

#### 存储边界

保留既有 JSONL 硬上限 100,000 行或 50 MiB（先到者）作为绝对安全上限，不另设主动轮转的 JSONL 阈值：执行会话是长命的，上下文体积由 compaction 控制送入模型的部分，文件规模由该硬上限兜底，正常情况下不会接近。若异常大的单次执行在安全检查点前逼近硬上限，Runtime 必须拒绝继续无界追加，在可恢复检查点暂停并保存状态；无法安全暂停时明确失败并保留已写入历史，不得静默丢弃或越过硬上限。（v2.7：删除"保存状态后轮转"这一句——后继会话机制已不存在，超限的处理是停止追加而不是更换窗口。）

#### 前端可配置边界（Agent 设置面板的 Session 设置卡片）

**本卡片不含 Intent 轮转配置项（v2.7 修订）。**v2.2 曾开放两项——空闲间隔轮转阈值（默认 24 小时，允许 1–72 小时，`0` 表示关闭 T1）与 Handoff context 目标上限（默认 1,500 tokens，允许 500–4,000）——它们随轮转机制一并删除：`AgentConfig.intent_idle_rotate_hours` / `intent_handoff_max_tokens`、对应的 `ConfigField` 变体与 `PUT /config` 请求体字段、部署级覆盖环境变量 `ACOWORK_INTENT_IDLE_ROTATE_HOURS`、前端字段与五份 locale 文案均不再存在。卡片其余项不变。

以下项**不开放**给普通用户：Compaction 70% / 80% / 95% 阈值（沿用现有 Runtime 行为与配置能力，不因 Intent 重复造配置）、JSONL 硬上限 100,000 行 / 50 MiB（绝对安全上限；实现期可由部署级高级配置调整，但必须保留硬上限语义）。

per-session 的 model / provider / reasoning / workspace 建议由路由时的 `session_config` draft 携带（§4.5.2）：只在新建会话时作为 seed 应用，路由到已映射会话时**不应用**——初值只服务于被创建的那条会话（同 ADR-099「修订（2026-10-28）」）；配置差异不再是任何新建或更换窗口的触发条件。

参数变更需要鉴权并记录审计；更改不重写已有历史。运行中的一次执行使用开始时解析出的配置快照。未提供配置时使用默认值。普通用户只能调整授权范围内的 Agent 级参数，不能修改他人 Agent home 中的 session 文件。

#### 并发与失败恢复

映射的写入是单点串行：`route_intent_session` / `chat_dispatch` 在 `SessionManager` 锁内解析与更新映射（候选清单的目录扫描在锁外完成），同一键的并发 Intent 按入站队列串行处理，不存在两个执行者同时改写一个映射的路径。已废止的 successor 崩溃恢复（预分配 id + `pending_successor` + `rekey`）与轮转一同删除：不再有需要重放的"中途换窗口"中间态。

队列不变量（ADR-099 D2）仍成立：一条 chat 恰有一个 drain task，FIFO，drainer 收尾时到达的消息不滞留；降级（`PassThrough` / `Fallback`）在该 drain task 上就地收尾，不重新入队、不丢消息。

### 4.2 Chat session 消息词汇表

Chat session 是用户聊天与 Agent 间消息的持久化会话，由 `acowork-user` 聊天域统一拥有和管理。其消息类型是固定的通信类型，不存 Agent 私有运行轨迹。**词汇表按"谁在说话、这句话要什么"划分，不按传输机制划分**：

| kind | 写入方 | 语义 | 必需约束 |
|---|---|---|---|
| `user_message` | 人类参与者 | 人类发言；带 `mentions` 即 @Agent；`in_reply_to` 指向一条 `execution_round` 引用行即寻址阶梯的**级别 1**（确定性续投该执行会话的下一轮，ADR-106 D6）；回答寻址卡也是本 kind 的一次重投递（`message_id` 取原行 id 做幂等）；在执行视图里敲的消息**不再**走本 kind（v3.0，ADR-100 Rev. 8：执行面消息不进 chat 通道）。`meta.exec_attachments` 服务的是 chat 面 composer 自己的 workspace 引用，带附件**显示投影**（`acowork_core::protocol::AttachmentLabel`：显示名 + ADR-046 臂名 + 芯片装饰，工作区臂另带 `absPath` 作为客户端的点击坐标——ADR-100 D12 Rev. 9）、不带 documentId、上传臂不带路径，与执行面同一枚 `AttachmentCapsule`、同一张 glyph 表（臂名只驱动图标），数据里有什么就打什么：带路径的工作区臂可点开文件，其余仍是装饰胶囊 | `body` / `attachments` / `meta.exec_attachments` 至少一个非空；`message_id` 必需 |
| `agent_message` | Agent 参与者 | **只剩 Chat Router 的自答**：router 不回人类却自己说了一句，它不是任何执行轮的投影，所以它是 chat 原生行。执行回合的回答与 agent↔agent 的应答**都不再**是本 kind（v3.1：改由 `execution_round` + 投影承载，ADR-106 D3 / D12） | `message_id` 必需；应答时 `in_reply_to` 必需 |
| `execution_round` | Runtime（全仓唯一 chat 写入口：`AgentLoop::run` 的轮次边界） | 一条**引用行**：不携带内容，只报"这一轮执行存在"，坐标 `round:{instance}|{session}|{round}` 即自然键（幂等），呈现时读执行侧投影得到答案、卡片与状态 | `message_id` 必需且等于自然键；`body` 恒空；meta 携 `agent_instance_id` / `agent_session_id` / `round_id` / `in_reply_to`（触发它的行），可带 `task_id` / `round_name`（仅呈现，ADR-106 D13；rename 之前写下的行为 `assignment_brief`） |
| `system_notice` | 系统 | 通道回执，按 `notice_type` 分流：`budget_exhausted`、`truncated`、`unavailable`（ADR-106 D12：预算收据等改为通知，Gateway 不再代写对话行）；`delivery_failed` 自 2026-11-06 起**无写者**——投递失败改为行上的内存态实时告警 `delivery.alert`（ADR-105 D5 修订），旧行仅作兼容读取并在渲染期隐藏，以及两类寻址问题 `session_choice`（级别 3 多候选）/ `confirm_continue`（级别 5 提议），meta 携 `options[]` + `allow_new_task`，幂等键 `{notice_type}:{message_id}`；旧的 `turn_dispatched` / `turn_started` / `turn_routed` / `router_fallback` / `interaction_resolved` 仅作兼容读取，渲染期隐藏 | `message_id` 必需 |

**Intent 不再是消息类型。**`intent_request` / `intent_response` 作为 kind 取消；v3.1 前它们的语义降级为 chat 行上的**字段**（`conversation_id`、`in_reply_to`、`agent_session_id`、`repo`、`status`），v3.1 起 ADR-106 D12 把它们从 chat 里整体拿走了：request / response 的方向语义活在**投递信封**与 Gateway mailbox（预算计数在那儿算），不再需要一行 chat 副本。

这样收口的理由：Agent 对人类说话、对群说话、对另一个 Agent 说话，在聊天域里是同一件事——"Agent 发了一条消息"。若按传输机制分类型，同一句话会因为发起场景不同而落成不同的行，前端必须写四套渲染分支，且 `user_message` 与承载相同正文的 `intent_request` 会在同一 session 里重复落两行。

具体 wire/schema 类型在协议实现阶段定义。Agent 的思考、隐藏推理、工具调用、工具结果和执行中间事件不得作为 chat message 写入。必要的投递或展示状态是消息元数据 / 独立事件，不扩充为任意 Agent 执行事件。

#### 4.2.1 附件

`user_message` 和 `agent_message` 支持附件，用于 Add to Chat、图片、文档和 workspace 引用。附件沿用 user-user 聊天已确立的规则（ADR-076 §决策 9、ADR-092）：

- blob 与元数据 sidecar 分文件存储，**blob 先写**：两次写之间崩溃留下无人引用的孤儿 blob，而不是指向空的元数据；
- 磁盘上的名字是服务端生成的不透明 id，**不是**用户上传的文件名——拼接用户字符串进路径会同时招来穿越和碰撞；
- 上限按 mime 分档（图片 / 文档），文件名长度只是展示约束，永不进路径；
- 附件归属 Chat session 目录，不归属单个参与者：一个文件由该会话的参与者共同寻址，再建一层命名空间只增加查找、不增加边界。

Agent 侧收到的仍是解析后的引用，Runtime 不读 Chat session 目录；附件的读取路径与消息正文一致，经唯一写入口所在的受信任服务。投递载体：**MQTT Intent 信封只携带附件 id，文件字节永不进 MQTT 消息**——Runtime 在回合开始时经 node 鉴权的 HTTP（Gateway 转发 user service 的内部流式端点）逐个拉取字节并物化进 agent home 的 `files/` 目录；user service 校验 durable `user_message` 行确实 addressed 给该 target 且 pin 了该 id，行即授权。单个拉取失败降级为"该附件本轮不可见"，回合照常运行。

**元数据与字节分道。** 附件元数据（filename / mime / size，由 user service 在落库时从会话 `files/` 目录重读，不采信客户端）随投递 params 的 `chat_attachments` 键送达，供 Chat Router 看清本轮带的是哪些文件（design 29 §5.1）；信封字段 `chat_attachment_ids` 仍然只承载 id，字节仍然一次都不上 MQTT。两者是不同通道：id 走信封给执行侧拉字节，元数据走 params 给路由判断。params 里缺这个键（旧 Gateway）或形状不符时，Runtime 一律降级为"本轮没有附件"——代价是一轮判断质量，不是整条消息。

本轮还可以携带**工作区引用**，走 params 的第三个键 `attached_items`（ADR-046 union）：它不是上传件，Gateway 读不到 node-local 路径，所以路由侧只取 `AttachmentLabel` 的显示名与行区间，渲染成 `[attached refs]` 一行。引用与上传件是两份列表，而 Chat Router 一度只读其中一份——"只发引用不发消息"的回合因此被判成空轮、在 chat 里回答"你没附任何东西"，而人的屏幕上芯片就在那儿。根因不是某个分支写错，是**同一件事没有唯一的判据**：现在 `turn_attachments` 与 `turn_refs` 是仅有的两个读者，"这一轮到底有没有内容"由 `context::turn_has_content` 一处回答，路由闸门与 Block C 渲染同问一处。

#### 4.2.2 引用行与卡片（v3.1 修订，ADR-106 D3 / D8）

Chat session 里只有一类指向 execution session 内事件的 durable 行：

- **引用行** `execution_round`：空正文、自然键 `round:{instance}|{session}|{round}`、**前置型**（先落行再跑轮）。它不复制任何执行内容，只报"这里有轮次要读"；一轮的答案、等待中的卡片、状态词全部由读时投影供给（§5.5.5）。

旧的**决策卡片** `interaction_request` 不再是 durable 行：卡片在执行侧 `track` 时以**非终态 `card` 快照**进入块的投影，答复直接投回执行会话——chat 不持有一张可点的卡，也就没有一个会写失败的答句副本（ADR-106 D8）。轮次进度**不是** durable 行：它在当前窗口由前端派生（§5.5.5）。

**这些行持久化的是指针与事实，不是状态。**引用行落库的是"这一轮存在、由哪条消息触发"这一交流事实；轮次的运行中 / 思考中 / 等工具 / 已暂停这些**实时状态一律不落 Chat session、也不在 chat 侧镜像**，只在投影里读、在块里看，从块头"查看执行"跳转（§5.5.5）。状态落库会在一次 turn 内产生几十次改写，要么破坏 §4.6 的 append-only 单写入口，要么让聊天历史被执行细节淹没。

#### 4.2.3 统一类型、不同参与者

所有聊天采用同一 Chat session 和消息日志模型，通过会话类型及参与者表达关系，至少覆盖：

| Chat session 类型 | 参与者 / 用途 | 消息内容 |
|---|---|---|
| User-user | 用户创建的一对一聊天 | `user_message`（含附件） |
| User group | 用户创建的通用群聊 | `user_message`；仅当显式 @Agent 时另有该轮的 `execution_round` 引用行（答案与卡片走投影，不落 chat 行） |
| Project group | 项目创建时生成的特殊群聊 | `user_message`、`agent_message`、`execution_round`、`system_notice` |
| **User-Agent chat** | **用户与单个 Agent 的一对一聊天——Agent 页面的默认对话面**（§5.4.4） | `user_message`、`agent_message`、`execution_round`、`system_notice` |
| Agent-Agent chat | 两个 Agent 的一对一聊天 | `execution_round`（应答方各轮）与 `system_notice`；v3.1 起 Intent 往来不落本会话行（ADR-106 D12）；无人类参与者，故不产生卡片（§5.5.4） |

上表是逻辑类型，**物理 schema 完全相同**：同一目录布局、同一 `session.json`、同一 `messages.jsonl`、同一唯一写入口，`kind` 字段是唯一区分。新参与者类型可以扩展同一聊天模型，但不得改变"聊天只记录消息，执行细节留在 Agent session"的边界。

两类会话的差别只在**行为**，不在结构：

| 行为 | 一对一（user-agent / agent-agent / user-user） | 群聊 |
|---|---|---|
| 触发 | 每条 `user_message` 都投递给 Agent 参与者 | 显式 @Agent 时投给被 @ 者；未 @ 时投给群的 `default_responder`（未配置则仅归档，ADR-105 / §5.3.1） |
| 目标解析 | 会话内唯一的 Agent 参与者，无需 @ | 客户端把 @ 解析成明确的 instance id 列表 |
| 卡片可答者 | 会话内的人类参与者 | 触发该轮的那条人类消息的作者（§5.5.4） |

`user_agent` 会话的 `session_id` 是服务端生成的 UUID，**不从参与者 id 拼接**。理由：participant 携带 agent instance id（ADR-073），重装 Agent 会产生新 instance；若会话身份由参与者推导，重装即丢历史。会话身份必须独立于成员身份。

### 4.3 存储所有权

`acowork-user` 是用户聊天和 Chat session 的权威存储与写入方。项目群聊虽然由 PM 项目创建和关联，但聊天记录归 `acowork-user` 聊天域维护，而不是存放在 PM task 文件或任一 Agent home 中。

远程或本地 Runtime、PM 和 Desktop 都通过受信任的服务接口操作聊天记录；Runtime 不直接访问或改写其它 Agent home 中的 session 文件。PM 持有 `project_id` / `task_id` 等业务关联，由服务端校验后映射到对应 chat session。

### 4.4 项目群聊生命周期

- 每个项目创建时必须创建一个关联项目群聊，且一个项目只能有一个此类群聊。
- **项目存活期间，项目群聊不能独立删除**：`ensure_group` 会在下次触碰时把它重新建出来，删掉只会白丢历史（与"默认 pair 会话不可删"同一个理由）。
- **项目被删除（PM 归档到 `.trash`）之后，群主或管理员可以删除该群聊**。删除**前置条件**由 PM 的事实表达：PM 在 `DELETE /projects/{pid}` 成功后经 `POST /api/intents/project-group/deleted` 在会话行上盖 `project_deleted_at`，chat 域据此放行（它看不到 PM 的项目树，也不该看）。
- 删除是**硬删**（会话目录 + 消息日志 + 索引），与既有 `DELETE /api/chat-sessions/{id}` 走同一条路径；治理规则与群聊其它写操作一致（ADR-098：群主或管理员，参与者身份仍是入口权限，非成员得到隐藏 404）。**PM 永不替人类删群聊**——归档项目不销毁聊天记录，删不删由人决定。
- `project_id` 与项目群聊稳定关联；PM 派发和 Agent 回复使用该群聊作为来源 / 回复地址。

### 4.5 Intent 字段模型

**Intent 是消息上的一组字段，不是一种消息类型**（§4.2）。它是投递信封与 Gateway mailbox 上的一组寻址、处理和回复上下文；人类发起时，这组字段可读的那部分落在 `user_message` 行上（v3.1：agent↔agent 的 Intent 不再落任何 chat 行，ADR-106 D12）；接收方看到的仍然是"一条发给我的消息"。下表描述字段语义，不代表存在独立的 Intent 记录：

| 字段语义 | 必需性 | 说明 |
|---|---|---|
| `message_id` | **所有 kind 必需** | 唯一消息标识；用于关联、重复去重、回复指向和诊断。由唯一写入口分配，客户端可提交但服务端可覆写（§4.6）。人类 `user_message` 也必须有 id，否则 Agent 的回复无处可指 |
| `source` | 必需 | 发起主体及类型；身份由可信服务确认，不能仅信任客户端自报 |
| `source_chat_session_id` | 必需 | 来源 Chat session，也是回复地址 |
| `source_agent_session_id` | Agent 发起时必需 | Agent 发起本次 Intent 的 execution session ID。用户 / PM 发起时为空 |
| `target_agent_instance_id` | 必需 | 精确目标 Agent Runtime 实例，遵守 instance ID 寻址约定 |
| `body` / `attachments` | 至少一个非空 | 给接收 Agent 的消息内容与附件（§4.2.1） |
| `mentions` | 群聊 @ 时 | 被 @ 的目标 instance id 列表。一条 `user_message` 只落一行，多个目标写在同一行的 `mentions` 里（§5.3.1） |
| `auto_routed_to` | 群聊未 @ 且该群配置了默认接收方时 | 自动接走这条消息的 agent instance id。与 `mentions` 分开：前者是人打的 @，渲染成 chip；后者是路由决策，只是"这句话去了哪里"的记录，**不再是等待行的判据**（v3.1，ADR-106 D9 / D14 废止 ADR-105 D4） |
| `repo` | 可选 | 本次工作涉及的代码仓库地址（规范化 remote URL）。项目有仓库时由系统在派发时自动注入；发送方不指定目标 Agent 的具体工作目录 |
| `assignment_brief` | 仅 `pm_task` | PM 派发的唤醒 playbook（claim → 工作 → submit 契约）。**高信任信封字段**：只有系统生产者（task-dispatch 管线）可以填，Runtime 仅当轮次来源为 `pm_task` 时采纳并注入执行会话的系统块（Block A），不进渲染后的用户消息、不落 Chat 表面、不随会话历史持久化；对等 `intent_send` / 卡片应答等 agent 来源的 params 携带该键时，Gateway 在出口剥离、Runtime 在提取与消费两处各设一道来源门控（ADR-102 信任边界：高信任信封字段只认系统生产者） |
| `session_config` | 可选 | model / provider / reasoning-effort / 工作目录的**初值建议**；新建 execution session 时作为 seed 应用（路由到已映射会话时一律不应用，见 §4.5.2 与 ADR-099「修订（2026-10-28）」），未提供的字段用目标 Agent 默认值。**不含 skills**——技能由模型自动加载，非 session 级初值，chat-message 通道也没有逐条指定技能的入口 |
| `conversation_id` | Agent 发起时必需 | 一次对话的标识，由发起方在首次 request 时生成，本次对话的后续 request / response 复用。系统据此统计 request 次数（§5.4.5）。**人类发起的轮次留空**，因此人类消息天然不计入对话预算 |
| `in_reply_to` | 应答时必需 | 被应答消息的 `message_id`；**由系统生成，不是模型可填参数**——它的存在即表示本条是应答（§5.4.5 R2）。人类回答卡片时同样使用此字段（§5.5.4） |
| `target_agent_session_id` | 路由后关联 | 实际处理本条消息的目标 Agent execution session ID。前端"查看执行过程"的下钻句柄 |
| `status` | 应答 / 回执 | `completed` / `failed` / `aborted`（§5.5.1） |

字段名和 wire schema 在协议实现阶段确定；本表约定语义，不构成最终序列化合同。

**投递信封另带 `chat_kind`（`user_agent` / `user_group` / `project_group`），由 Gateway 依来源 Chat session 的 `kind` 填充。**它不进消息体、不落 Chat session，只用于让 Runtime 区分 1:1 与群聊路由：卡片失效后按普通消息 fall through 时 `in_reply_to` 非空，靠它才能判断这其实是群聊而不是单聊（§5.1、ADR-099）。

#### 4.5.1 Repo 字段与接收方边界

`repo` 是可选字段，值为本次工作涉及的代码仓库地址，采用规范化 remote URL（去协议与凭证、host 小写、去 `.git` 尾）。它的语义是"这个任务关于哪个仓库"，**不是"去哪个文件夹"**。

> 术语约定：本文中 `workspace` 一词只指 Agent 侧已注册的工作目录（`WorkspaceDir`、`allowed_dirs()`、前端 workspace 选择器）。Intent 协议不使用该名称：一个仓库可对应多个 workspace（多 clone / 多 worktree），一个 workspace 也可能不是代码仓库。

规则：

1. 项目可以没有代码仓库（非软件项目）。此时 Intent 不带 `repo`，Agent 在自己的默认工作目录处理消息——这是正常目标，不算静默回退。
2. 项目有仓库时，PM 派发的 Intent 与该项目群聊 @Agent 生成的 Intent **由系统自动注入仓库地址**。发送方（人或模型）不需要知道目标 Agent 的目录结构，也不填写具体路径。
3. 收到 `repo` 后如何使用，由接收 Agent 的模型自行理解与决策（在哪些已授权目录里工作、要不要切分支）。协议层不提供工作区选择算法，也不维护"仓库 → workspace"绑定表：这类判断的复杂度无法由单个 Intent 参数覆盖。

模型的自由度到此为止，以下是硬约束：

- Agent 只能在自己已注册 / 已授权的工作目录集合内读写；
- 发送方的本地路径不得作为跨 Node 的寻址依据，也不得出现在 Intent 中；
- 不得因为收到一个仓库地址就自动 clone 或创建 worktree——磁盘失控与供应链风险，且没有任何主体授权过该动作；
- 没有对应工作目录时，明确回复"我没有该仓库的工作区"，不得退到 Agent home 等无关路径写文件。

首版一个项目按 0 或 1 个仓库处理，多仓库场景不设计。

#### 4.5.2 Session 配置为初值、可授权修改

Intent 可携带 model / provider / reasoning-effort / 工作目录的目标 Agent session 配置初值；未提供时使用目标 Agent 默认值。该配置不是不可变快照，授权用户可以在 session 生命周期内修改，包括该 session 的工作目录。

**初值是建议，不是命令，并且只作用于被创建的那条会话。**`session_config` 在 execution session **创建的那一刻**被应用为 seed（router `dispatch(new)`、PM 任务首次建立 `conv:task:` 映射，或用户经 `+` 手动新建）。路由到**已存在**的会话时一个字段都不应用：model / provider / reasoning-effort / workspace 全部保留那条会话自己的值（2026-10-28 修订；此前本节与 ADR-099 验收标准要求复用时"下一轮生效"，workspace 单独例外）。理由是投影的自洽：一个 Chat 是一段对话压着多条可能同时在跑的执行会话，composer 上的 draft 是这段对话对"还没开始的活"的偏好；把它当命令打进一条在跑的会话，等于让一条消息悄悄把任务换到另一个模型或另一个目录，而用户看到的下一个事实是文件写进了错的树。要改一条在跑的会话，去它自己的执行面：model / provider / reasoning-effort 走 `ConversationSession::apply_config`（ADR-047 唯一配置入口，立即持久化、下一轮生效），工作目录走 `route_workspace_switch`（§8 红线单入口，带改 per-agent 默认的副作用）。

应用前必须先钳制到**接收 Agent 自己的允许集**：model / provider 必须落在该 Agent 的 provider list 内，工作目录必须落在其已注册目录集内；越界的字段被丢弃并回退到 Agent 默认，绝不因一个非法建议而失败整条投递。reasoning-effort 不属 provider 允许集，按现有枚举解析——不可解析的值在解析阶段即丢弃，避免落盘后每次读取都解析为 `None`。

**Chat 表面在发送前就能定这些初值。**用户在 Chat tab 直接选 model / reasoning-effort / 工作目录，随本条 `user_message` 一起发出——因为任务一旦发出，Agent 立即开始 execution session 的执行，等切到执行中的 exec tab 再改，模型可能已经跑了好几轮，配置来不及生效。所选值持久在 **Chat session 行的 `session_config`** 上（因此跨 tab 切换、重载、换设备都保留），并作为本条消息的 draft 初值随信封发出；它不改变任何 execution session。

**其中工作目录是 Chat 面自己的属性。**它决定 Chat 面右侧工作区面板投影哪个工作区——文件树、搜索、以及"Add to Chat"引用的来源，三处必须是同一个工作区，否则"从 X 目录里选一个文件加进对话、输入框显示的是 Y 目录"自相矛盾。在 Chat 面切换它只写 Chat session 的 `session_config`，**对 execution session 完全无感**：既不重指向任何已有会话的工作目录（那是执行面面板经 `PUT /sessions/{sid}/workspace` 的显式操作，与本处无关），也不因它而新建会话；只有 router 决定 `dispatch(new)` 时它才作为新会话的初值落地。两个面各自投影自己那份事实：执行面看到并切换"这条会话正在哪个目录里跑"，Chat 面看到并切换"这段对话在哪个目录里工作"。技能（skills）在 Chat 表面**不展示**：它由模型自动加载，非 session 级初值，chat-message 通道也无对应字段，无可写入口，故直接隐藏该 chip，而不是显出来再提示不可用。

**这份 draft 落在服务端的 Chat session 记录上，不是前端局部状态。**按"后端是唯一数据源"，model / provider / workspace / reasoning-effort 的选择持久化在 session 行（`ChatSession.session_config`，经 `PATCH /api/chat-sessions/{id}/session-config` 整体覆盖写回，参与者身份即权限，局外人按 §5.3 隐藏 404），因此上一次的选择能扛过切 tab、刷新与换设备；前端的 draft 只是这行的**缓存与乐观显示**，选定即发 PATCH、PATCH 响应回读规范化值、失败即回滚，绝不独立持有一份后端没有的草稿状态（v2.1 修订）。

**显式选择在它作用的那条路径上不得被静默丢弃，但不再用"冲突即新建"实现（v2.2，2026-10-28 修订收窄其作用面）。**`session_config` 只在 execution session 创建的那一刻作为初值；对已存在的会话，配置差异不再是路由信号，而是交给 router 的语义判断：改参数且说明"继续" → `dispatch` 到该会话，该会话保留它自己的配置（draft 不覆盖在跑的会话，见上）；改参数且描述的是新任务 → `dispatch(new)`，初值用于新会话。要改一条在跑的会话的配置，走那条会话自己执行面上的显式修改（受授权校验）。`session_config` 缺省的字段表达「无意见」，永不构成冲突。配置修改从被接受后影响后续消息 / 后续执行。正在执行的单次操作使用启动该操作时解析出的配置快照，避免用户更改工作目录后正在进行的文件操作中途切换目录。服务端必须校验用户有权修改目标 Agent 的对应 session 配置；不授权的修改失败，不静默忽略。工作目录的修改只能在目标 Agent 已注册的目录集合内选择，不接受任意路径。

### 4.6 Chat session 的串行写入

聊天消息按时间顺序串行写入，后端只有一个权威写入口。采用**每个 Chat session 单写者 / 串行队列**，不采用整个服务全局串行：同一聊天中的写操作严格排序，不同聊天仍可并行处理。

唯一写入口负责：

1. 校验调用者及参与关系；
2. 按 `message_id` 做幂等去重；
3. 按后端接纳顺序分配该 chat session 内单调递增的消息序号 / 顺序键；
4. 按顺序原子追加消息并持久化；
5. 将 Intent 投递与消息记录关联。

同一聊天的规范顺序是后端串行写入口接受并提交的顺序，而不是不同客户端各自的本地时间戳。时间戳用于展示，不作为唯一排序依据。QoS 1 重投或请求重试不得追加重复消息。

此原则从根上避免多个 Runtime、PM 和 UI 直接并发修改聊天记录。具体队列实现可由 `acowork-user` 选择，但所有写入都必须经过同一逻辑写入边界。

由于唯一写入方在消息被接受的当下就能看到它，这个写入边界同时维护着让读取保持有界的派生状态（ADR-096）。`append_message` 把 `message_count` 与 `last_message_preview` 持久化进 `session.json`，并更新幂等 id 索引，使任何请求路径都无需重开 `messages.jsonl` 去数行、判定重复 `message_id`、或投影列表预览。因此预览字段是**写时派生状态，而非读路径投影**：会话列表直接返回存储值，不触碰日志。对日志的唯一一次全量扫描，是每进程一次的首触协调（它同时完成这些字段存在之前所写数据的迁移）。

### 4.7 对外接口清单

接口细节（字段级 schema、错误码枚举）留待实现设计；此处固定**有哪些接口、谁调用、归属哪个模块**，避免后续把业务逻辑塞进 Gateway。

| 接口 | 归属模块 | 调用方 | 职责 |
|---|---|---|---|
| `POST /chat-sessions` | acowork-user | Gateway 转发 | 创建 Chat session（含 `user_agent` kind 的解封） |
| `GET /chat-sessions?participant=` | acowork-user | Gateway 转发 | 列出某参与者的会话；非参与者 403 |
| `GET /chat-sessions/{id}/messages` | acowork-user | Gateway 转发 | 分页读取历史（append-only 快照） |
| `GET /chat-sessions/{id}/search` | acowork-user | Gateway 转发 | 会话内分类历史查询（文本 / 图片 / 文件，游标分页；ADR-097） |
| `POST /chat-sessions/{id}/messages` | acowork-user | Runtime / Gateway | **唯一写入口**；分配 `message_id` + `seq` |
| `GET /chat-sessions/{id}/chat-md` | acowork-user | Gateway 转发 | 读会话级 chat prompt（缺省时按需播种默认；非参与者 404；返回体为剥掉 `# Safety` 尾块后的正文，仅供面板编辑） |
| `PUT /chat-sessions/{id}/chat-md` | acowork-user | Gateway 转发 | 覆盖会话级 chat prompt（群主 / 管理员；16KB 上限；`# Safety` 尾块由 Runtime 重新追加） |
| `GET /chat-sessions/default?pair=` | acowork-user | Gateway 转发 | 取某 pair 的默认会话（§5.4.4） |
| `POST /intent` | acowork-gateway | 前端 / Runtime / PM | Intent 投递入口，仅做通信与资源管理 |
| `GET /sessions/{sid}` | acowork-runtime | 前端 | 合并 meta + live_state，供 execution 视图的状态展示（§5.5.5） |
| `chat_history_read` | acowork-runtime（MCP） | Agent | Agent 侧读取 Chat 历史（§5.5.3） |
| `intent_send` | acowork-runtime（MCP） | Agent | Agent 发起 Intent；声明 `intent:send` 权限 |

不变约束：Chat session 的读写**只经 acowork-user**；Gateway 不得为绕过它而直接访问 Node-local 文件系统（§7.1 红线）。

## 5. 通信协议框架

### 5.1 投递管线

三种形态共用同一条管线，差异只在入口校验与角色判定。

```mermaid
sequenceDiagram
    participant FE as 前端
    participant GW as Gateway (intent_api)
    participant CU as acowork-user (唯一写入口)
    participant RT as Runtime
    FE->>GW: 消息 + target
    GW->>GW: 身份确认 / 参与者校验 / 预算检查
    GW->>CU: append user_message（分配 message_id + seq）
    CU-->>GW: 落库成功
    GW->>RT: MQTT 投递（source_chat_session_id + chat_kind）
    alt 人类消息（kind=user_message, action=chat）
        RT->>RT: Chat Router turn（历史投影 + 候选 + draft）
        RT-->>CU: 纯文本回答 → agent_message（不进入执行会话）
        RT->>RT: dispatch → 复用 / 新建 execution session
    else PM 派单 / agent-agent / 卡片回答
        RT->>RT: 解析 routing key → 复用或新建 execution session
    end
    RT->>CU: 开轮前写一条 execution_round 引用行（空正文）
    RT->>RT: turn 执行（思考 / 工具 / 卡片）
    CU-->>FE: push 新消息；块内容读时投影
```

管线上的四个强约束：

1. **落库先于投递**——Chat session 有该行，才允许执行发生；否则 Runtime 崩溃后消息凭空消失。
2. **投递携带来源会话**——写回触发条件只看 `source_chat_session_id` 是否存在，与本轮角色无关（§5.5.1）。
3. **执行侧写 chat 只剩引用行一处，且经唯一写入口**——Runtime 不直接写文件，Gateway 不代写业务事实（§7.1）；答案与卡片从不落 chat，读时才从执行侧投影。
4. **人类消息的路由是语义决策（v2.2）**——`routing_key` 不再兼任"该由哪个任务接着做"的判断；router 只在候选集内选目标，并使用同一个 `message_id` 与信封坐标转交，不重写正文、不重新落库（ADR-099）。

### 5.2 三形态时序

#### 5.2.1 User-Agent 单聊

```mermaid
sequenceDiagram
    participant U as user-A
    participant CS as Chat session (user_agent)
    participant ES as Execution session (agent-A)
    U->>CS: user_message（文本 + 附件）
    CS->>ES: 投递，发起方角色
    Note over ES,CS: 不写进度行，不写正文副本
    ES->>CS: 轮次边界：一条 execution_round 引用行（空正文）
    ES->>ES: 执行中…
    CS-->>U: 块（读时投影）+ 前端派生等待行（仅本设备 claim 未结算）
    ES-->>CS: 需人决策：卡片进投影（不落 chat 行）
    U-->>CS: 在块里作答（直投执行会话）
    CS-->>U: 投影转终态，块收在途感，等待行早已消失
```

#### 5.2.2 群聊

```mermaid
sequenceDiagram
    participant UA as user-A
    participant UB as user-B
    participant CS as Chat session (project_group)
    participant EX as agent-X execution
    participant EY as agent-Y execution
    UA->>CS: user_message（无 @）
    Note over CS: 仅存档，不触发任何 Agent
    UB->>CS: user_message（@X @Y）
    CS->>EX: 定向投递 X
    CS->>EY: 定向投递 Y
    EX->>CS: execution_round 引用行（X 的那一轮）
    EY->>CS: execution_round 引用行（Y 的那一轮）
    Note over CS: 两次投递互不干扰，各自一条引用行，内容读时投影
```

#### 5.2.3 Agent-Agent

```mermaid
sequenceDiagram
    participant AX as agent-A execution
    participant CS as Chat session (agent_agent)
    participant BY as agent-B execution
    AX->>BY: Intent request（投递，conversation_id 走信封，不落 chat 行）
    BY->>CS: 轮次边界：一条 execution_round 引用行
    BY->>AX: Intent response（投递 push 回发起方，不落 chat 行）
    CS-->>CS: 块内容读时投影
    Note over AX,BY: 预算耗尽 → system_notice / budget_exhausted，双方均知止
```

### 5.3 项目群聊与 @Agent

#### 5.3.1 群聊消息行为

群聊中的普通 user message 只写入 Chat session，不触发 Agent 执行。Agent 默认不监听并回复群聊中的所有消息。用户明确 @ 一个或多个 Agent 时，系统为每个被提及的目标生成一条定向消息并派发；只有被提及的 Agent 收到并处理。

@Agent 解析必须转成明确的目标 Agent 实例列表，而不是让每个 Runtime 自行从正文猜目标。**一条人类发言只落一行 `user_message`**，多个被 @ 的目标记在该行的 `mentions` 字段里；不得为每个目标另写一条正文相同的行——那会让同一段文字在同一个 session 里出现多次，前端必须去重才能正确渲染，且 `seq` 与计数全部虚增。各目标分别路由、各自维护 execution session。**Agent 那一轮不再往 Chat session 写正文副本**（ADR-106 D3/D12）：只落一条 `execution_round` 引用行，`in_reply_to` 指回那条人类消息，内容由块的读时投影呈现（D4/D5）。

**寻址先走确定性阶梯，阶梯走完才跑 router（ADR-106 D6/D7，本节改写原"@ 只选 agent，router 选会话"）。**第一级 `@` 选 agent，第二级 `@` 选它的执行会话。被 @ 的每个 Agent 先在自己的 Runtime 内走前几级的确定性规则：回复某个执行轮块（`in_reply_to` 指向 `execution_round` 引用行）⇒ 续投该会话；`@` 指名会话 ⇒ 投给指名会话；`@` 未指名且该 agent 在本 chat 只有一个可投会话 ⇒ 直投，多个 ⇒ 回一张 `session_choice` 卡（`system_notice` + `meta.options[]`，卡上"新建任务"永远并列），零 ⇒ 落到 router。候选集仍只含 `meta.chat_session_id` 指向本群聊且权限允许的执行会话，绝不列个人私聊或其他群的会话；第二级 `@` 的候选列表由**带 chat 作用域的会话列表读** `GET /api/agents/{id}/sessions?chat_session_id=&surface=`（Gateway 透明代理）给出——不为它新建端点："这个 chat 里我能寻址哪些执行会话"与"这个 agent 的会话列表"是同一个问题的两种问法，两个端点只会得到两份会漂移的答案——**候选过滤就是权限**，它只在拥有执行会话的那台节点上算得出，客户端不得自行复刻这份判定；过滤发生在分页之前，所以分页本身就是窗口。答一张寻址卡不是一句新话语：原文本连同刚选定的地址按原行的 `message_id` **重投递**，chat 存储按该 id 幂等，因此转录仍只有一条消息；地址也只写给提问的那个 agent（下条的 `force_new_for` 同理）。router 的 act 空间收缩为 `new_task(brief)` / `propose_existing(alias, why)` / 纯文本发言：它**不再拥有静默投递旧会话的权力**，提议只能落成 `confirm_continue` 卡由人确认（别名表不再向模型开放投递，只是提议的句柄）。群聊 router 没有 `ask_user_question`，需要澄清时只用文本发言（§5.4.5）。

群聊场景下对话的 owner 是那条人类消息：Agent 那一轮只以引用行出现在群聊 Chat session，内容渲染给人，**不 @ 任何人，因此不唤醒任何 Agent**。多个 Agent 之间的接力不由 Intent 完成，而由人决定是否 @ 下一个（§5.4.5 R5 的推论）。

**未 @ 的消息交给群的默认接收方（ADR-105）。**"agent 刚说完话、人直接回一句"是最高频的交互，而按上面的规则它只归档、谁也不唤醒——用户必须 @ 一次重发。群 Chat session 因此带一个可选的 `default_responder`（该群的一个 agent 参与者）：

- 群发只有一个入口 `POST /api/intents/mentions`。`targets: []` 是**正向指令**（"你决定"），不再是非法请求；有 @ 时按 `targets` 投递，无 @ 时投给 `default_responder`。两级 `@` 同走这一入口：请求体新增 `target_sessions`（agent → 指名会话）与 `force_new_for`（**按 target 的列表**，逐个 agent 拒绝续投），所寻地址以 delivery `params` 键（`target_session_id` / `explicit_mention` / `force_new`）抵达 Runtime，零 proto 变更。`force_new_for` 之所以不是全局旗标：一次扇出把同一句话投给多个 agent，而人只能拒绝**自己正在回答的那一个**的续投——一个旗标会让 `@A @B` 里给 A 选"新任务"顺带替 B 也新建一个会话。校验因此是显式的：地址里的 id 必须在本条消息的 `targets` 内（否则 400），同一 id 不得同时出现在 `target_sessions` 与 `force_new_for`（互相矛盾的指令不由服务端猜）。1:1 面仍是单接收人形态（`ChatMessageRequest.force_new`）。
- 未配置 `default_responder` 的群保持原行为：仅归档，响应 `{"routed": "archived", "results": []}`。纯人类群永远不需要配置。
- 自动路由是行上的独立事实：`meta.mentions` 只记人打的 @ 名单，自动目标写 `meta.auto_routed_to`；@ chip 不因自动路由出现。`auto_routed_to` 只是"这句话去向"的记录，**不再是任何渲染的输入**（v3.1，ADR-106 D9 废止 ADR-105 D4）。等待行只由本设备自己那次发送的 claim 派生——claim 取自发送响应里的 row id，收口于第一条回答它的行（v3.1.2，ADR-106 修订 2026-11-03）。
- 写入 owner-or-admin，目标必须是当下的 typed agent 参与者；成员被移除或 roster 重新同步时该字段自动清空，投递前再复核一次（`live_default_responder`），因此陈旧 id 只会退化为"未配置"，不会投进虚空。
- 投递失败**不落行**（ADR-105 D5 修订，2026-11-06）：Gateway 把失败报给 user 服务的内存态告警表（`chat_alerts`，TTL 600 s、随进程消失），读模型把它作为 `delivery.alert`（`{target, error_code, expires_in}`）装饰在欠投递的那条人类行上，各面用 `DeliveryAlertLine` 画在那一行的列里、按 `expires_in` 自行消失。它既是“失败必须可见”，也正是结算该行 claim 的落点——两条规则仍是同一条，只是载体从日志行变成有时限的实时状态。
- 被选中的 agent 仍跑自己的 Chat Router（§5.4.1）：本机制只决定"第一接收人"，不决定进哪个执行会话。

#### 5.3.2 群聊历史

项目群聊记录：

1. 项目成员的 user message；
2. 发给 Agent 的 Intent request；
3. Agent 回复群聊的 Intent response；
4. 支撑发送方、接收方、mention、回复关联、展示及排序的最小元数据。

群聊历史不包含 Agent 内部思考、工具调用、工具结果或执行中间内容。被 @ 的 Agent 默认收到当前 Intent 消息，不自动获得整段群聊历史；若需查看上下文，通过 `chat_history_read` 在权限和返回上限范围内读取。

#### 5.3.3 可见性原则

参与同一群聊的用户和 Agent 可以看到该群聊中的公开消息；public 的范围限定在此 Chat session 的参与者 / 项目边界，不是平台全局公开。权限规则保持简单：服务端依据聊天参与关系判断能否读取和写入，不把 session ID 本身视为权限。

### 5.4 接收 Agent execution session 的创建与复用

发送方提供来源 chat session 和业务上下文；目标 execution session 的创建 / 复用由系统根据会话类型决定：人类消息由 Chat Router 按语义在候选集内选择；PM 派单与 agent-agent 由确定性映射决定（ADR-099）。

#### 5.4.1 群聊中的 @Agent：群聊内候选 + Router 选择

群聊中 @Agent B 时，@ 只唤醒 B 的 Runtime；具体进入哪个执行会话由 B 的 Chat Router 决定，候选集是**绑定该群聊的 B 的执行会话**：

- 首次 @B（无候选）→ `dispatch(new)`，创建 B 的 execution session 并写入 `chat_session_id = 群聊 id`；
- 后续 @B → 候选含该群绑定的会话（历史 predecessor 也在其中，标记 current）与 PM 任务会话；router 按语义复用，或对无关话题新建；
- 同一群里多个话题因此可以并行到不同执行会话，不再被迫串进一条 `chat:{group}` 链；
- B 的 Intent 回复追加到该群聊 Chat session，`in_reply_to` 指回触发它的那条 `user_message`。

群聊候选只看 `chat_session_id` 绑定，**不按 `conversation_id` 推断**：同一群里可以有多个并行的执行链。候选为空是正常首态，不是错误。

#### 5.4.2 PM 任务：每任务独立逻辑执行上下文

PM 创建任务并向 Agent 派发时，以任务作为执行 session 边界：

- 每个 `(project_id, task_id, target_agent_instance_id)` 对应一个稳定的逻辑执行上下文，首次派发时创建其物理 execution session；
- 同一任务后续发给同一 Agent 的 Intent 继续使用该逻辑上下文；其物理 execution session 长命、原地追加，不因时长或压缩而更换（§4.1）；
- task 进入终态（`Done` / `Rejected` / `Cancelled`）即视为该逻辑执行上下文结束：关闭当前 execution session、删除 `conv:task:` 映射并记入终态清单（候选排除），后续新任务使用新映射；
- 不同任务创建不同的逻辑执行上下文；
- Intent 的来源 chat session / 回复地址是项目群聊；Agent 回复回到项目群聊，并关联派发消息；
- 任务会话在创建时绑定项目群聊（`chat_session_id` + `SessionVisibility::Members`），因此会出现在该群聊的 router 候选里，用户可在群里追问该任务（§8.2）；
- task 终态时 `close_intent_context` 将其记入终态清单，候选构造时排除——终态任务不会被 router"复活"；按同一任务身份新建的是新任务，不是旧任务的重开；
- **PM 派单本身不经 router**：`conversation_id = task:{project}:{task}` 同时是幂等键与生命周期边界，语义路由会破坏这两者。

任务逻辑执行上下文对应 Agent execution session 链，不是 Chat session。群聊负责可见的项目交流历史，PM task ID 负责选择目标 Agent 的执行上下文。

**派发触发与幂等（v2.8，ADR-102）。**派发的唯一触发源是 PM 侧扫描器：每 tick 从索引取「pending + 已过审批闸门 + `start_at` 已排期且到期（创建时缺省填 `now`；`start_at` 为空 = 未排期 = 不派发）+ 未派发 + 有 agent 责任人 + 依赖已满足」的候选集，按优先级推送 `POST /api/intents/task-dispatch`（信封 `kind=pm_task`）。事件钩子（创建即派发 / 审批即派发）撤销——触发时机只有一个事实来源（`start_at` 与 `dispatched_at`），不依赖进程内事件链。投递是**代次幂等**的：扫描器在首次尝试**之前**把 `dispatch_message_id` 落盘（`Task` 字段，crash-safe、重启复用），重试以同一身份重放——chat 行按 `meta.message_id` 去重（存储层唯一写入口的既有合同）、信箱预算按 `message_id` 去重（每代次占 1 个名额，投递失败不再能烧穿 §6.5 R4 上限把任务永久饿死）。失败退避 60s→5m→15m→30m，第 5 次失败后放弃并 `error` 日志（放弃按代次计：状态跃迁/换人清除 `dispatch_message_id` 与 `dispatched_at`，新代次重新计数）。派发到项目群的 chat 行只带人读文本（标题+描述）与 `meta.task_id`（前端渲染为任务胶囊，点击复制裸 id）；`project_id` 不进 meta——行本就活在该项目的群里；唤醒 playbook 走信封 `assignment_brief`（§4.5 字段表），不进消息正文、不落历史。

#### 5.4.3 Agent-Agent：三个 session

Agent A 与 Agent B 建立一对一聊天时存在三个互相独立但可关联的 session：

1. **一个 Agent-Agent Chat session**：由 `acowork-user` 维护，v3.1 起它只承载应答方各轮的 `execution_round` 引用行与通道回执，不再承载 request / response 正文（ADR-106 D12）；不含任一 Agent 的思考和工具调用。
2. **Agent A 的 execution session**：由 A 的 Runtime 在 A 的 home 管理，保存 A 处理收到消息或继续对话的执行上下文。
3. **Agent B 的 execution session**：由 B 的 Runtime 在 B 的 home 管理，保存 B 处理收到消息或继续对话的执行上下文。

Agent A 通过 `intent_send` 向 B 发送 request 时，**这条 request 不再落 chat 行**（v3.1，ADR-106 D12）：它直接投递到 B 的 execution session，B 的轮次边界在自己的会话绑定的 pair chat 上落一条引用行；response 同样以投递的形式回到 A 的入站面，**不再写 `resp-` 响应行**，并由它关掉 mailbox 里的那条待答请求。A 若仍在执行，response 注入其当前 turn；A 若已空闲，唤醒 A 开新 turn。被自己的 response 唤醒的 turn 仍属发起方角色，复用同一 `conversation_id`（§5.4.5）。Chat session ID 不等于 A 或 B 的 execution session ID。

Agent-Agent Chat session 的首次创建由明确的聊天入口 / 首次建立对话触发；创建后，按同一聊天关系继续复用该 Chat session。双方各自的 execution session 关联由系统维护，并采用稳定逻辑映射、物理 execution session 长命的规则（§4.1）；Agent 无需在每次消息里判断新建还是复用。

**A-B 之间的引用行总是落在双方共享的 agent-agent Chat session，与 A 本轮由什么触发无关。**A 被用户单聊消息触发后去问 B 时，投递信封的 `source_chat_session_id` 填该共享 Chat session（pair 首次发送时幂等创建），于是 B 的那一轮属于这段对话；A 自己那一轮的引用行仍落在 A 绑定的那个 chat（通常是用户的单聊），因为 D11 的绑定唯一。这条规则不因 user-agent 单聊本身成为 Chat session 而改变：**user-A 不是 A↔B 这段对话的参与者**，把 A-B 交流写进 A 与用户的会话会把第三方消息泄露给不属于它的人。A 的用户在 Agent 页面看到的是 A 的单聊块（投影 A 那一轮，含它向 B 发问的回答）与 B 在 pair chat 的那一轮。

#### 5.4.4 User-Agent 单聊：Agent 页面的默认对话面

用户在 Agent 页面与某个 Agent 的一对一对话**就是一种 Chat session**（`kind = user_agent`），与群聊、agent-agent 共用同一套存储、词汇表、唯一写入口和前端会话模型。它不是 Agent 的 execution session，也不替代 execution session：

- 打开 Agent → 解析该 (user, agent) 的默认 Chat session（不存在则幂等创建）；
- 用户在 Chat session 输入消息 → 落一行 `user_message` → 投递给该 Agent → Chat Router 在本 chat 候选内选择（直接回答 / 复用 / 新建）→ 进入对应 execution session；
- Agent 轮次边界 → 在本 chat 落一行 `execution_round` 引用行（空正文，v3.1：答案与卡片进投影，不落行）；
- 每个 (user, agent) 恰好一条 Chat session（v2.1）：没有"新建第二条同 pair 会话"或会话列表的入口。多会话并行仍是业务事实（ADR-086 决策 8 已否决"一个 Agent 一个活动会话"），但它由 **execution session 层**承载——标签栏的 `+` 新建的是 execution session tab，不是第二条 Chat session。

**人类消息是 `user_message`，不是 Intent request。**这条必须钉死：若把用户消息实现成 request 语义，Runtime 会把该轮判成 responder，于是 `ask_user_question` 和 `intent_send` 被从工具集里摘掉、审批被自动拒绝——单聊直接废掉。传输可以复用 Intent 投递管线，**入站类型必须是人类输入，本轮角色是发起方**（§5.4.5）。

**执行过程不进 Chat session，但必须可见。**Chat session 按 §2.3 只存消息；若前端只渲染 Chat session 的行，用户就变成"发完消息空等几分钟，然后蹦出最终答案"。规则：**Chat session 是持久对话记录，在途轮次只是当前窗口的运行期状态**。前端 = 历史渲染自 Chat session（引用行 + 读时投影成一个块）+ 在途渲染自本设备自己那次发送的 claim（POST 在途 → 转录落点）；块转终态后等待行早已消失（§5.5.5，v3.1 重写本段、v3.1.2 延长判据窗口）。不变量不破，UX 不退化。

**长命执行会话因此对单聊同样成立（v2.7 修订）。**v2.2 及之前用 §4.1 的 T1/T2 轮转来控制用户直聊那条 JSONL 的无限增长；轮转删除后，单聊走同一条 `chat:{chat_session_id}` 逻辑映射且其物理会话原地追加，文件规模由既有 JSONL 硬上限兜底、送入模型的上下文由 compaction 阈值控制。"连续性由 Chat session 承载而非物理 execution session"这条已确认原则不因删除轮转而改变，反而更干净：用户看到的是一条不断变长的对话，而不是一个会被更换的执行窗口。

**Chat session 是 Agent 页的固定 tab，与 execution session tab 共存（v2.1）。**打开 Agent 必然落在 Chat tab 上，它不可关闭、不可删除，不存在"没有会话可看"的空白态；execution session 是同一标签栏里的普通 tab，可随意开关，关掉最后一个也只是回到 Chat tab，两个表面互不销毁。执行侧入口保留——Session history 仍可达，卡片点击也可达。后台不等于隐藏，否则迁移前的既有 execution session 无从触达，调试与成本审计也失去落点。

**因此 `pairs.json` 的单条语义是对的（v2.1）。**v2.0 里"默认打开 default chat"与"点 `+` 建第二条同 pair 会话"要同时成立，单条索引装不下，该缺陷因此从 P2 升为 P1；微信模型定稿后每个 pair 恰好一条会话，`pairs.json` 存该 pair 的唯一会话正是正确形态，升级项作废。其余 kind 的会话（群聊、agent-agent）仍经 `list_for` 枚举。

**不迁移历史。**既有 execution session 留在 Session history，新会话才建 Chat session。好处是这一步完全可回滚：Chat surface 不可达（无账号、用户服务异常）时 Agent 页整体退回迁移前的 exec 形态（§6.1）。

#### 5.4.5 对话所有权与停止条件

Intent 采用 C/S 式的所有权模型：一次对话由发起方拥有并负责结束，应对方只对每次输入给出结果，不具备开启下一轮的能力。

| 规则 | 内容 |
|---|---|
| **R1 所有权** | 只有被人类输入（用户消息 / PM 派单 / cron）触发的 turn 才能发起对话；作为应对方处理的 turn 不得发起新对话 |
| **R2 两种方向** | Intent 字段只有 request 与 response 两种方向语义，v3.1 起这语义活在**投递信封**而非 chat 行（ADR-106 D12）。`intent_send` 只能产生 request 方向的投递；response 方向只能由系统在应对方 turn 结束时产生（§5.5.1），不构成新对话的起点 |
| **R3 结束** | 发起方不再发 request，对话即结束。结束是"不再说话"，不是进入某个 Ended 状态，无需超时或清理动作 |
| **R4 上限** | 单对话 request 次数有上限；超限时系统拒绝投递，并落一条 `system_notice`（`budget_exhausted`）记下这个通道事实（v3.1，ADR-106 D12：不再是 Gateway 代写的对话行） |
| **R5 需要第三方** | 应对方不得转问别人，把"我需要什么"写进 response 交回发起方决定 |

角色由入站消息类型决定，不由模型自行声明：

| 入站 | 本轮角色 | `intent_send` | `conversation_id` |
|---|---|---|---|
| 人类消息（用户单聊 / 群聊 @ / PM 派单 / cron） | 发起方 | 可用 | 新建 |
| Intent request（别的 Agent 问我） | 应对方 | **不在本轮 tool set 中** | 复用入站值 |
| 应答（别的 Agent 答我） | 发起方，延续自己的对话 | 可用 | 复用 |

**入站类型是信封 `kind` 字段上的四值词汇表（v2.8，ADR-102），不是靠字符串线索推断的潜规则。**`user_message`（人类聊天轮：单聊 / 群聊 @）、`pm_task`（PM 任务派发——人类输入，但路由键是任务身份、不经 router）、`request`（agent 发起的对话请求；空值与未知值保守兜底为本项，覆盖 cron 与混版的旧生产者）、`response`（应答回传）。每个值在 `acowork-core::intent::TurnOrigin` 的策略表上对应一行 `{router_eligible, render_envelope, responder, write_back}`：路由资格、渲染形态（人读文本还是完整信封）、本轮角色（R1 的工具裁剪依据）、写回义务四个决策点**查同一张表**，生产端（Gateway 各构造点）显式盖章，消费端不再各自从 `conversation_id` 前缀或 kind 字面量反推场景。`pm_task` 与 `user_message` 的差别只在路由资格一项——它是人类输入，角色与渲染与聊天轮相同（v2.8 修复的正是它此前骑 `request` 值、被按应对方裁剪 `ask_user_question` 并被渲染成信封 JSON 的错位）。task 坐标（`task:{project}:{task}` 与 `conv:task:{project}:{task}`）的格式常量与判定/构造函数同样收在 `acowork-core::intent`，feature 代码不得裸写 `starts_with`。

R1 的强制点在工具可见性：应对方 turn 的工具集合里没有 `intent_send`，同时移除 `ask_user_question`（靠工具可见性强制，不是运行时报错）。模型看不到这两个工具，因此既不会尝试，也不会消耗 token 想别的路子绕过去——比运行时报错更省，也更硬。

**Chat Router turn 不属于上表两角色（v2.2）。**它的工具集只有 `dispatch` 一个：没有 `intent_send`（不得代人类 @ 其他 Agent——@ 是用户行为，router 代为发起会绕过"谁有权唤醒谁"）、没有 `ask_user_question`（卡片有归属 / 超时 / 答复状态机，router 澄清只用文本）、没有文件与执行工具（它不做任务，只决定谁来做）。执行 turn 则相反：不暴露 `dispatch`，`intent_send` / `chat_history_read` / `ask_user_question` 按既有 responder / 可答者规则裁剪。三层工具可见性互不重叠，同样靠可见性硬约束（设计 29）。

**`ask_user_question` 的移除条件是可答者是否存在，不是消息来自谁。**原文把它写成"应对方背后无人可答"，这个理由比措辞弱：群聊里 @ 这个 Agent 的那个人就是可答者。因此规则改为按事实判定——**该 Chat session 内存在人类参与者 → 卡片可以创建；纯 agent-agent 会话无人类 → 不创建工具、按 §5.5.1 兜底回执**。成本闸门（R4 的 request 计数）与防环路不受影响，因为卡片不产生新对话、也不增加 request 计数。

```mermaid
graph LR
    H["人类输入"] -->|"开新对话, A 为 owner"| A["Agent A"]
    A -->|"request"| B["Agent B"]
    B -->|"response, 非新对话起点"| A
    A -->|"request, 仍属 A 的对话"| C["Agent C"]
    C -->|"response"| A
    B -.->|"应对方 turn 无发起权"| X["intent_send 不可见"]
```

循环没有生成路径：B 无法延长对话（R1）；A 的每次 request 可计数（R4）；A 的 turn 必然结束。因此不需要因果链深度、链路 ID 或跨 agent 环路检测。

| 参数 | 默认 | 范围 | 开放给 Session 设置卡片 |
|---|---|---|---|
| 单对话 request 次数上限 | 6 | 2–16（0 = 禁止 Agent 间对话） | 是（Agent 级） |

人是唯一切断点，也是唯一重启点：对话触顶后要继续，由用户发一条新消息 = 新对话。**不提供"应对方申请提高上限"的能力**——那等于让循环自行决定是否继续。

编排归属：决定"谁接着做、按什么顺序做"属于人 / PM 的任务依赖图，不属于 Intent（§3.2 非目标"不为 Intent 引入任务编排"）。R5 是这条边界的具体体现。

### 5.5 回复、历史入口与历史读取

#### 5.5.1 回复写回

**Agent 的回复不是模型的工具调用，而是系统在 turn 结束时生成的产物。**Agent 回答一条消息，与用户和 agent 正常对话中"说完话结束"是同一件事；若把回复做成工具，模型说完话就直接结束 turn 是常态，发起方将永远等不到结果。

**写回的触发条件是"本轮由某个 Chat session 的消息触发"，与本轮角色无关。**这一点必须写明：早期版本只在应对方 turn 结束时写回，而 §5.4.4 的用户单聊轮次是**发起方**角色——照原实现，Agent 在单聊里的回复不会落进 Chat session，单聊会话永远是空的。现在的规则：入站消息携带 `source_chat_session_id` → 本轮必须在**轮次边界**写一行 `execution_round` 引用行（v3.1 修订：正文不再是 chat 行，答案活在执行侧、由投影呈现）；不携带（cron 自启、无会话上下文）→ 不写任何 chat 行。

回复正文的来源是**本轮 turn 的最终输出**，不是"从上下文末尾取一段"：一轮 turn 结束后上下文末尾可能有多条 assistant 文本（工具调用前的叙述、中间结论、最终总结），取多少只能靠猜。Runtime 已有一等公民的最终输出（流式结束事件携带本轮回复正文，写入前已剥离思考块），回复正文即该文本——一条消息，不是一个窗口（v3.1：这段文本留在执行会话，块的投影把它端出来，不再抄进 chat）。

任何属于某个 chat 的轮次，结束时**无论成败都必须把终态交给投影**（v3.1：终态是执行侧轮次记录里的字段，不再落 chat 行）：

| 结束形态 | `status` | 正文 |
|---|---|---|
| 正常完成 | `completed` | 本轮最终输出文本 |
| LLM / 工具错误 | `failed` | 仅取面向用户的错误摘要，**不含内部 detail**——内部错误不外发给另一个 agent |
| 用户手动 stop | `aborted` | 本轮已产出的文本，可为空 |
| 以未决卡片结束 | `waiting` | 卡片摘要（§5.5.4），正文可为空 |
| Runtime 崩溃 / 无任何事件 | `failed` | 系统兜底固定文案 |

**应对方 turn 不得进入无限期等待人类的状态。**现有两个暂停态（迭代上限暂停、环路检测暂停）依赖人在前端点"继续"。规则按可答者分流（§5.4.5）：会话内有可答的人类 → 卡片进那一轮的投影（不落 chat 行）、本轮以 `waiting` 结束、**释放该 session 的串行执行槽**，人类回答后开新 turn 继续（§5.5.4）；无可答者（纯 agent-agent）→ 一律按 `aborted` 终态结束，把"卡在哪"写进正文。

写路径：引用行由 Runtime 经 Gateway 的 chat-append 中继交给唯一聊天写入口（这是全仓只剩的一处执行侧→ chat 写）；答案与卡片不写 chat，读取时经投影取得。Runtime 不直接写入另一个 Runtime home 下的 session 文件。

**发起方通过 push 获知结果，不轮询。**回复产生后由系统路由为发起方的入站消息：其 execution session 仍在执行则在迭代边界注入当前 turn（同一轮拿到答案），已空闲则唤醒开新 turn。LLM 不具备等待能力，若结果只能靠查询获取，模型会在 turn 内反复重试 = busy-wait，白烧 iteration 与 token；而发起方发完 request 通常已结束 turn，没有"活着的轮次"可轮询。

长度上限：回复正文按字节 / token 双限截断并带 `truncated` 标记。发起方需要更多内容，**再发一条 request 说清要哪部分**，不提供读取对方 execution session 的能力（§2.3 边界不变量）。

回复地址（v3.1 修订）：Agent-Agent 场景 response 直接投递回发起方的 execution session，pair chat 只承载引用行；PM 场景的 task 状态由 agent 自己调 `pm_submit_task` 推进（服务读工具调用的结果，不读 chat 行，也不是 LLM 轮询），群里的人看到的是那一轮的块；用户单聊与群聊场景为对应 Chat session 的块投影，只渲染给人。

#### 5.5.2 Agent 页面会话历史入口

Agent 页保留现有 **Session history** 入口，用于查看该 Agent 的 execution session 历史。v2.0 另设的 **Chat history** 按钮撤销（v2.1）：user-agent 单聊就是固定 Chat tab，不再经列表入口进入，Agent 页不再有 Chat session 列表 UI。

其它 kind 的 Chat session（与其他 Agent 的一对一聊天、参与的群聊、项目群聊等）仍从各自业务视图（项目、联系人列表等）进入，选择某条会话展示 `acowork-user` 中的聊天消息；它不展示、也不拼接 Agent 私有 execution session 中的思考和工具调用。用户和 Agent 的普通直接会话历史仍留在现有 Session history。

#### 5.5.3 `chat_history_read`

`chat_history_read` 读取 Chat session 的历史消息。它**不用于获取本次对话的结果**——结果由系统 push 送达（§5.5.1）。它的用途是读取"不属于本次对话"的消息：

1. 群聊里更早的人类讨论（§5.3.2：被 @ 的 Agent 不自动获得整段历史）；
2. 同一次 @ 了多个 Agent 时，A 查看 B 的回复；
3. 群里发给其他 Agent 的消息。

工具不能读取任意 session；默认只允许查询调用者有参与关系的 Chat session。入参是 `chat_session_id + offset + count`，**不接受 message_id 作为"等结果"的句柄**——那会把它变成轮询接口。

命名说明：本工具原名 `intent_send_result_query`，与 `intent_send` 成对。该名称会让模型把"发完 request 立刻查询"当成取结果的必经步骤，主动诱导上段的失效模式，因此改名；工具描述必须写明"response 会自动送达，本工具仅用于读取会话中的其他消息"。

工具提供 `offset` 和 `count`，以消息追加顺序分页读取 Chat session 消息。LLM 可以根据已读内容决定是否继续读取，prompt 可以建议单次读取量，但不能代替服务端限制。后端必须对单次 `count`、返回字节数或等效负载设置硬上限，并对查询的 Chat session 做参与关系校验。

建议请求语义：

```json
{
  "chat_session_id": "...",
  "offset": 0,
  "count": 20
}
```

返回至少包含消息、稳定顺序 / offset 信息及是否还有后续消息。若 chat session 为追加写入，offset/count 可作为首期分页方式；并发追加时后端返回的顺序必须稳定。未来若支持中间删除或复杂同步，再评估 cursor，而不在首期引入。

**验收**：Agent 页固定 Chat tab 展示该 pair 唯一会话，无列表入口；其它 kind 的 Chat session 按业务视图列出，点入只读，显示参与者、消息、每条消息的 `agent_session_id`；不提供输入框。

#### 5.5.4 需要人决策的东西：在执行侧产生，在块里可答（v3.1 修订，ADR-106 D8 / D10）

execution session 中需要人决策的东西——`ask_user_question` 的选项、工具权限审批、迭代上限 / 环路的暂停续跑、失败重试——**不再是 chat 的一行**（§4.2.2）：它随轮次投影以非终态 `card` 快照进入那一轮的块，在块里就地可答，答复直接回到持有该轮的执行会话。这是**类型层的统一**：同一套卡片类型、同一个来源（投影）、同一份呈现语法，用户在 Agent 页面、群聊、单聊里回答的是同一种东西。

**为什么必须走投影。**卡片作为 chat durable 行时，答句的写入口也在 chat 侧：一次跨进程写失败，卡就永远"未决"，而执行侧其实早已继续——答案有两块真源，这正是 v2.4–v2.8 那一族缺陷的成因。现在卡片的内容只在需要呈现它的那次读里存在，chat 的写路径只剩引用行一处。**未决判定不再靠 chat 反查**：看投影的 `card != null` 与 `terminal`，过期也在读时求值，不写回存储。旧转录里的 `interaction_request` 行与 `interaction_resolved` 回执留在盘上但呈现层隐藏——答句的写入口已不存在，旧历史不得复活一张可点的卡（ADR-106 D14）。

**语义层的非统一，一处，加上界面层的另一层收窄，必须写明：**

1. **本轮持有 vs 结束本轮。**若所有卡片一律结束本轮，编码 Agent 一次任务里几十次文件写审批就变成几十次"结束 turn → 人回答 → 重放上下文开新 turn"，token 与延迟成本都不可接受。因此：卡片**总是**进投影（呈现 + 回答通道，但不进 chat 转录），而执行侧按卡片类型决定是持有本轮等待（沿用现有 `ask_user_question` 的长轮询语义，带超时上限）还是结束本轮、释放执行槽（§5.5.1 的 `waiting`）。前者是执行侧的等待策略，后者是呈现层的读时事实，两者不冲突。

2. **可答者判定。**会话内存在人类参与者 → 卡片可创建、可回答；纯 agent-agent 会话 → 不创建需要人的东西，按 §5.4.5 / §5.5.1 走自动拒绝或 `aborted` 回执。判据是"有没有人能答"这一事实，不是"消息来自谁"。界面上再收窄一层：**只有发起这一轮的人看得到按钮**，旁观者看得到问题而不得替人做决定（ADR-106 D10）。

卡片不产生新对话、不计入 §5.4.5 R4 的 request 上限——它是对话内的一次交互，不是新的委派。回答卡片不唤醒应对方以外的任何 Agent。**寻址问题是另一类**：`session_choice` / `confirm_continue` 两张卡问的是"这句话该投给哪个任务"，它们仍是 chat 的 `system_notice` 行，答案是人类那句话的一次重投递（带地址 + 原行 `message_id` 幂等，ADR-106 D6），不是执行侧的决策。

#### 5.5.5 一轮的呈现：引用行 + 块投影（读时内容）与执行视图入口（v3.1 修订，ADR-106 D3–D5 / D9）

用户发完消息到 Agent 回复之间可能间隔几分钟。若这段时间聊天界面什么都不显示，UX 就退化成"发完消息空等，然后蹦出答案"。本节规定这段时间在 chat 里的呈现：**一条引用行 + 一次块投影读**，再加前端自己那一条在途等待行——没有任何中间状态落盘，也没有任何执行内容被复制进 chat（v2.1 的"执行状态卡片"与 v2.3 的"进度事件行"已撤销，v2.5 的 `turn_routed` 随本节一并废止）。

**为什么撤销 live 卡片。** chat 是 append-only 的持久事件日志；live 卡片是在这套真相之上叠加的**第二块镜像真相**（数据源为 execution session 的 `session_state` 事件流）。两块真相永远对不齐：消息被路由进一个正忙的 execution session 后开新一轮，卡片按镜像显示"未 idle"，而 chat 侧当时毫无反馈，于是"派发后到回复前"出现可见性空窗；stop 当前轮时卡片即时消失，被加塞的 pending 却已开新一轮、执行并未 idle，卡片与真相打架。

**为什么连事件行也不要（v2.4）。** v2.3 把"已派发 / 已开轮"落成 durable 指针行，但实测中它们对**历史会话**毫无意义："你的消息送达了 agent""这一轮开跑了"是执行面的运行时事实，对三个月后翻记录的人零信息量；落下去只是把进度日志混进对话日志，让每段历史都飘满状态行。真正值得留存的"为什么没有回复"由终态行表达（v3.1 起：由引用行的块投影与 `system_notice` 通道回执表达）。因此 v2.4 把中间状态彻底移出 chat：**历史只记对话与"未获回答的事实"，进度只在运行时的当前窗口里存在。**

**运行中：块 + 一条前端等待行（都不落盘）。**

1. **块**说这个会话的每一轮到哪一步：引用行给坐标，投影给状态词、答案、卡片。投影四态 `pending` / `ok` / `not_found` / `unreachable` 各有一种形状——只有 `pending` 可以说"执行中"，`not_found` 是显式空洞，`unreachable` 是一次带重试的故障（ADR-106 D5 / D9）。
2. **"进行中"是正证据，不是缺证据。**等待行的判据是本设备自己那次发送的 claim（v3.1.2，ADR-106 修订 2026-11-03）：发送响应给了 row id、且服务端确实接下了这句话，才写 `inflight[chatSessionId] = { messageId, at }`——1:1 看 `status === "delivered"`，群看 `results[]` 里任一 target delivered，claim 记**顶层** row id（一行 N 次投递，欠答案的是那一行）。`routed: "archived"` 与全失败不建 claim：没有接收人的发送不欠转录任何东西，等待它等于伪造一个结局。**为什么不能停在"本地在途的那次 POST"（旧 v3.1 写法）**：POST 约 150 ms 就返回，它只证明投递落地，而该轮第一条 durable 行实测落在 0.008 s – 81.3 s 之后——那段窗口里转录一片空白、composer 立即可再发，用户读到的是"我这条没发成功"，并且能在上一句尚无落点时压入下一句。历史上"最新一条 `user_message` 尚无终态回答"式的反推依旧全部废止：它把"读不到"当成"还在跑"，主机不可达时画出一个永不结束的轮次；`meta.auto_routed_to` 那一类证据位也一并退出（ADR-106 D14 对 ADR-105 D4 的废止）。前端不读 execution session 实时状态、不新增 MQTT 通道、不改会话列表 API。
3. **结束条件与闸门**：转录里出现第一条 `meta.in_reply_to === claim.messageId` 且 kind ∈ {`execution_round`, `agent_message`, `system_notice`} 的行即释放（呈现层隐藏的 `RETIRED_NOTICES` 不算落点；`delivery_failed` 自 2026-11-06 起属于隐藏集合，失败经由 `delivery.alert` 结算）；结局行永不出现时由 300 s deadline 兜底，释放按 messageId 匹配，因此迟到的结算不会误清后继发送的 claim——"进行中"是一份有到期日的声明，不是一条派生自缺行的事实。发送失败另有一条 `delivery_failed` 通知，它必须可见，也正是该行 claim 的结算行（ADR-105 的"失败必须可见"语义保留，形状改成通知）。**同一个 claim 同时驱动等待行与 composer 闸门**（`turnInflight` ⇒ `composerLocked`）：上一句未确认落点前发不出下一句，按钮禁用并给出原因——锁是诚实的呈现，不是排队的伪装，因为系统里没有队列。`sending` 退回原义（POST 在途），不再是等待行与锁的信号。轮的在途感仍由块自己收，投影转终态即消失：claim 说的是"我这句话有没有落点"，块说的是"那个会话到哪一步"，两个对象。落点一旦出现，人可以立刻发下一句，即使那一轮尚未终态；等待行不宣称任何 agent 在忙，因此纯人类会话里它也无害，刷新或重开后也没有等待行（本设备没有自己的发送可声明）。
4. **文案一条，不分态**：不区分排队 / 开轮 / 工具执行——前端无法可靠区分它们，分了就是猜测。群面一律不指名（多 agent 时点名会误导），1:1 面可指名（该面恰好只有一个 agent）。带文字滚动闪烁动画（复用"正在执行工具"的 shimmer），给用户"有人在干活"的实感。
5. **只在末尾显示**：窗口被 locate 定位到中段（后面还有未加载的新页）时没有"最新消息"可言，不显示。

**历史里留什么。**一轮的执行结果不再是 chat 的 `agent_message`——引用行才是历史的一部分，正文留在执行侧。`agent_message` 只剩一个生产者：Router 的自答（§4.2；v3.1 起 agent↔agent 的回答也不再是 chat 行，ADR-106 D12）。"为什么没有回复"这类通道事实仍是 `system_notice`：`budget_exhausted` / `truncated` / `unavailable`（v3.1：预算收据从 Gateway 代写的对话行改成通知，ADR-106 D12）。`delivery_failed` 2026-11-06 起退出这一格：它不落盘，作为 `delivery.alert` 画在欠投递的那条行上（ADR-105 D5 修订）。旧的 `turn_dispatched` / `turn_started` / `turn_routed` / `router_fallback` / `interaction_resolved` 行不迁移、不删除，但在渲染期一律隐藏（Desktop 两视图 + Mobile 同一套隐藏集合）；历史 `agent_message` echo 仍按普通气泡呈现，历史 `interaction_request` 卡不复活（§5.5.4）。

**块是"一个执行会话"，不是"一条消息"（v3.1.1 修订，ADR-106 呈现契约）。**存储按轮、渲染按会话：布局层按 `agent_instance_id|agent_session_id` 分组引用行，**一个执行会话在转录里只出现一次、成一个块**；块锚在该会话已加载窗口里的**最早**引用行——**位置属于这次往来，不属于它最新那一轮**（v3.4 修订，见下条）。被折叠的触发行（引用行 `in_reply_to` 指向的人类消息）不再作为独立气泡出现——一句话只出现在一个地方（D6 重投递是同一规则的既例）。同一窗口里两个会话各自成块，不会合并成一个"看起来在干所有事"的块。

**块的位置不随新轮次移动，更新靠"把视口带过去"（v3.4 修订，ADR-106 呈现契约）。**锚定"最新引用行"是错的：一个会话比打开它的那一轮活得久，每次续投都往转录更下方写一条引用行，块于是被拖着跨过中间所有行——几次往来之后，人的消息全堆在上面、回答它们的块在最下面，一来一回没了。改为锚定该会话在本窗口里**最早**那一行：续投只增加块内轮次与轮数，位置一次定死。不损失可读性——块内默认展开最近 2 轮、本身是滚动容器，`keyOfSeq` 仍把每个被折叠的 seq 映射到块，所以 locate 一条新轮次照样落在块上；虚拟列表的 item 身份也从"最新行"变成"首行"，块在会话生命周期内不再重建（旧实现每轮都换 key，连高度缓存一起丢）。由此有两条配套。①布局必须**先算映射再出行**：旧实现边走边写 `keyOfSeq`，被折叠的行若排在它所属块之后，会把块自身的映射覆盖成该行的 key，locate 于是指向一个不在 DOM 里的行——只是因为块总在最新行（即最后）才侥幸成立。②`confirm_continue` 询问卡不再作为独立行停在最新处，而是挂到它所指会话**块的尾部**（仅当 `options` 恰好只有一个目标、且该块在本窗口唯一时才挂；`session_choice` 多候选没有唯一目标，留在原位），块收起时同样可见（D8：藏在折叠后的可答项不可答）；卡片落位的那一帧，surface 把视口带到该块——仅在块不在视口时、只做一次、居中对齐，复用既有 locate 通道。**不做常驻角标、不做闪烁、不发 toast**：读者被直接带到问题面前，就地回答。

**块头回答"这个会话在做什么"（v3.1，废止 v2.5 的 `turn_routed`）。**路由结论不再单独落一行：块头就是那条信息——agent 头像 + 名字 + 会话名（优先执行会话自身标题，空则首轮触发正文截断）+ `N 轮 · 状态词` + 任务号；状态词的**主语是这个会话此刻在干什么**：会话在跑（`session_state` 为 processing）就照会话的状态词说（思考中 / 工具执行中 …）——**即使块内最新一轮已经答复**，因为会话可能已在跑一条本聊天还没有引用行的新轮，而那个事实只有会话自己知道，块头不冒充知道得更少；会话空闲才落到块内最新一轮（块就锚在那一行）的投影上——停在未答卡片上的轮说"等待你回"（它不是在跑，是在等人），已终态说它的终态，读成空洞/故障就说"结果未知"。与此**独立**的另一条断言是"在跑"标记（转圈图标）：它只认本块最新一轮的 `pending`，会话另有新轮不许冒充本块在跑（ADR-106 D9：进行中是正证据，且证据得属于被描述的那一轮）。内容来自投影/推送，因此永远与执行侧一致，也不再出现"写失败就永远没说"或"读不到就当在跑"这两宗（ADR-106 D9：进行中是正证据）。**头像不变量**在块级：一个会话一个块只挂一次头像 / 名字 / 跳转（ADR-100 Rev.3 的不变量本身不变，承载从"两行折叠"变成"块的一次"）。等待行不因块存在而消失：它说的是自己那次写，块说的是那个会话。块与块内每一轮**默认都折叠**（v3.5），问答对要人点开——这是块不至于把转录拉高一大截的原因，而读者并不因此少读一句：这一会话**最新一条已答复的 assistant 消息**画在块**下方**，用转录自己的 agent 气泡渲染（折叠着的轮次本来就带这份投影 `answer`，不新增读取）；但**触发行所在的轮永不被窗口藏起来**（它承载的是本聊天自己那句话，行已被折进块里，藏掉这轮等于让这句话在整条转录里消失，也让可见的几行看起来像 agent 在回答没人问过的问题），**停在未答卡片上的轮同样常驻**（D8：藏在"显示更早"后面的卡片不可答）；只有块自己长出来的那些轮（人转到执行面去说、或会话自己继续迭代）留在"显示更早 N 轮"后面。v1 不做流式，块头不承担逐 token 更新。

**执行视图的输入不进 chat（v3.0，废止 v2.6；v3.1 又收回了回写的那一半）。**上一段的回写只覆盖**尾**：绑定 chat 的执行会话，其回合的交互卡片与终态 echo 曾落进那个 chat——现在尾也不落了，回写从 chat 收回执行侧（引用行 + 投影，ADR-106 D3 / D8）。**头**——人类在执行面敲的那句——不落 chat，只存在于执行会话自己的 JSONL。v2.6 曾为把它落进 chat 引入 `POST /api/intents/chat-message` + `for_execution_session` 定向直投，而 chat 里的 `user_message` 正是 Router 的输入：整套 pin 机制（`Closed` 重开、被拒后的确定性路由、`pinned_agent` 参与者校验、字段上限、per-chat 投递队列）都是为抵消它自己造出来的问题而存在，v3.0 起取消（ADR-100 Rev. 8）。数据源上因此只剩一种 chat `user_message`：Gateway 为 chat 面回合写的那一条。

**"查看执行"跳转，不是状态镜像。**跳转是**块头上方动作行**里的一个独立 affordance（**脑图标**、纯图标无文字，`aria-label` 为"查看执行"，tooltip 带该执行会话名——固定标签重复图标等于没说），坐标直接来自块自身的 `agent_instance_id` / `agent_session_id`，点击 best-effort 打开该 execution session——块打开的是执行会话这个对象，所以块内每一轮不再重复挂跳转（v3.1.1 修订）。它只跳转、不镜像——chat 界面不读、不持有 execution session 的实时状态，因此没有会打架的第二块真相，也无需为该状态镜像维护前端投影。

**执行界面的 pending 队列仍是投影，与本次改动无关。**输入框上方的「消息队列」盒反映后端真实排队消息：一条 chat 转发的 `user_message` 被路由进一个正忙的 execution session 时，Runtime 维护一份**有界**的 pending 清单——意图在目标 session 正忙时于入队侧记录、在该轮出队开跑时清除；条目只携带 `message_id`、发送者、受限摘要（≤ 120 字符，ADR-096 受限读）与 `from_chat` 标记，随既有的 retained `session_state` 快照（design 29 §6.4 的同一投影面）推送。它是 **execution 视图内**的能力，保留不动；本次只改 **chat 侧**的呈现。idle session 不入队（消息直接开跑、落进历史即可），因此这份清单不引入第二个写入口、也不落库。

**工具授权 / ask_question 落在块里（v3.1 修订）。**`ask_user_question`、权限审批这类需要人决策的东西随轮次投影进入那一轮的块，在块里就地可答（1:1、群聊、PM 任务共用同一套块语法）；它不再是 chat 的 durable 行（§5.5.4 / ADR-106 D8）。它不是 live 镜像——卡的内容只在读时存在，chat 不持有一份会变旧的状态副本。本轮执行到哪一步、正在等哪个工具，仍只在 execution 视图里细看，从块头"查看执行"跳转过去。

**块的动作行与续投抽屉（v3.3 修订，ADR-106 修订三 2026-11-06）。**续投一条已看见的任务不该被赶到主 composer 去重新寻址；v3.2 把它放在块底、与块头的"查看执行"分处两端，等于一个对象挂两个交互带，且底部输入行不使用也占着高度。v3.3 把交互收成一行：块头上方一行纯图标动作——查看执行（跳该执行会话）/ `@` 引用 / 复制整块 / 续投。发消息按钮自成胶囊，hover / 聚焦 / 草稿非空 / locate 上升沿任一时输入框向右抽屉式展开，图标留在胶囊左端（"输入框把按钮包住"）；块底部不再保留任何输入行。三条机械约束：行高固定、只过渡 `max-width`（抽屉开合不得推动下方转录）；胶囊内输入框宽度固定、由胶囊裁剪（动画期间子元素重排是抽屉抖动的来源）；收起态 `inert`（被裁掉的输入框不进 Tab 序列）。收起条件是"指针离开 + 无焦点 + 草稿为空"，指针离开后再延迟约 100 ms，跨行扫过不频闪；**草稿非空或持焦绝不收起**。发送仍走级别 1（`sendGroupText(chatId, text, [], rowId)`，`rowId` 是最新一轮**引用行的 `meta.message_id`**（`round:…`），不是块 ref 串），claim / 等待行自动成立——与工具授权卡、pause 卡的"继续执行"（借道主 composer）是并存的三条路径，彼此不动。`@` 引用复用第二级 picker 的引用材料通道：把 `{agent_instance_id, session_id}` 推进主 composer 已有的地址状态（`ChatRefPick` → 可删 chip），上限仍是服务端的 1–5（第 6 条给提示而非静默丢弃），重复引用幂等；级别 1 的回复自带会话坐标，因此点击 `@` 先释放回复目标（两道指令不同乘一次发送）。复制为整块的 Markdown 导出：会话名 + 每轮"问 / 答"，**含块内折叠的轮次**，不可读的轮次写一行"无法读取"，绝不编造内容；答案正文原样粘贴，成功以图标短暂变对勾反馈。`@` picker 的"去续投"与用户气泡的来源 chip 点击仍走同一条 locate（经 `keyOfSeq`/`scrollToKey` 定位该会话块锚点 → 置一个上升沿 focus 信号 → 抽屉展开并聚焦输入框，先展开、下一帧再聚焦）；窗口外 locate 失败只提示"任务块不在已加载历史"，v1 不自动翻页。Mobile 无 hover，保持块内底部输入行不变（§6.5：折叠的运行不展示输入），本修订不涉及。

**来源 chips 是行内呈现、不是块认领（v3.2 修订二）。**一条 `user_message` 钉好的 `meta.refs`（四元组坐标）在其用户气泡上渲染为一排来源 chip，点击 = 同一条 locate。它属行内呈现，`chatTranscriptGroups` 的折叠不参与（材料认领一条轮，不拥有一个块）；坐标只读行 meta，无新读。

**点击权限按"是否该 Chat session 的参与者"判定，不按 owner。**群聊的存在理由就是成员共享同一份工作上下文，因此**群聊触发的 execution session 对该群聊全体参与者可读**——否则成员看到的是"Agent 在干活"却永远无法知道它卡在哪，群聊退化成给 Agent 发工单。单聊里用户本就是 owner，行为不变。判定依据是**派生，不是新表**：群聊 execution session 由 routing key `chat:{chat_session_id}` 创建（§5.4.1），该 key 已持久化在 Runtime `sessions` 表的 `intent_key` / `intent_closed` 两列（v3.1：`intent-sessions.json` / `terminal-sessions.json` 已退役，ADR-106 D11），因此"这个 execution session 属于哪个 Chat session"是已知事实，读权限 = 该 Chat session 的参与者集合。不引入独立 ACL 存储。**不得使用现有 `SessionVisibility::Public`**——它语义是"任何已认证用户可读"（[session_meta.rs](../../../core/acowork-memory/src/session_meta.rs)），是平台级可见，会让非群成员读到群执行历史，比原问题更糟。具体建模留实现设计（§10.20）。

**agent-agent 会话的执行记录不对界面外的人类开放。**A 与 B 的共享会话没有人类参与者，用户 A 可点开 A 侧的执行，但**不得**借此打开 B 的 execution session——那是 B 的私有执行记录。

**历史行始终可点。**带跳转的行指向当轮实际执行的物理 session；执行会话长命，其 JSONL 一直留在 Agent home，因此点击历史行打开的是**当时那一轮**，不是"当前会话状态"。这是期望行为：它是"这条消息产生了哪一次执行"的凭证。会话被 close（PM task 终态）后其历史同样可读。

**与 §5.5.4 的分工。**块里的卡是"要人做决定"，阻塞，必须有人答；等待行是"这条 POST 还没回来"，不阻塞，不需要任何人做任何事——但它**不是对话内容**，只活在当前窗口，不落盘、不进历史。一轮之内两者可同时存在（执行中触发审批）。1:1、人类群聊 @、PM 任务派发三条链路共用同一套行类型契约与同一渲染器（Desktop 的 `ChatExecutionBlock` + `ChatNoticeEventRow`，Mobile 的 `ChatExecutionBlock` + `ChatNoticeRow`），读起来是同一种语法。

**「事件文本」的落点语法：按事实的归属选一条轴，只有两档（v2.6 固化）。**chat 流里那类灰色小字——既不是哪位参与者说的话（气泡）、也不是控件——统称**事件文本**。它落在哪里不由渲染器决定，由**这条事实属于谁**决定：

| 档 | 事实归属 | 落点 | 现有个例 |
|---|---|---|---|
| **轮次级** `turn` | 不属于任何一行：整轮的结果、在途等待、通道回执 | **居中、不带图标**——它替整段转录说话，没有可依附的行 | 块的加载 / 空洞 / 不可达占位行、派生等待行（仅本设备 claim 未结算）、`budget_exhausted` / `truncated` / `unavailable` 通道回执 |
| **行级** `row` | 欠投递的那条人类行 | 贴在该行的列里（自己的行靠右、别人的行靠左），带一个指回上文的箭头 | 投递失败实时告警 `delivery.alert`（`DeliveryAlertLine`，ADR-105 D5 修订；到期自删，不是收据行） |
| **行级** `row` | 属于**某一行**，占那一行原本的槽位 | 该行的**内容列**（`ml-12`，与气泡正文同一缩进）**并带 kind 图标** | 暂无活跃个例（闭卡的"未获应答…"注记随卡片归块退役，v3.1）；新增行级事实时才用这一档 |

**两档必须一眼可分辨，判据只有一条：有没有图标。**有图标 = "这是关于某一行的事实"；无图标 = "轮次 / 通道在说话"。这条区分不是装饰——同一句"未获应答"会逐张卡片出现在交错的卡片流里，没有这枚图标，读者无法判断它是在说紧邻的这张卡，还是在说整轮；上一版两档的字号颜色完全相同，位置差异于是读不出含义，只读成"放歪了"。

**落点只有一个实现入口。**共享渲染原语 `ChatEventText`（`anchor: "turn" | "row"`）承载这两档；既有渲染器（`ChatTurnStatusLine`、`ChatNoticeEventRow`、以及块内的不可达 / 空洞占位行）全部委托给它，`InlineInteractionCard` 与其闭卡注记分支已随卡片归块退役（v3.1 / ADR-106 D8）。新增一条事件文本时**必须**声明它陈述的事实属于哪一档，不能再各自手写 class 串——那正是两档漂移回原状的路径。

**不属于本语法**：翻页带（"载入更早" / "跳到最新"）是列表级 chrome，恒居中；执行面的 iteration-limit / 错误提示是带边框的 banner，另有一套外观。

**同一族但另一条轴：气泡自身的元信息**（引用行、时间戳、"查看执行"）不与事件文本争位——它**贴气泡自身的外缘**（agent 气泡贴左缘、人类气泡贴右缘），落在气泡宽度之内，因此窄气泡与宽气泡下的位置一致。判据的唯一定义在 `bubbleMetaAnchor(side: "own" | "peer")`，由 1:1、群聊、执行面四处气泡列共同读取。

### 5.6 PM 集成与身份

PM 派发任务是项目 owner 通过项目群聊向 Agent 发消息：

1. PM 项目有明确 owner；
2. `acowork-user` / 受信任服务从已认证项目数据确认 owner 身份，PM 不得任意自报 owner；
3. 项目群聊 Chat session 是 Intent 的来源会话和 Agent 回复地址；
4. 项目可选挂一个代码仓库。有仓库时，PM 派发和项目群聊 @Agent 生成的 Intent 都由服务端注入该仓库地址作为 `repo`；注入发生在服务端，不依赖发送方填写，发送方也无法伪造项目仓库；
5. task ID 与目标 Agent instance ID 决定 Agent execution session；
6. PM task 后续派发消息复用该 task execution session，新的 task 使用新 session；
7. Agent 的回复回到触发它的那一轮，项目群聊里只留这一轮的 `execution_round` 引用行（v3.1，ADR-106 D12：不再是群聊中的一条 `agent_message`）；
8. PM 场景的对话 owner 是派单这条人类输入：Agent 的 response 路由回发起方那一轮、**不唤醒任何 Agent**；task 状态由 Agent 自己调 `pm_submit_task` 推进（`acowork-pm` 明确"聊天回复本身不构成交付"），不是 PM 服务去读群聊消息；
9. Agent 若需要其他 Agent 掌握的信息，写进 response 交回发起方判断，不由应对方转问（§5.4.5 R5）；任务间的先后与接力由 PM 的任务依赖图和 owner 决定，不由 Intent 承担编排。

群聊存储在 `acowork-user`，不代表 PM 服务失去项目 / 任务领域所有权：PM 仍拥有 task 与项目业务数据，聊天服务拥有会话与消息记录，二者通过稳定 ID 关联。

### 5.7 投递、错误与可靠性

Intent 保持消息语义，不引入通用任务状态机；最低限度的可靠性语义包括：

- `message_id` 跨聊天记录、Intent 路由和回复关联保持稳定；
- MQTT QoS 1 发布成功不等于 Agent 已处理或完成；
- 接收侧按 `message_id` 去重，防止同一消息重复进入 execution session；
- response 由系统 push 回发起方，发起方不以查询方式等待结果（§5.5.1）；
- 单对话 request 次数超限时，系统拒绝投递并落一条 `system_notice` 通知，不静默丢弃（§5.4.5 R4）；
- 目标 Agent、来源 chat session、session 配置无效或 `repo` 地址格式非法时明确失败；
- Agent 试图在授权目录集合之外读写，或因 `repo` 触发自动 clone / 建 worktree 时，操作被拒绝并回执，不静默降级；
- 工具反馈只说明消息被记录 / 接受或明确失败，不把"已发布"伪称"已完成"。

建议至少区分 `invalid_repo`、`unauthorized_directory`、`target_not_found`、`source_chat_session_not_found`、`session_config_invalid`、`not_authorized`、`delivery_unavailable`、`conversation_request_limit_reached`、`response_unavailable`（应对方 turn 异常终止时随兜底 response 携带）和重复 message ID。正式错误码在 HTTP / MQTT 协议修订时统一。

**接收侧验收回执（v3.2）。** 「MQTT 发布成功」只证明 broker 收到了字节，**不证明接收 Runtime 能把消息路由进执行会话**，而后者会失败：2026-11-04 的事故里，目标 Runtime 的 session-meta 库缺 `intent_key` 列，`route_intent_inbound` 的查询报 `no such column`，消息只留下一行 WARN 就被丢弃；与此同时 Gateway 把信箱记成 `delivered`、PM 把 `dispatched_at` 盖章，于是这条任务既不执行也不会重试——派发链路两端各自"成功"，中间的失败没有任何人能看见。因此接收侧补一条**回执**（`POST /api/intents/{message_id}/accept`，协议面见 `protocols/http.md` §4.16.4）：

- Runtime 处理完一条 request（`kind != response` 且有 `message_id`）后 best-effort 回报 `{sender, accepted, error}`；`accepted=false` 时 `error` 是路由失败原文，不归一化（它就是这次故障唯一自述）。回报在派发循环之外 `spawn`，不阻塞下一条入站消息；失败只打 debug —— 诊断通道不得自己变成重试循环。
- Gateway 只接受**记录在案的 target** 的回执（D4 身份校验 + `to_instance` 比对，其余 403），写入信箱 `acceptance{at, status, error}`，并**唤醒**正在等该 `message_id` 的派发方（进程内 oneshot，事件驱动）。`GET /api/intents/{id}` 暴露 `accepted`（`null` = 无回执，**不等于**拒绝）、`accepted_at`、`accept_error`。
- 只有 `POST /api/intents/task-dispatch` 等回执，且有上界（`INTENT_ACCEPT` = 5s）：`accepted` → `delivered`；`rejected` → `status=failed` + `error_code=intent_rejected` + 原文，PM 落 `dispatch_error` 后由既有退避自动接管；**无回执（旧版 Runtime / 超时）→ 沿用发布即成事的旧语义**，混版部署不回归。通用 `/api/intents` 只记事实、不等待，agent 间投递不为它加延迟，也不引入任何轮询。
- 回执是**每次尝试**的判定：同一 `message_id` 重新派发（PM 按代次重放）清空上一轮 `acceptance`，`delivered_at` 作为历史事实保留。两者本就允许不一致——这正是本次事故的形态。

不变式：`delivered_at` 是 Gateway 的发布事实，`acceptance` 是接收侧的受理事实，**互不覆盖、互不推断**；"无回执"只表示"不知道"，任何一方都不得把它当成成功或失败。

## 6. 前端交互设计

- **Agent 页标签栏 = 固定 Chat tab + execution session tab**：打开 Agent 必然落在该 pair 唯一的 Chat session（首次打开时幂等创建），tab 标题 = Agent 名，不可关闭（v2.1 微信模型）。execution session 是同一标签栏里可随意开关的普通 tab，`+` 新建的是 execution session tab。现有 Session history 保持原行为。
- 单聊输入框的附件、@、卡片回答能力与群聊完全一致——同一组件、同一套消息类型，仅按 kind 隐藏不适用的控件（如单聊无需 @）。
- 其它 kind 的 Chat session（agent-agent、群聊）从各自业务视图进入，点击后展示 Chat session 消息；Agent 页不再设 Chat history 下拉列表（v2.1 撤销）。
- PM 项目视图进入项目固定群聊；PM task 派发的 Intent 与回复在群聊中按序可见。
- 群聊普通消息不触发 Agent；@Agent 生成定向 Intent。多个 Agent 被 @ 时分别显示独立请求和回复。
- 聊天历史展示消息和必要的投递提示，不展示 Agent 私有思考、工具调用或执行过程。
- Chat session 视图需展示对话归属：每条对话的 owner（发起方）、request 计数与 response 状态（`completed` / `aborted` / `failed`）。
- request 次数触顶时必须显示明确的限制回执与"继续"入口——点击后由**用户**发一条新消息开新对话。不能让用户只看到"两个 Agent 突然不说话了"。

### 6.1 主视图模型

统一为 Chat session + Execution session 两层后，Agent 页只有一个统一标签栏（v2.1）：

```mermaid
graph TB
    AP["Agent 页面"] --> BAR["统一标签栏"]
    BAR --> CT["固定 Chat tab<br/>标题 = Agent 名，不可关闭"]
    BAR --> ET["Execution session tab<br/>可随意开关"]
    CT --> MSG["消息流<br/>user_message / agent_message / execution_round（块）/ system_notice"]
    CT --> INPUT["输入区 + 附件"]
    MSG -.->|"点块头'查看执行'跳对应 tab"| ET
    ET -.->|"关闭最后一个回落 Chat tab"| CT
```

精确规则（v2.1 微信模型，避免「不可关闭」被过度解读）：

| 对象 | 规则 | 理由 |
|---|---|---|
| **Chat tab** | 固定在标签栏最左，标题 = Agent 名，**不可关闭、不可删除** | 打开 Agent 必须有个可说话的地方；删掉就回到「无会话可输入」的老问题 |
| **Execution session tab** | 普通 tab，**可随意开关**；关掉最后一个回落到 Chat tab | 多会话并行由执行层承载（ADR-086 决策 8） |
| **两个表面共存** | 切换表面只切换活动 tab，**不销毁对方 tab** | 「打开 chat 关掉所有 exec tab」的互斥旧模型已于 v2.1 撤销 |
| **单一焦点** | 任意时刻标签栏恰好 **一个** tab 高亮：谁在前台谁是焦点。chat 表面在前台时，执行层记录的 active session 保持不变（翻回 exec 时即恢复），但其 tab **不高亮** | 两层状态各自驱动高亮会出现双焦点，v2.1 修正 |
| **图标区分** | Chat tab（及 NavBar）= 聊天气泡，**仅两态**（选中实心 accent / 未选中描边），**不参与呼吸**；Execution session tab（含历史下拉行）= **brain 图标**，四态矩阵（描边/实心/呼吸/accent）形态不变，只换形状 | 两个表面共用气泡没有区分度；气泡专属 Chat 表面，brain 表达「agent 正在思考/执行」；chat 回合必然跑在某个 execution session 里，其 brain 图标已在呼吸，Chat tab 再跟着呼吸只是噪声 |
| **Agent 未启动** | Agent 页维持旧启动屏交互（大 Play 按钮 + 空白聊天背景），**不显示统一标签栏与 Chat session**；启动后才进入标签栏形态 | Chat session 消息没有 Runtime 无法得到回应，未启动时显示它没有意义 |
| **同 pair 第二条 Chat session** | **不存在** | 每个 (user, agent) 恰好一条，无新建/列表入口 |

Execution session 不再是对话的默认入口，但也不降级为"只能通过跳转进入的后台会话"：它是同一标签栏里平权的一等 tab，Session history 仍是并列入口。

### 6.2 等待行与块的映射（v3.1 修订，ADR-106 D3 / D5 / D9）

chat 侧不读 execution session 的 `live_state`，也不为轮次落任何中间状态行。转录里只有一种"这一轮"的形状：引用行渲染成块，内容来自投影读。等待行是界面自己那次发送尚未在转录里落点的反馈，与轮次无关（§5.5.5；v3.1.2 把"尚未返回"改成"尚未落点"）。

| 事件 | chat 呈现 | 语义 |
|---|---|---|
| 本设备那次发送已交接、转录里尚无落点（claim 未结算） | 居中等待行 + shimmer，composer 锁定（上一句未落点前发不出下一句） | 只声明"我这句话还没人接住"；POST 已返回也算，它不宣称 agent 在忙，300 s 无落点即自动释放 |
| 引用行落地、投影读到 `pending` | 块头状态词"执行中"（`card != null` 时另有一行卡片） | 这一轮在飞，由执行侧说 |
| 投影读到 `ok` 终态 | 块内答案内联 + 终态状态词 + 块头"查看执行" | 收尾判据是投影的 `terminal`，不依赖 `Idle` |
| 投影读到 `not_found` | 显式空洞（"本轮执行已不可用"） | 坐标失效 / 旧会话无轮次索引，不编内容（D5） |
| 投影读到 `unreachable` | 故障形状 + "重试" | 主机不可达是一次故障，结对不画成进行中（D9） |
| 轮次终态 / 停在卡片（`round_state` 门铃） | 命中已挂载的块 ⇒ 立即重读投影（带一次 500 ms 有界重试） | 事件只说"值得现在重读"，正文仍来自投影（D3 / D4）；它不建立 liveness |
| `system_notice`（预算 / 截断 / 寻址问题） | 居中通道行；寻址问题带选项，旁观者只见问题 | 通道事实与寻址决定（D10） |
| `delivery.alert`（行上装饰，不是行） | 该行自己列里的一枚短线，到期消失 | 投递失败这一瞬间的状态（ADR-105 D5 修订） |
| 旧的 `turn_dispatched` / `turn_started` / `turn_routed` / `router_fallback` / `interaction_request` / `interaction_resolved` 行 | 不渲染（历史 `agent_message` echo 仍按气泡呈现） | 历史里的中间状态一律隐藏，旧卡不复活（§5.5.5） |

收尾判据是**投影的 `terminal`**，不是 Runtime 的 `Idle`，也不再是聊天侧的终态行：`Idle` 只说明"执行结束"，不说明"结果已可读"。工具授权、迭代上限 / 环路暂停的 Continue 等 execution 阶段动作在块里就地可答（§5.5.4），要细看过程仍从块上方"查看执行"进入 execution 视图。

**"块什么时候该重读"是 v3.1 零写删掉的通知面**（v3.1.3，ADR-106 修订 2026-11-03）。Runtime（`close_turn_round` 后，或活回合内卡片开合时）经既有 chunk 通道发一枚门铃：`acowork/agents/{instance_id}/sessions/{sid}/messages/round_state`（`SessionMessage{round_state = 34}`，QoS 1、**非 retained**），载荷只有两个坐标 `round_id` / `chat_session_id`，没有正文、也没有终态词——投影不开新通道，也不因此多出第二块 live 真相（ADR-106「被拒绝的备选」那一条守的是内容，不是信号）。三个约：

- **它是触发，不是内容。** 事件在**两个**时点触发：回合边界，以及活回合内卡片打开／收回（v3.1.3 补，ADR-106 修订 2026-11-05）。两种情况下轮次记录都是会话单写者的另一次 append（ADR-092，实测晚约 8 ms），所以读侧允许一次有界重试（500 ms），并把"读到还是 `pending`"当正常。`round_state` **不**进入内容事件集：它的 `session_id` 是执行会话，不得被当成一个对话会话写进消息窗口。
- **它也管非终态翻转，且两段都要管。** 回合**末尾**停在 `pause` 卡上的轮次走 `close_turn_round` 的那枚铃；`ask_question` / `approval` 则在回合**内部**开合，走 `publish_round_card_state` 那枚铃（实测缺陷：一张活了 54 s 的问题卡在 Chat 侧一次都没被读到，因为两端的时点都不经过回合边界，而 webview 被遮挡时 15 s 兜底会被计时器钳制吃掉）。两种情况块重读后都可能读到 `waiting`，必须把问题画出来并**继续**显示这一轮在进行中；收回那一端同样要打铃，否则块会留着按钮指向一个已经答过的问题。
- **兜底仍是周期重读，但只是兜底。** 15 s 的 `setInterval` 只负责门铃丢帧时收口（它从 v3.1.3 起才真正会重新装配自己）。订阅面：桌面已有通配 `acowork/agents/+/sessions/+/messages/#`，零新增；移动的门铃走 presence 通道（§6.5）——`realtime.ts` 的单会话精确订阅不变量不为它开口子，而事件非 retained，订阅时不会全量重放。另：15 s 兜底与门铃并存不违反设计 25 §8.0 的"push 与 poll 不得同时驱动重载"——那条管的是消息窗口的 `chat/changed` 门铃，这里说的是块投影的重读，两个对象。

载荷为什么不带终态词：没有订阅方会在不重读的前提下用它（每个订阅方都要拿到正文才画得出块，终态词随重读一起回来），而 proto3 之后要加字段是免费的——现在带上一个投机字段，只是把一个公共契约（以及手写解码器那份不得不跟着维护的诚实）放宽给一个不存在的读者。所以门铃只说"哪一块、哪个 Chat"。

"进行中"的判据仍然是读回来的 `terminal`（D9）：门铃不建立 liveness，也不作为证据位——否则它就变成了 v3.1 刚刚废掉的那种东西。

### 6.3 块内卡片与等待行规范（v3.1 修订）

| | 决策卡（块内的卡片行） | 在途等待行（运行期派生） |
|---|---|---|
| 是否阻塞 | 是，Agent 本轮持有等待或结束本轮（§5.5.4） | 否，纯展示 |
| 持久化 | **不落 chat**；执行侧 `track` 时进入投影的非终态 `card` 快照 | **不落盘**，仅存在于当前窗口 |
| 未决判定 | 投影 `card != null` 且 `terminal=false` | 本面那次 POST 尚未返回 |
| 过期 | 读时求值（不落库改状态） | POST 返回即消失 |
| 可操作性 | 按钮 / 选项 / 输入（块里就地可答；只有发起该轮的人看得到按钮；块折叠时卡片仍在块级可答，D8） | 无；块上方有"查看执行"跳转（受权限约束） |
| 无人类参与者时 | **不创建**（§5.4.5） | 仍可显示——它说的是自己的写，不是 agent 的忙 |

跳转不可用时置灰而非隐藏——引用行的坐标对全体 chat 参与者可读，可点性由执行侧读权限决定（§8.2 / ADR-106 D10）。

### 6.4 群聊特有交互

- **@ 选择器（v3.1：两级 `@`；v3.1.1 补单聊入口，ADR-106 D6 / D7）**：第一级 @ 决定哪个 Agent 触发执行；第二级 `@` 指名该 Agent 的哪一个任务（`mentions.target_sessions`，与 `force_new_for` 并列）。@ 到人仅产生提及，不触发（§5.3.1）。**群聊与单聊的 composer 都有 `@` 按钮**（一级只列 agent / 成员，不做级联子菜单）：单聊只有一名 agent，因此一级退化，但手势两端一致——地址是人给的指令，不能靠模型替人决定，也不需要两套语法。**v3.1.2：单聊输入 `@` 也直接打开任务列表**（与 `@` 按钮同一路径；`@` 不写入正文，因为单聊的地址走 composer 上方胶囊而非文本，残留的 `@` 会变成模型要读的噪声；词中的 `@`（邮箱）按一级规则不劫持，IME 组合期内同样不拦截）。单聊 placeholder 因此从「Enter 发送，Shift+Enter 换行」换成提示 `@` 选任务——按键机制是 IM 常识，本表面真正提供的能力却没有任何提示。
- **多 Agent 并发**：同一消息 @ 多个 Agent 时，各自独立地跑一轮（chat 侧只有一条群级等待行，不逐个点名）。
- **成员变更**：被移出群的成员，其视图中的历史群消息保留，但群 execution session 立即不可达（§8.3）。
- **任务胶囊（v2.8；v3.1 移至块头）**：PM 派单的机器坐标（`meta.task_id`）随**引用行**渲染在执行块的块头（ADR-106 D3 / D13：坐标属于那一轮工作，不属于那句话），派单 `user_message` 行只带人读正文；唤醒 playbook 不上任何聊天表面（§5.4.2）。`project_id` 不进 meta，也不渲染——行本就活在该项目的群里。

### 6.5 移动端

移动端（ADR-086）与桌面共用同一视图模型与同一套行词汇表（ADR-106 D1 的四类 kind，含 `execution_round` 引用行），差异仅在呈现密度：**一个执行会话折叠成一块**（布局层同样按 `agent_instance_id|agent_session_id` 分组，与桌面共用同一套分组规则；块头 = 头像 / 名字 / 会话名 / `N 轮 · 状态词` / 任务坐标），最新一轮的答案内联，等待中的卡片收成一行、按钮走底部弹层，且**块折叠时卡片仍在块级可答**（D8 / §6.5 的误触考虑只改形状，不改可答性）；第二级 `@` 选址也走底部弹层。块头的活线只在"本块会话就是前台被监视会话"时才写——前端的 `agentStates` 不覆盖后台会话（D9）。**v3.1.3：轮次门铃不走 `realtime.ts`。** 移动的单会话精确订阅不变量（设计 25 §8.0）不为一个跨会话的信号开口子，所以 `round_state` 搭本就通配订阅的 presence 通道（`agents/+/sessions/+/messages/round_state`），命中已挂载块才 reload；事件非 retained，订阅时不会全量重放。线格式是手写的（`proto-wire.ts`），所以字段 34 靠 Rust 侧单测钉住。发送→回复之间的空窗仍只由本地 `sending` 态派生一条等待行，不从任何中间状态行推导（ADR-106 D9）；旧的 `turn_dispatched` / `turn_started` / `router_fallback` / `interaction_resolved` 行不迁移、不删除，在渲染期一律隐藏——两端共用同一套隐藏集合。本节取代 v2.4 里"移动端同构于居中状态行"的表述。

### 6.6 会话内历史查询与消息定位（ADR-097）

**入口与形态。** Agent 单聊与群聊面板顶部各有一个「聊天记录」入口，打开同一个 `ChatHistoryDialog`：`文本 / 图片 / 文件` 三个类目共用 `GET /chat-sessions/{id}/search`。文本类目走 FTS trigram（短词回退 `LIKE`），图片 / 文件按 `cat` 位掩码列出，文件类目再按 filename 子串过滤。

**查询只读侧表。** 对话框只读 `search.sqlite`（ADR-097 D4/D5），`before_line` 是向更旧翻的游标，满页时返回 `next_before_line`；`total_lines` 是权威物理行数，前端据此把命中的 `line_pos` 换算成 `read_messages` 的 offset。因此打开与翻页都不扫 `messages.jsonl`。

**定位是窗口重锚，不是整段加载。** 点击命中后，前端以 `line_pos` 为基准取「命中行上方半页」（`PAGE/2` 行 lead）的一页，把窗口模型从队尾页改成双向游标 `firstPos/lastPos`，滚动到对应 `data-seq` 锚点并短暂高亮。读者停车在中段时，门铃 `refresh` 只更新 `total` 与 in-flight 投影，不把人拽回队尾；发送消息或重新打开会话走 force refresh，才回到队尾。滚动到两端分别触发 `loadOlder` / `loadNewer`，加载文案分别是「加载更早消息…」与「加载更新消息…」。

**边界。** 本期定位为桌面端先行：移动端仍是队尾单向加载，后续按 ADR-086 的移动交互补齐；查询权限与读消息共用同一条参与校验，非参与者与未知会话一律隐藏 404。

### 6.7 i18n

既有键位可直接复用：`sessionTabBar.closeChat` / `closeOtherChats` / `deleteSession` / `openNewSession`、`sessionHistory` 历史入口。`chatHistory` 命名空间随 §6.6 的历史查询对话框重新启用（`title` / `close` / `tabText` / `tabImage` / `tabFile` / `loading` / `loadingEarlier` / `loadingNewer` / `empty` / `error` 等），新增入口级键位限定在该命名空间内；中英语言文件必须同批同步。

## 7. 架构边界与现有实现

### 7.1 职责边界

- **`acowork-user`**：拥有全部四种 kind 的 Chat session（含 user-agent 单聊）、参与者关系、固定类型聊天消息及唯一串行写入口；提供聊天历史分页查询。会话记录一律由聊天域拥有，不按 kind 分裂存储路径——按 `userId/chats/{peerId}` 一类"每个发起者一个目录"的布局存单聊，会与 Chat session 的 UUID 会话 + IntentMeta 布局并存成两套系统，同一语义两处权威，正是本设计要消除的。
- **PM 服务**：拥有项目与任务数据（项目可选关联一个代码仓库）；项目创建时请求创建项目群聊；派单时提供经验证的 owner / project / task 关联及项目仓库地址。
- **Gateway / 可信路由边界**：验证 Runtime / PM 身份、解析目标、注入并校验 `repo` 地址与 `chat_kind`、路由 Intent、按 `conversation_id` 统计 request 次数并拒绝超限、把 response push 路由回发起方的 execution session，并协调 user chat 与 Node / Runtime；不直接访问 Node-local Agent home 文件，也不承载"该由哪个执行会话接话"的语义判断（v2.2，ADR-099）。
- **Agent Runtime**：按入站消息类型判定本轮角色（发起方 / 应对方）并据此裁剪 tool set（§5.4.5）；对人类消息先跑 Chat Router（无状态、工具集仅 `dispatch`、失败降级回旧路由），在候选集内选择回答 / 复用 / 新建；将选中的消息作为指定 Agent execution session 的用户输入；在 Agent home 保持现有 session 格式；只在自身已注册 / 已授权的工作目录集合内读写，不因 `repo` 自动 clone 或建 worktree；turn 结束时由系统生成 response（含异常与中止的兜底形态）交回可信消息写入口，不依赖模型调用回复工具。
- **Desktop**：Agent 页固定 Chat tab 呈现该 pair 唯一的 user-agent Chat session，execution session tab 在同一标签栏共存；经 Session history 呈现执行历史；不自行拼接或持久化业务历史。

### 7.2 传输与 ACP

Intent 的消息语义不绑定 MQTT。MQTT 可继续承担 Runtime 控制消息传输；Chat session 读写经 `acowork-user` 提供的受信任 HTTP / 服务接口，实际接口需与 Gateway 反向代理、Node 远程拓扑和认证边界一致。

ACP 可参考其 Agent 能力描述、发现和结构化消息；本设计不要求内部消息采用 ACP REST API，也不采用 ACP run lifecycle。

### 7.3 现有实现需调整

当前 [intent_send.rs](../../../core/acowork-runtime/src/tools/builtin/intent_send.rs) 通过 MQTT control topic 发 Intent，且 `async` 参数不代表已实现请求等待 / 结果返回。实现新模型时应以本设计的消息和 Chat session 语义为准，复核或退役 Gateway 中依赖已移除 gRPC 的旧 Intent Router 路径，并更新 [06-communication.md](./06-communication.md)、[12-tool-system.md](./12-tool-system.md) 及 MQTT / HTTP 协议，形成唯一权威合同。

## 8. 安全与权限

### 8.1 身份冒充防护

`participants` 必须携带**身份类型**（human / agent），写入时按类型校验：

- `user_message` 只能由已认证的人类身份写入；Agent 不得写入该 kind。
- `agent_message` 只能由该 Chat session 的 Agent 参与者写入；`execution_round` 只能由 Runtime 的唯一 chat 写入口在轮次边界写入（v3.1，ADR-106 D1 / D3）。
- `system_notice` 只能由系统写入。

校验放在唯一写入口（§4.6），不放调用方——否则任何持有 Runtime 权限的 Agent 都能伪造人类发言。当前实现缺此校验（§3.5 P1）。

### 8.2 可见性模型

三层作用域，从窄到宽：

| 层级 | 判据 | 适用 |
|---|---|---|
| **owner** | 会话创建者 / 账号所有者 | 用户私有数据 |
| **参与者派生** | 是否为该 Chat session 的参与者 | 群聊触发的 execution session |
| **平台级 `Public`** | 任何已认证用户可读 | 明确公开的内容 |

关键区分：**群聊可见 ≠ `Public`**。`session_meta` 的 `Public` 是平台级作用域，按字面用于群聊会让任何登录用户读到群执行历史。群聊 execution session 的正确判据是从 routing key `chat:{chat_session_id}` 派生——读权限等于该聊天会话的参与者集合，**不新建 ACL 表**（§5.4.2）。

**v2.2：候选集是权限边界的一部分。**router 只能看到并选择候选集内的执行会话，而候选集已按 `chat_session_id` 绑定与 owner 硬过滤：1:1 只含该用户自己的会话，群聊只含本群绑定的会话。因此"可见性"与"可路由性"同源——不存在 router 把群消息投递到某成员私聊会话的路径，也不存在通过候选标题探测他人会话的路径（ADR-099）。PM task session 绑定项目群聊后为 Members 可见，与 §8.3 的回收规则一致：被移出群聊者同时失去对它的读权限。

### 8.3 权限回收

被移出群聊的成员，对该群既往 execution session **同时不可达**。执行记录含思考过程、工具调用与文件内容，敏感度高于聊天消息；聊天消息保留而执行记录失权，是有意设计的不对称，不是遗漏。

### 8.4 数据暴露面

| 暴露面 | 控制 |
|---|---|
| 执行记录（思考 / 工具 / 文件） | 参与者派生读权限 + hidden 404（不区分"不存在"与"无权"） |
| 卡片 | 不得作为枚举探针：无权时置灰而非隐藏，且不返回 403 细节 |
| 附件 | 随消息体，不单独可寻址 |
| Agent home 内部状态 | 仅经 Runtime HTTP 读写，Gateway 不直连文件系统（§7.1） |
| 预算计数 | Gateway 重启不得丢失（§5.7） |

## 9. 落地计划

### 9.1 分阶段落地建议

### Phase 1：Chat session 存储与消息写入

- 在 `acowork-user` 实现通用 Chat session / participant / message 模型，支持 project group、user group、user-agent 和 agent-agent 类型；
- 消息类型统一为 `user_message` / `agent_message` / `execution_round` / `system_notice` 四类，Intent 信息降为字段（§4.2）；四类都要求服务端分配的 `message_id`；
- 解封 `SessionKind::UserAgent`：放开创建入口的参与者校验 + `ensure_pair` 的 kind 参数化，并补参与者身份类型校验（§10.14）；
- 写回触发点改为"入站消息携带 `source_chat_session_id`"，与本轮角色无关（§5.5.1）；
- `pairs.json` 保持"每 pair 单条会话"语义即为正确形态（v2.1：每个 (user, agent) 恰好一条），无需改造；其余 kind 会话经 `list_for` 枚举（§5.4.4）；
- 落 `system_notice` 的 `notice_type` 分流（仅通道回执，§4.2.1）；
- 建立单一后端写入口、每 Chat session 串行化、顺序键及 `message_id` 幂等；
- 固化 Chat session 与现有 Agent execution session 的边界及关联引用。

### Phase 2：项目群聊与定向 Intent

- PM 创建项目时幂等创建唯一项目群聊；项目生命周期处理策略暂缓；
- 群聊 user message 只存档；@Agent 才写 Intent 并定向投递；
- 群聊按 `(chat_session_id, target_agent_instance_id)` 维护稳定逻辑执行映射，映射到的物理 execution session 长命、原地追加（§4.1）；
- `repo` 为可选字段，项目有仓库时由服务端注入；接收方只能在已授权工作目录内操作，缺失对应目录时明确回执，不自动 clone；
- 按入站消息类型判定本轮角色并裁剪应对方 tool set（`intent_send` / `ask_user_question` 不可见），实现 §5.4.5 R1；
- 按 `conversation_id` 统计 request 次数并实施上限（默认 6），超限拒绝投递并落 `system_notice` 通知；
- session 配置使用目标默认值或 Intent 初值；有权用户可以后续修改工作目录。

### Phase 3：PM task 与 Agent-Agent Chat

- PM task 按 `(project_id, task_id, target_agent_instance_id)` 映射专属 Agent execution session，回复回到触发它的那一轮、群聊里只留引用行（v3.1，ADR-106 D12）；该逻辑映射指向的物理 execution session 长命，task 终态时 close 映射——绑定与关闭状态就是 Runtime `sessions` 表的 `intent_key` / `intent_closed` 两列（v3.1，ADR-106 D11：终态清单文件退役）（§4.1 / §5.4.2）；
- response 由系统在应对方 turn 结束时生成（正文 = 本轮最终输出），四类结束形态均有终态（交给那一轮的投影，不再是 chat 回执行），并 push 路由回发起方；
- 实现 Agent-Agent Chat session 及双方各自 execution session 的关联；A-B 之间的 Intent 不再在共享 Chat session 里落行，留下的只是该轮在绑定 pair chat 上的引用行（§5.4.3，v3.1 / ADR-106 D12）；
- 提供 `chat_history_read(offset, count)`，实施服务端硬上限；
- Agent 页不新增 Chat history 入口（v2.1 撤销 v2.0 该项），保留原 Session history；其它 kind 的 Chat session 从各自业务视图进入。

### Phase 4：Chat Router 与语义路由（v2.2）

- Runtime 新增 `chat_router` 模块与 `ChatSessionManager`（每 chat 串行队列），人类消息走无状态 router turn，工具集仅 `dispatch`；
- 候选构造按 1:1 / 群聊两套过滤规则（chat 绑定 + owner + 终态排除），上限 10，只读 session meta 不读 JSONL（ADR-096）；
- 三段式上下文编排（Block A / B / C）、窗口锚点与 cache 断点按设计 29 实施；
- 三态轮转全部删除（v2.7；v2.2 曾只删 T3、把 T1 降级为 prompt 信号、T2 保留到 router 选定目标之后）：执行侧不再有物理轮转点；
- 群聊 @Agent 经过该 agent 的 router，多话题可并行；PM task 会话绑定项目群聊并进入候选，终态清单排除；
- 失败降级：chat 来源的人类消息**新建**执行会话（`chat_dispatch(Target::New)`，v2.5 / ADR-099 D8 修订），`PassThrough` 保持确定性复用；v2.4 起不再写 `router_fallback` 回执，v3.1 起降级与正常派发也**不再落反应行**——路由结论由引用行的块头呈现，永远与执行侧一致（ADR-106 D1 / D14）；决策日志 `chat-router-decisions.jsonl` 可审计；
- 前端补 origin 标识与在途等待行（§5.5.5）；显式新建不新增 composer 按钮，而是复用会话栏的 `+`（创建即绑定 chat，ADR-099 D9）。

---

**核心决策总结：**

- Chat session 与 Agent execution session 是两种不同模型；前者由 `acowork-user` 管理聊天消息，后者保留现有格式并由各 Agent home 管理执行上下文。
- Chat session 消息类型固定为 user message 与 Intent message；思考、工具调用和工具结果只存在于 Agent execution session。
- Agent-Agent 对话有三个 session：一个共享 Chat session，加 A、B 各自的 Agent execution session。
- 群聊普通消息不触发 Agent；@Agent 才发送 Intent。群聊 @Agent 使用每 Agent 每群聊稳定逻辑映射，其物理 execution session 长命、原地追加；PM task 使用每 task、每 Agent 专属逻辑执行上下文，只在 task 终态时 close。新开一条执行链一律是显式决定（§4.1）。
- Intent execution session 沿用 70% 告警、80% LLM compaction、95% emergency trim；执行会话不做物理轮转（v2.7，T1 / T2 / T3 全部删除）——上下文卫生由 compaction 阈值单独负责，文件规模由既有 JSONL 硬上限兜底。不设执行次数上限：上下文污染已由 compaction 阈值直接覆盖，不存在无限膨胀路径。开新执行会话只有三个显式来源：router `dispatch(new)`、PM task 终态 close、用户经 `+` 手动新建。
- **人类消息的路由是 LLM 语义决策（v2.2，ADR-099）**：每条 1:1 / 群聊 @ 消息先经过目标 Agent 的 Chat Router（无状态、工具集仅 `dispatch`），产出回答 / 转交已有会话 / 新建会话；router turn 在 per-chat drain task 上顺序执行，不占用 MQTT 入站循环；执行会话没有代码级轮转点（v2.7），配置差异也不触发新建；复用到的会话也会拿到 draft 配置（§4.5）。PM 派单与 agent-agent 保持确定性映射，不经 router。
- **已废止（v2.7）**：Handoff 分级（T1 / T2 两档）与 Session 设置卡片的两个轮转配置项随轮转机制一并删除，卡片其余项不变。新建执行会话不携带任何旧话题摘要，其上下文只有所属 Chat session 的对话记录；compaction 阈值与 JSONL 硬上限仍不开放给普通用户。
- PM 项目群聊由 `acowork-user` 保存，项目与群聊生命周期联动暂不处理。
- 每 Chat session 由后端唯一写入口串行写入；Intent 配置作为初值，授权用户可在之后修改。
- Intent 用 `repo`（可选、服务端按项目注入的仓库地址）而不是 `workspace` 表达工作对象；`workspace` 一词在本文只指 Agent 侧已注册工作目录。工作区如何选择交给接收 Agent 的模型，协议层不做绑定表或选择算法；硬约束是只能在已授权目录内读写、不得因 `repo` 自动 clone 或建 worktree、无对应目录时明确回执。
- `chat_history_read` 用 offset/count 分页读取有权访问的聊天历史，服务端设硬上限；它不用于获取本次对话的结果。

**对话所有权与结果送达：**

- 一次对话由发起方拥有并结束（C/S 式）。只有被人类输入触发的 turn 能发起对话；应对方 turn 的工具集合里没有 `intent_send`，也没有 `ask_user_question`。应对方无法延长对话，因此不存在无限循环，不需要因果链深度或跨 agent 环路检测。
- Intent 只有 request 与 response 两种语义：`intent_send` 只产生 request；response 由系统在应对方 turn 结束时生成，正文 = 本轮最终输出（不是"从上下文末尾取一段"），异常、中止、崩溃均有兜底回执。
- 发起方通过 push 获知结果：response 落库后路由为发起方的入站消息（执行中则注入当前 turn，空闲则唤醒新 turn）。禁止以查询方式等待结果——LLM 不具备等待能力，轮询等于 turn 内 busy-wait。
- 唯一的成本闸门是单对话 request 次数上限（默认 6，Agent 级可配）；人是唯一切断点和重启点，不提供应对方申请提高上限的能力。
- 编排（谁接着做、按什么顺序）归人 / PM 任务依赖图，不归 Intent：应对方需要第三方信息时写进 response 交回发起方；群聊与 PM 场景的 response 只路由回发起方那一轮（v3.1 起不再是群聊里的一行，ADR-106 D12），不唤醒任何 Agent。

## 10. 开放的实现问题

核心语义已确定，以下是实现细节而非待定产品原则：

1. `acowork-user` 内 Chat session、participant、message、Agent execution session 映射的具体存储 schema；应使用一个可扩展的统一模型，参与者类型和 chat 类型可扩展。
2. `acowork-user` 唯一写入口和每 Chat session 串行队列的进程内 / 持久队列实现、崩溃恢复和事务边界。
3. PM 创建项目与 user 服务创建项目群聊之间的幂等创建、失败恢复和删除 / 保留策略。项目归档或删除策略本阶段不展开。
4. 有权用户修改执行中的 Agent session 配置时的 API、审计事件及在途操作配置快照实现。
5. （v2.7 作废）原「首版 session 轮转参数」不再是实现问题：轮转机制与 `intent_idle_rotate_hours` / `intent_handoff_max_tokens` 两配置项已删除，Session 设置卡片不新增 Intent 轮转项。
6. 物理 session 逼近 JSONL 硬上限时，在执行中的安全检查点停止追加 / 明确失败并保留已写历史的具体处理仍需实现细节（v2.7：successor / handoff 的事务边界与"双 successor"防护随轮转删除，不再是实现问题）。
7. `chat_history_read` 的硬性 count / bytes 上限、输出 schema 和 prompt 中建议的软上限。
8. Agent-Agent Chat session 首次创建入口，以及 Chat session 与双方逻辑执行映射 / 物理 execution session 链的持久化位置。
9. 群聊 mention 的 ID 表示、消息展示与多个目标的回复排序 UI。
10. `repo` 的规范化规则（remote URL 归一、host 小写、凭证剥离、`.git` 尾处理）与 PM 项目关联仓库字段的存储 / API；项目多仓库场景延后。
11. response 正文的字节 / token 双限具体数值，以及 `truncated` 标记在 Chat session schema 中的表示。
12. 应对方 turn 异常终止的兜底实现点：Runtime 崩溃或未产出任何结束事件时，由谁、在何时、依据什么判据写出 `status=failed` 的 response（发起方不轮询，因此该兜底必须存在）。
13. `conversation_id` 的 request 计数存储位置与持久性：Gateway 重启后不得丢失对话预算，否则上限可被绕过。
14. **user-agent 单聊的可达性解封**：`SessionKind::UserAgent` 当前在创建入口被参与者校验拒绝（多参与者仅允许 user_group / user_user），且 `ensure_pair` 将 pair 会话 kind 硬编码为 `agent_agent`——两处改动即可解封，但需同时补参与者身份类型校验，否则 Agent 可冒充人类写 `user_message`。
15. **Agent 页面 Chat history 的鉴权路径**（v2.1 随入口撤销关闭：Agent 页不再有 Chat history 列表，user-agent 单聊经 pair 寻址的 default 会话 API 读取；其它 kind 会话的读取仍受 §8.2 参与者作用域约束，鉴权模型本身不变）。
16. **`user_message` 的 `message_id` 与幂等**：当前仅 Intent 类消息强制 `message_id`，人类消息没有；统一词汇表后四类消息都需服务端分配稳定 `message_id`，否则卡片回答的 `in_reply_to` 无处可指。
17. **附件与未读计数**：`chat_sessions.rs` 目前无附件存储与 sidecar、无 per-participant unread 计数，而单聊 UX 两者都需要；复用 `chat.rs` 的 `files/` + sidecar 与 `BTreeMap<uid,u32>` 方案还是新做，需定。
18. **卡片超时的具体数值**与超时后执行侧行为（自动拒绝 / 保持暂停）；以及"本轮持有等待"与"结束本轮"按卡片类型的分流表。
19. **Agent 重装换 instance id 后旧 Chat session 的孤儿处理**：`session_id` 是服务端 UUID 而非参与者推导，历史不会丢，但 participants 里的旧 id 需要重绑或标记。
20. **群聊 execution session 的参与者级可见性如何建模**：判定依据是 routing key `chat:{chat_session_id}`（v3.1：已持久化在 Runtime `sessions` 表的 `intent_key` / `intent_closed` 两列，`intent-sessions.json` 已退役，ADR-106 D11）。v2.2 已选定落地方式：execution session 侧存 `chat_session_id`，`set_chat_binding` 同时置 `SessionVisibility::Members`（实现已存在），群聊与 PM task session 均走此路径；剩下的是读时如何反查参与者集合（或由 Gateway 注入会话作用域令牌），以及群成员被移出后对既往执行记录的访问如何回收。
21. **卡片状态事件的订阅拓扑**：前端订阅的是 execution session 的 `SessionStateChanged` 频道，需确认远程拓扑（Gateway ↔ Node ↔ Runtime）下该频道对非 owner 用户是否可达、以及多卡片并发时的订阅成本。
22. ~~**`turn_started` 与 `agent_message` 的配对窗口**~~：**v2.4 已关闭**——中间状态不再落盘，不存在永不收尾的进度行；崩溃后聊天侧只会看到一条缺失的终态，由 §5.5.1 的兜底写回补上。
23. **卡片置灰的展示文案**：不可点时向无权限参与者显示什么，既不能泄露 session 是否存在（hidden 404 的语义），又要让用户明白"这里有个执行过程但你不能看"。
24. **Router 的模型与成本口径（v2.2）**：`AgentConfig.chat_router.model` → compact model → Agent 默认模型的解析优先级；router turn 是否计入 Agent 预算；离线部署无 compact model 时的降级选择（关闭 router 回旧路径 vs 用主模型）。
25. **候选枚举的成本与一致性（v2.2）**：候选需合并 `SessionMetadataService::list_sessions` 与 `sessions` 表的 `intent_key` / `intent_closed` 两列（v3.1：`intent-sessions.json` 已退役），而当前 `list_for` 是目录扫描（§3.5 P2）；router 每条消息都要枚举一次，需补 per-chat 索引或短期缓存，否则会把 P2 缺陷放大成 P1。
26. **决策日志的保留与隐私（v2.2）**：`chat-router-decisions.jsonl` 记录候选标题与决策理由，属 Agent-private 数据，只经 Runtime HTTP 读取；需定保留窗口、轮转与导出边界（不得把其他 chat 的标题写进日志）。
27. ~~**降级回执的语义**~~：**v2.4 已关闭**——降级不再落 `router_fallback` 行，排障改看 `chat-router-decisions.jsonl` 与 WARN 日志（见开放问题 26）。

## 11. 与既有 ADR 的关系

| ADR | 与本设计的关系 |
|---|---|
| **ADR-009** Gateway Workspace 隔离 | 本设计的红线依据：Gateway 不直接读写 Agent-private 数据，Chat session 读写只经 acowork-user（§7.1） |
| **ADR-055** Runtime 远程化 / Node Agent | `install_path` 为 node-local 路径，使"Gateway 侧 fs 直读"在跨机部署时必然失败；这是写回必须由 Runtime 承担的部署学理由 |
| **ADR-073** 身份模型分层 | `agent_instance_id` 寻址使 Agent 重装后旧会话成孤儿（§10.19）；kind 中的 Agent 参与者存 instance id 而非 `agent_id` |
| **ADR-082** 记忆存储迁移 SQLite | Chat session 仍为文件存储（JSONL），未纳入 SQLite；两者边界需在实现设计中确认 |
| **ADR-084** 账号/用户聊天剥离为 acowork-user | 本设计把聊天域统一到 `chat_sessions.rs` 新布局；`chat.rs` 的旧 `users/{min}/chats/{max}` 布局是待收敛项（§4.3） |
| **ADR-060** Prompt Cache 友好的上下文块重排 | chat 层上下文编排的直接依据：Block A 静态内核 + Block B append-only 历史 + Block C 末尾动态块，cache 断点只在 A/B 末尾（设计 29） |
| **ADR-063** 包级 Prompt 覆盖 | `prompts/chat.md` 纳入 override 清单后从 execute prompt 拼接中排除；新增 override 需三处协同改动（§3.4 决策 27） |
| **ADR-066** Provider Cache Tokens | 缓存命中率的口径与观测点（cache_read / cache_write），用于验证编排效果 |
| **ADR-099** Chat Session 的 LLM 语义路由 | 本设计 v2.2 的决策来源：Chat Router 分层、T3 删除、群聊 / PM 边界、chat prompt 与 execute prompt 分离；其 2026-10-24 修订进一步删除执行会话的物理轮转（本设计 v2.7） |
| **ADR-086** 移动端 | §6.5 的移动端交互约束 |
| **ADR-091** LSP 注册表下沉 Node | 无直接耦合；仅共享"业务状态下沉到 Node/Runtime"的同一取向 |

## 12. 新旧编号对照

v2.0 重排了章节。代码注释中对本设计的 `§` 引用需按此表更新。

| 旧 | 新 | 旧 | 新 |
|---|---|---|---|
| §1.1 | §3.1 | §6 | §5.4 |
| §1.2 | §3.2 | §6.1 | §5.4.1 |
| §2 | §4 | §6.2 | §5.4.2 |
| §2.1 | §4.1 | §6.3 | §5.4.3 |
| §2.2 | §4.2 | §6.4 | §5.4.4 |
| §2.3 | §2.3 | §6.5 | §5.4.5 |
| §3 | §4 | §7 | §5.5 |
| §3.1 | §4.3 | §7.1 | §5.5.1 |
| §3.2 | §2.3 | §7.2 | §5.5.2 |
| §3.3 | §4.4 | §7.3 | §5.5.3 |
| §4 | §4.5 | §7.4 | §5.5.4 |
| §4.1 | §4.5.1 | §7.5 | §5.5.5 |
| §4.2 | §4.5.2 | §8 | §4.6 |
| §5 | §5.3 | §9 | §5.6 |
| §5.1 | §5.3.1 | §10 | §5.7 |
| §5.2 | §5.3.2 | §11 | §3.4 |
| §5.3 | §5.3.3 | §12 | §7 |
| | | §13 | §6 |
| §14 | §10 | §15 | §9.1 |

**重要更正与前置问题：设计文档编号 27 被两份文档同时占用。**

`docs/design/zh/27-global-search-llm-summary.md`（全局搜索 LLM 摘要，ADR-093）与本文件 `27-intent-messaging-and-group-chat.md` 共用编号 27。代码中 109 处 `design 27 §X` 引用里，**21 处指向全局搜索文档**，其中 18 处（`§4.4`×8、`§4.6`×4、`§4.1`、`§5`、`§5.1`、`§5.4`、`§4.2`、`§7`）的编号在本文件 v2.0 里**也存在**，因此会被静默解析成完全错误的章节（例如 `design 27 §4.4` 实义为"摘要 API"，本文件里却是"项目群聊生命周期"）。

本版本已把 81 处指向本文件的引用改写到 v2.0 编号；指向全局搜索文档的 21 处**未改动**，需先解决编号冲突才能修正。

**建议**：(1) 先消除文档编号冲突（重命名全局搜索文档并更新其引用与互链），(2) 代码注释改用章节标题引用（如 `design 27 "回复写回"`）而非纯编号——编号耦合是本次歧义的根因。

## 13. 变更记录

保留历史修订要点（原文照录；其中 § 引用为当时的旧编号，新旧对照见 §12。v2.2 / v2.0 等条目中描述的 T1/T2/T3 轮转与 handoff 机制**已在 v2.7 整体删除**，仅作历史记录保留）

> v3.1.3 修订（轮次门铃事件与重读装配修复，ADR-106 修订 2026-11-03）：① 新增 `round_state` 事件（proto `RoundStatePayload`，`SessionMessage` oneof 字段 34——当时的最高已用字段是 33），由 `AgentLoop::close_turn_round` 在关闭轮次后（round id 必须在关闭**前**取，关闭会清掉写者的 active round）经每会话 chunk 通道 → `relay_chunk_event_mqtt`（逐个 await，故调用序等于 broker 入队序）→ `publish_round_state` 发布，topic `acowork/agents/{instance_id}/sessions/{sid}/messages/round_state`，QoS 1、非 retained。载荷只有两个坐标 `round_id` / `chat_session_id`，正文与终态词一律不带（D4 不变），语义是"值得现在重读"而不是"这是最终内容"——事件在回合边界触发，而轮次终态记录是会话单写者的另一次 append（ADR-092）。它也覆盖停在卡片上的轮次（与终态走同一条门铃：块重读后读到 `waiting`，画出问题并继续显示本轮在进行中），此前这一支同样零通知。终态词不上线的原因：没有订阅方能在不重读的前提下用它，而 proto3 事后加字段是免费的。② 前端 pending 重读由一次性 `setTimeout` 改为 `setInterval`（根因：零写使重装配依赖 `[chatId, key, anyPending, reload]` 永不变，定时器跑完就再也装不回来，只有卸载/重挂载才恢复——这正好解释了"切走再切回才对"），周期从 4 s 放宽到 15 s 并降级为丢帧兜底；门铃命中已挂载块时立即 reload，带一次 500 ms 有界重试。③ 订阅落点：桌面通配 `messages/#` 零新增；移动因 `realtime.ts` 的单会话精确订阅不变量，门铃落在本就通配订阅 `agents/+/sessions/+/state` 的 presence 通道；被否决的写法是由执行会话将事件发到 chat 的 topic 树下（依赖 broker 的 permissive ACL，且破坏"topic 归属等于会话归属"）。④ 字段号是跨仓契约：移动不发 protobuf 运行时、线格式手写，所以 Rust 侧单测钉住字段 34 的 tag 与 proto3 默认值省略规则。⑤ 后端无罪证明：磁盘侧表实测同一轮 09:02:43.370 非终态 → 09:02:43.378 终态（8 ms 闭合），修的是通知面不是投影。D9 纪律不变：门铃不建立 liveness，只触发一次读。逐项见文档头 v3.1.3 注、§6.2、§6.5。

> v3.1 修订（chat 只留引用行，执行内容读时投影，ADR-106）：① chat 词表收为四类 `user_message` / `agent_message` / `execution_round` / `system_notice`，`interaction_request` 退出词表；一轮在 chat 里只有一条**空正文引用行**，自然键 `round:{instance}|{session}|{round}` 即幂等 id，**前置型**（先落行再跑轮）。② 引用行的内容全部读时投影：执行侧 `{sid}.jsonl.rounds` 可重建侧表 + chat 侧 `rounds.sqlite` 缓存，四态 `pending` / `ok` / `not_found` / `unreachable`，缓存只在终态写入（终态不可变 ⇒ 无失效路径）；exec→chat 方向的跨进程 HTTP 写从每回合 1–3 次降到 0，Runtime 侧 chat 写入口只剩引用行一处。③ 呈现单位从"行"变成"块"：块头回答"这条消息去了哪个任务、到哪一步"（v2.5 的 `turn_routed` 反应行随之废止），答案内联，卡片随投影进入块内，细看过程从块头"查看执行"跳转。④ 等待行的判据从 v3.1 的"本地在途的 POST（`sending`）"延长为 v3.1.2 的"本设备未结算的发送 claim"——两种写法都是正证据而不是缺证据的反推，`unreachable` 是一次故障不是进行中的轮次，`meta.auto_routed_to` 一类的证据位一并退出（废止 ADR-105 D4）。⑤ 寻址成为显式阶梯：级别 1 `in_reply_to` 指向引用行 ⇒ 确定性续投，级别 2 两级 `@`（`mentions.target_sessions` 与 `force_new_for`），级别 3 唯一候选直投 / 多候选 `session_choice`，级别 5 `new_task` 或 `propose_existing` + `confirm_continue`；答卡 = 人类那句话带地址重投递一次（原行 `message_id` 做幂等），因此转录里始终只有一条人类消息而不是两句；Router 的 act 集合收缩，不再静默投递到已有会话。⑥ 会话绑定单一真相：`intent-sessions.json` / `terminal-sessions.json` 退役，真相是 Runtime `sessions` 表的 `intent_key` / `intent_closed` 两列。⑦ 权限三层：读按群成员、答按发起人、按钮只给发起该轮的人；预算收据等改为 `system_notice`，Gateway 不再代写对话行；废止清单（`turn_dispatched` / `turn_started` / `turn_routed` / `router_fallback` / `interaction_request` / `interaction_resolved`）不迁移、不删除，只在渲染期隐藏。Desktop 两视图（1:1 + 群）与 Mobile 1:1 面同批接入块渲染器与两级 `@` 的第二级选址。不做兼容层，不迁移历史。逐项见 §3.4 决策 12、15、17、18、20 / §4.2 / §4.2.2 / §5.5.4 / §5.5.5 / §6.1 / §6.2 / §6.3 / §6.4 / §6.5。
>
> v2.8 修订（轮次来源词汇表与 PM 派发链路，ADR-102）：① Intent 信封 `kind` 收为四值词汇表 `user_message` / `pm_task` / `request` / `response`（空值与未知值兜底 `request`，cron 与混版旧生产者落保守侧），生产端在 Gateway 各构造点显式盖章，消费端（路由资格 / 渲染形态 / responder 角色 / 写回义务四个决策点）统一查 `acowork-core::intent::TurnOrigin` 策略表；task 坐标格式常量与判定/构造函数收进同一模块，feature 代码禁止裸 `starts_with("task:")`。修复的错位：PM 派发此前骑 `kind="request"`，被按应对方裁剪 `ask_user_question` / `intent_send`（人类任务轮失去向人提问的能力），并在执行 transcript 里渲染成整个信封 JSON、经会话标题派生二次泄漏到群聊回执。② PM 派发消息拆分：人读文本（标题+描述）进群聊 chat 行，唤醒 playbook 拆为信封字段 `assignment_brief`，Runtime 注入执行会话系统块（ADR-060 字节稳定：每代次常量），不进渲染文本、不落会话历史、不进任何聊天表面；群聊按 `meta.task_id` 渲染任务胶囊（该键必须在 chat 存储 `IntentMeta` 上声明——决策 29 的静默丢弃陷阱在本键上真实发生过一次，两端各有测试钉住），`project_id` 从派发行 meta 移除。信任边界：`assignment_brief` 是高信任字段，Runtime 按来源门控（提取与消费两道），Gateway 对 agent 来源 params 在出口剥离（保护混部旧 Runtime）。③ 派发触发源收敛为 PM 扫描器（`start_at` 到期 + 审批 + 依赖三闸门），撤销事件钩子；重试幂等：`Task.dispatch_message_id` 首试前落盘、按代次复用，chat 行与信箱预算每代次各占一份，退避 60s→5m→15m→30m、五次失败放弃并 error 日志（放弃按代次计，跃迁即重计）。逐项见 §4.5 字段表（`assignment_brief` 行）、§5.4.2（触发与幂等段）、§5.4.5（词汇表段）、§6（任务胶囊）。协议面：task-dispatch 是 Gateway 内部服务间端点，契约（`assignment_brief` / 可选 `message_id` / kind=`pm_task`）以本文 §4.5 字段表为准，不重复收录进 protocols/http；版本偏差注记（新 Gateway→旧 Runtime 会把 `pm_task` 兜底成 responder 语义，多节点部署 Runtime 先行或同批）见 runbooks。

> v2.7 修订（执行会话轮转机制删除，取代 v2.2 的三态轮转模型）：T1（空闲间隔）/ T2（PM task 终态或本次执行发生过 compaction）与已在 v2.2 取消的 T3（配置冲突）一并删除，连 successor / predecessor 会话对、`handoff_note` 通道、`rotation_handoff` transcript 标记、`last_compaction_summary` 派生列、以及 `intent_idle_rotate_hours` / `intent_handoff_max_tokens` 两个配置项（含 Agent 设置卡片与五份 locale）。理由：execute session 本就是可一直追加、持续交互的会话，每一轮都重新读全量 JSONL（ADR-032），压缩过的会话同样能继续接 dispatch——「压缩过就该换窗口」没有依据；T1/T2 是「没有 LLM 做路由判断」时代用物理信号近似「任务该结束了」的产物，语义决策上提到 Chat Router 后不再承担任何路由含义；T2 的 handoff 摘要与「新任务不被旧摘要污染」的目标直接冲突。收敛后的唯一规则：执行会话**长命**，上下文卫生只有 compaction 阈值，文件规模只有既有 JSONL 硬上限，开新执行会话只有三个显式来源（router `dispatch(new)` / PM task 终态 close / 用户经 `+` 手动新建）。不变面：`turn_mode` 仍只有 new / continue 两态（ADR-100 D11），`chat_dispatch` 返回值随之从三元收回二元，transcript 不新增 tag，映射可指向已压缩的会话且不做健康检查，压缩自身的恢复跳过仍用 `last_compaction_offset`；历史 JSONL 里的 `rotation_handoff` 行不迁移、不清理，仍从聊天渲染中过滤。详见 ADR-099「修订（2026-10-24）」，逐项见 §2.4 / §3.3 / §3.4 决策 3、4、23 / §4.1 / §4.5.2 / §5.4.1 / §5.4.2 / §6.2。

> v2.4 修订（轮次状态只在运行期，中间状态不再落盘，取代 v2.3 的指针行）：中间状态彻底不落盘。`turn_dispatched` / `turn_started` / `router_fallback` 均不再写入，旧行在 Desktop 两个表面与 Mobile 渲染期一律隐藏。发送到回复之间的空窗由**一条前端派生的居中等待行**覆盖（"{{agent}}正在处理…"，所有在途态共用一种文案 + 文字滚动闪烁），它的唯一判据是"最新一条 `user_message` 尚无终态回答"——不新增 MQTT 通道、不改会话列表 API。终态仍然落盘；**空正文的终态渲染为居中状态行**（aborted 中性，failed / timeout 红）并保留"查看执行"，不再画成 agent 气泡；绑定 chat 的执行会话其本地回合被 stop 时落一条幂等的 `aborted` 空行。取代 v2.3 的 ADR 决策集与 ADR-100 D9 末条。详见 ADR-100 Rev.2（D10）与 §2.3 / §3.4 决策 7、17、18、20 / §4.2.2 / §5.5.5 / §6.2 / §6.3 / §6.5。

> v2.3 修订（chat 事件日志化，撤销 v2.1 的执行状态卡片）：删除叠在 durable 事件日志上的 live 状态镜像卡片（`ExecSessionStatusCard`）——它与事件日志两块真相永远对不齐（派发后到回复前的可见性空窗、stop 时卡片与仍在跑的 pending 新一轮打架）。chat 回归纯时间点事件日志：早期 pointer 改名 `turn_dispatched`（路由时点，幂等 id `td:{message_id}`）+ `turn_started`（开轮时点，id `ts:{message_id}`）两类并存，终态沿用 `agent_message`；"仍在执行"由新一轮自己的开始行表达，执行过程细节靠"查看执行"跳转进 execution 视图。`interaction_request` 仍为行内决策卡（`InlineInteractionCard`）。1:1、人类群聊 @、PM 任务派发三条链路本就同构（同一 pointer 写入），共用同一渲染器（`ChatNoticeEventRow` + `InlineInteractionCard`），数据侧零新增。两端删除驱动 live 卡片的读时投影。**已在 v2.4 被取代：两类 pointer 行不再写入。**详见新增 ADR（chat 事件日志化）与 §3.4 决策 16–20 / §4.2 / §4.2.2 / §5.5.5 / §6.2 / §6.3 / §6.5。

> v2.2 修订（Chat Router）：每条人类消息（1:1、群聊 @Agent）先经过目标 Agent 的**无状态 Chat Router turn**，工具集只有 `dispatch`，产出回答 / 转交已有执行会话 / 新建；T3 配置冲突轮转删除，T1 降级为 prompt 信号（仅降级路径硬触发），T2 保留且在 router 选定目标后求值；群聊 @ 只决定 agent、不决定执行会话，多话题可并行；PM 派单保持确定性 `task:` 映射，但 task 会话绑定项目群聊并进入候选、终态排除；chat prompt 与 execute system prompt 分离（内置基座 + 可整段覆盖的 `chat.md`，会话级优先于包级 + 场景段）；新增 `dispatch` 不替代 `intent_send` / `chat_history_read` / `ask_user_question`，只按 turn 类型裁剪可见性。详见 ADR-099 与设计 29，逐项见 §2.2 / §3.4 决策 22–28 / §4.1 / §5.1 / §5.4.1 / §5.4.5 / §6.2 / §8.2。

> v2.1 修订（user-agent 单聊微信模型）：每个 (user, agent) 恰好一条 Chat session，作为 Agent 页统一标签栏最左的固定 tab（标题 = Agent 名，不可关闭/删除）；execution session 是同一标签栏里可随意开关的普通 tab，两个表面共存互不销毁，关掉最后一个 exec tab 回落 Chat tab。撤销 v2.0 §5.4.4「点 `+` 新建第二条同 pair 会话」与 §5.5.2「Chat history 按钮」；`pairs.json` 单条语义即正确，原 P1 升级作废；§10.15 鉴权开放问题随入口撤销关闭。ADR-086 决策 8 的多会话并行改由 execution session 层承载。逐项见 §5.4.4 / §6 / §6.1。

> v1.8 修订：群聊触发的 execution session 对该群聊全体参与者可读，判据由 routing key `chat:{chat_session_id}` 派生、不建独立 ACL，并明确**不得复用 `SessionVisibility::Public`**（其语义为平台级可读，会泄露群执行历史）；`turn_started` 写入方定为 Runtime，与 §7.1 写回同源，Gateway 不承载业务逻辑，发送侧即时反馈改用前端乐观占位（§7.5）。
> v1.6 修订：user-agent 单聊正式定义为 Agent 页面的默认 Chat session（§6.4），与群聊、agent-agent 共用同一套存储、消息词汇表与唯一写入口；Intent 从消息类型降为字段，词汇表收口为 `user_message` / `agent_message` / `interaction_request` / `system_notice` 四类（§2.2）；交互卡片纳入 Chat session 并按可答者分流（§7.4、§6.5）；回复写回触发条件改为与本轮角色无关（§7.1）。
> v1.7 修订：Chat session 正式成为 Agent 页面唯一主视图且不可隐式关闭，execution session 降为后台但入口保留（§6.4）；新增§7.5 执行状态卡片——持久化不可变的 `turn_started` 指针行，实时状态一律读时投影不落 Chat session，收尾由聊天终态行而非 `Idle` 决定，状态只订阅不轮询，可点性由 execution session 读权限决定；`pairs.json` 由 P2 升为 P1。
