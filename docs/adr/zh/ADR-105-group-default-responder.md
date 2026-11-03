# ADR-105：群级默认接收方——未 @ 的群消息交给一个声明的 agent

- **编号**：ADR-105
- **状态**：已接受，**部分被 ADR-106 取代**（D4 的证据位与 D5 的 chat 行形态）
- **日期**：2026-11-01
- **决策者**：ACowork 架构组
- **关联**：ADR-098（统一标准群组模型）、ADR-099（Chat session LLM router）、ADR-100（chat 事件日志化）；设计文档 27 §5.3.1 / §5.4.1 / §5.5

> **取代说明（2026-11-03）**：[ADR-106](./ADR-106-chat-execution-reference-projection.md) D14 废止了本 ADR 的 **D4**（`auto_routed_to` 作为等待行的证据位）与 **D5 的 chat 行形态**（等待行反推机制删除，`delivery_failed` 作为通道事实保留）。D1 / D2 / D3 / D6 / D7（默认接收方本身、写入四道门、生命周期、不引入群级 LLM 路由）仍完全生效——默认接收方如今依旧在 `mentions` 投递路径上工作，只是它留下的痕迹不再决定前端怎么渲染等待行。

## 摘要

群聊消息在没有 @ 任何 agent 时不再只能归档。Chat session 行上新增一个字段 `default_responder`（该群的一个 agent 参与者），未 @ 的群消息由 Gateway 投递给它；没有配置该字段的群**保持原行为**（仅归档）。客户端不再为"有 @ / 无 @"分叉发送入口：群发一律走 `POST /api/intents/mentions`，`targets: []` 的语义由"非法请求"改为"由服务端决定"。

## 背景

- **事实：未 @ 的群消息是死信。**群聊的普通 `user_message` 只写入 Chat session、不投递（§5.3.1），因此"agent 刚说完话、人直接回一句"这一最高频的交互没有任何反应：没有投递、没有回复、没有提示，用户必须 @ 一次重发。私聊没有这个问题，因为私聊每条输入都进 agent 的 Chat Router（ADR-099）。
- **事实：客户端已有的分叉掩盖了它。**`chatSessionStore.sendGroupText` 曾按 `targets.length > 0` 分叉：有 @ 走 `/api/intents/mentions`，无 @ 走仅归档的 `POST /api/chat-sessions/{id}/messages`。归档入口按设计不投递，于是"人回了 agent 一句"在数据上就是一条无人唤醒的行。
- **事实：没有配置就没有选择，不等于需要 LLM。**"谁该接这条消息"有两种可行来源：（a）群里声明一个接收方，人一次决策、长期有效；（b）每条未 @ 消息由一个不归属任何 agent 的公共群 chat session 跑 LLM 路由。方案（b）让每个 agent 各有一个群绑定 session、再加一个公共 session，消息拓扑从"一条群消息 → 一个目标"变成"一条群消息 → 一次路由 → 一个目标"，且路由结果无法被人稳定预期。
- **事实：被选中的 agent 已经自带路由能力。**它收到消息后仍在自己的 Runtime 内跑 Chat Router（ADR-099），决定回复还是转交正在执行的会话。因此本 ADR 只需要解决"谁是第一接收人"，不需要再实现一次语义路由。

## 决策

### D1 — 群级默认接收方是会话行上的一个字段，不是新 session

`ChatSession` 新增 `default_responder: Option<String>`（agent instance id）。`None` = 未 @ 的消息仅归档（历史行为，也是纯人类群的唯一合理默认）。

不新增 session、不新增 kind、不新增消息类型。语义上它是"成员身上的一个角色"，与 `owner` 同级。

### D2 — 写入：owner-or-admin，且目标必须是当下的 typed agent 参与者

存储层 `set_default_responder`（`core/acowork-user/src/chat_sessions.rs`）在会话锁内一次校验四道门：

1. kind 是群（`user_group` / `project_group`；`user_user` / `user_agent` / `agent_agent` 返回 `not_a_group_session`）；
2. 调用者是参与者（`not_a_participant`）；
3. 调用者 owner-or-admin（复用 `can_govern_group`，与 `chat.md`、改名、转让同门）；
4. 目标必须是 `identity == Agent` 的参与者（`ParticipantIdentity::Agent`）。未标注身份的遗留行不算证据，不做猜测。

`PUT /api/chat-sessions/{id}/responder` 只做转发；`GET` 对任意参与者开放，响应带 `editable`，避免前端自己推导治理规则。

### D3 — 投递：`targets: []` 表示"服务端决定"，不是"非法"

`POST /api/intents/mentions` 取消 `targets` 非空校验。空列表时：

- 会话 `default_responder` 有值且仍是 live agent 参与者 → 投递给它；
- 否则 → 归档，响应 `{"routed": "archived", "results": []}`。

`routed` 字段让调用方能区分"无人被请求"与"请求了但全部失败"——这两者在改造前都是静默。

### D4 — 自动路由是行上的独立事实：`meta.auto_routed_to`

被投递的行仍是一条广播行（`to: ""`），`meta.mentions` 保持"人打的 @ 名单"不变，自动目标写入 `meta.auto_routed_to`。

理由：`mentions` 会被渲染成 @ chip，把自动目标写进去等于替发送者发言。而等待行("正在处理")的判据是"这条人类行是否惊动了 agent"，它需要一个新的证据位——`auto_routed_to` 就是它。存储层必须显式声明该字段，否则会在序列化边界被丢弃（与 `meta.task_id` 同一陷阱）。

> **已被 ADR-106 D9 / D14 推翻的部分**：把 `auto_routed_to` 当等待行判据，等于把"读不到"当成"还在跑"——跳机、离线、投影失败都会产生永久等待。字段仍然写入（它是"这句话去了哪里"的记录），但不再驱动渲染。等待行后来被收缩成"本地在途、尚未返回的那次 `sending`"，那一版也太短（POST 约 150 ms 就返回，而该轮第一条行可在一分钟后落地）；现行规则见 [ADR-106 修订（2026-11-03）](./ADR-106-chat-execution-reference-projection.md)：本设备自己那次发送的 claim，键为响应里的 row id，收口于第一条回答它的行，300 s deadline 兜底。

### D5 — 投递失败必须可见

`wake_and_deliver` 失败时，Gateway 追加一条 `system_notice`：`notice_type: "delivery_failed"`、`in_reply_to` 指向原 `user_message`、`error_code` 记录失败原因，**body 为空**。措辞由各个面本地化（与 `router_fallback` 同一约定：行由 producer 写、措辞由 surface 拥有），写入失败本身只记 warn、不影响响应。

前端两条规则：

- `ChatNoticeEventRow` 按 `notice_type` 渲染该行（body 为空也必须画）——这条行存在的前提就是"失败不再静默"；
- `chatTurnState.answeredAfter` 见到绑定同一 trigger 的 `delivery_failed` 就结束等待：收据就是结局，继续显示等待等于更响的谎言。

> **ADR-106 D14 后的现状**："失败必须可见"的语义与 `delivery_failed` 通道行保留（第一条规则仍成立）；第二条规则的宿主 `chatTurnState.answeredAfter` 已随 P6 删除。2026-11-03 后回执重新承担一个更小、也更真实的角色：它不是"反推的关闭信号"，而是本设备那次发送的 claim 的**结算行**——群面上失败回执可见，也同时结束等待，两条规则是同一条（1:1 的失败不建 claim，因此无需结算）。

### D6 — 生命周期：成员走了，角色跟着走

- `remove_participant`：被移除者等于 `default_responder` 时清空该字段；
- `sync_group`（PM 同步成员）：`default_responder` 不在新 roster 中时清空；
- 投递前复核：`SessionInfo::live_default_responder()` 每次读都要求目标仍在 `participants` 中且是 agent；不满足则视为"未配置"。

已卸载但仍在成员表里的 agent **故意通过**复核：投递会以 `target_not_installed` 明确失败，并落 `delivery_failed` 行（D5），而不是被静默降级成归档。

### D7 — 本 ADR 不引入 LLM 路由（阶段二不做）

"未 @ 的消息由 LLM 判断该给哪个 agent"被明确排除在本 ADR 之外：

- 群级默认接收方 + 该 agent 自己的 Chat Router 已经覆盖"人在跟谁说话"的绝大多数情形；
- 公共群 session 的路由要求一个不属于任何 agent 的上下文长期维护群状态，其消息拓扑与 ADR-098 的"kind 化单一容器"相冲突；
- 路由决策不可被人预期，反而比"没反应"更难解释。

若未来需要，它应作为一个独立的、**在接收方 Runtime 内**的判定（answer_self / handoff / stay_silent），而不是新增一个公共 session。

## 被拒绝的备选

| 备选 | 拒绝理由 |
| --- | --- |
| 公共群 chat session + LLM 路由（方案 b） | 每个 agent 的群绑定 session 之外再维护一个公共 session，消息拓扑与状态归属复杂化；见 D7 |
| 硬编码 PM agent 为全平台默认接收方 | 接收人是群的事实，不是平台的常量；纯人类群、非 PM 群都会被强加一个 agent |
| 保留客户端的"有 @ / 无 @"分叉，仅在后端补一条兜底 | 路由是服务端的事；客户端分叉意味着每个新面（移动端）都要再分叉一次，且分叉本身就是该缺陷的成因 |
| 把自动目标并入 `meta.mentions` 复用现有等待行逻辑 | 会在气泡上画出人没打的 @ chip；`mentions` 的语义必须保持"人说的话" |
| 投递失败只写日志 | 用户看到的仍是"发了没反应"，即本 ADR 要消灭的现象 |

## 影响

- **数据**：`chat_sessions` 新增可选字段，`serde(default)`，零迁移；`meta.auto_routed_to` 同理。
- **协议**：`/api/intents/mentions` 接受空 `targets`（以前 400），响应新增 `routed`。老客户端不发空 targets，行为不变。
- **UI**：群设置面板新增"默认接收 Agent"卡片（读取对所有参与者开放，写入 owner-or-admin）；群发送只剩一个入口。
- **回滚**：把 `default_responder` 置空即回到全量历史行为；字段与 `routed` 都可安全忽略。