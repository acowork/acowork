# ADR-098：统一标准群组模型——所有聊天面收敛到 kind 化的 chat_sessions

- **编号**：ADR-098
- **状态**：已接受
- **日期**：2026-10-21
- **决策者**：ACowork 架构组
- **关联**：ADR-076（多用户账号体系）、ADR-092（user chat 身份与有界读取）、ADR-096（请求路径禁止全量读会话）、ADR-097（会话内聊天记录分类查询——DM/群组复用其检索面）；设计文档 27 §4.4 / §5.4.4 / §6

## 摘要

把桌面端所有「人在对话」的聊天面收敛到**一个 kind 化的通用容器** `chat_sessions`：不再有「一对一 / 群 / 项目群」各自独立的存储与人类面组件，而是**一种标准群组 + 五个 kind 特例**。人类面统一由 `GroupChatPanel` 渲染，差异只由 `kind` 驱动；消息、检索（ADR-097）、附件、定位全部共享，零新增存储。

五个 kind 都是标准群组的特例：

| kind | 语义 | 成员来源 | 成员治理 |
| --- | --- | --- | --- |
| `user_user` | 两个人的最小群组（DM） | 会话参与者 | 无（1:1，不可增删） |
| `user_group` | 用户自建的多用户 / 多 agent 群组 | 会话参与者 | 任意参与者增删（本 ADR 新增） |
| `project_group` | 项目频道 | PM 项目成员，服务端 `sync_group` 同步 | PM 中介，只读 |
| `user_agent` | 人↔agent 的 1:1 | 会话参与者 | 无（1:1，不可加人） |
| `agent_agent` | agent↔agent 的 1:1 | 会话参与者 | 系统中介 |

## 背景

- **收敛前的碎片化**：`user_user` 一度走独立的 `userChatStore` / `InboxPanel`，`project_group` 走 `GroupChatPanel`，`user_agent` 走 `AgentChatView`——三套人类面、两套存储语义，检索 / 附件 / 未读要在每个面各写一遍。
- **P0（已落地并验证）**：`user_user` 迁到 `chat_sessions(user_user)`，DM 复用 `GroupChatPanel` + `chatSessionStore` + `ChatHistoryDialog`；`userChatStore` / `InboxPanel` 退役。DM 由此免费获得会话内检索（挂 `activeGroupId` 即可走 `/api/chat-sessions/{sid}/search`）。
- **后端本已 kind 无关**：`chat_sessions` 早已是通用容器，`SessionKind = { UserUser, UserGroup, ProjectGroup, UserAgent, AgentAgent }`；`create_session` 已接受 `kind=user_group`（参与者 accounts→Human、其余→Agent，仅要求调用者本身是参与者）。

## 决策

### D1 — `chat_sessions` 是唯一容器，`kind` 只是显示 / 行为标签

不为任何 kind 另立存储。`kind` 仅决定：侧栏行的图标、成员面板走哪条治理路径、@ 选择器是否启用（`user_user` 关闭）。消息读写、分页脊柱、检索索引、附件、未读对五种 kind 完全同构。

### D2 — `user_group` 的「建群」零后端新增

既有的 `POST /api/chat-sessions`（`kind=user_group`）已足够：服务端经 `typed_participants` 解析身份（不信 body 的角色），并强制调用者是参与者。P1 的**净新增只有用户侧成员增删路由**。

### D3 — 用户侧成员增删路由：any-participant 授权、限 `user_group`

在 [chat_sessions_api.rs](../../../core/acowork-user/src/http/chat_sessions_api.rs) 新增两条 human-facing 路由：

- `POST /api/chat-sessions/{sid}/participants`（body `{ participant_ids }`）
- `DELETE /api/chat-sessions/{sid}/participants/{pid}`

规则：

1. **鉴权**：调用者须是该会话当前参与者（any-participant）。非参与者复用既有 offload 错误约定映射为 `404`（隐藏存在性）。
2. **kind 限定**：仅 `user_group`。`project_group` 拒绝（PM 治理）、`user_user` / `user_agent` 是 1:1 pair 拒绝加人——一律 `422`。
3. **身份服务端解析**：新成员经 `typed_participants` 判定 Human / Agent，不接受 body 声明。加人幂等（已在册忽略）。
4. **下限**：移除后参与者 < 2 拒绝（群组至少留 2 人）。可移除自己（= 退出）或他人。
5. **门铃**：两条路由变更后都向该会话的 human 参与者发 `notify_chat_changed`，保证其他端实时刷新成员列表（`chat_sessions::add/remove_participant` 本身不发门铃，由 handler 补发）。

### D4 — 前端单一 `groups` 桶 + `dms` 桶，按 `kind` 分流

[projectGroupStore.ts](../../../apps/acowork-desktop/src/stores/projectGroupStore.ts)：

- `fetchGroups`：`user_user`→`dms`；`project_group` ∪ `user_group`→统一 `groups`（`kind` 供行图标 / 成员栏区分）。admin 跨账号并集仅对 `project_group` 有意义，用 `mergeById` 合并、保留自己行的未读上下文。
- 用户侧动作：`createGroup(title, memberIds)`（self 强制入且去重 → POST → upsert → `selectGroup`）、`addMembers(sid, ids)`、`removeMember(sid, pid)`（成功后从响应同步本地参与者；移除自己则丢行并释放中栏）。

### D5 — P2（Scoped）：成员面板按 kind 分派，不物理合并 AgentChatView

[GroupMembersPanel.tsx](../../../apps/acowork-desktop/src/components/chat/GroupMembersPanel.tsx) 作为一个 hook 后分派的 dispatcher：

- `user_group` → [UserGroupMembersPanel.tsx](../../../apps/acowork-desktop/src/components/chat/UserGroupMembersPanel.tsx)：数据源是会话 `participants`，暴露 add / remove（any-participant）。
- `project_group`（或仍在解析的会话）→ 保留 PM-project 只读路径（`ProjectGroupMembersPanel`）。

`GroupChatPanel` 表头对任意非 DM kind 通用渲染标题 + 成员数 + 历史按钮，`@` 选择器仅对 `user_user` 关闭。**不做** `AgentChatView` 与 `GroupChatPanel` 的物理合并（记为未来 ticket）。

### D6 — 收敛达成标准

单一人类面组件（`GroupChatPanel` + kind 分派的成员面板）覆盖 `user_user` / `user_group` / `project_group` 三种 kind，差异仅由 `kind` 驱动。

## 后果

- **存储/组件收敛**：三套人类面降到一套 + kind 分派，检索 / 附件 / 未读只写一遍即全 kind 共享。
- **`user_group` 成为一等公民**：用户可自建多用户 / 多 agent 群组并在 any-participant 下自助管理成员，无需 PM 中介。
- **旧投影面保留（本 ADR 不删）**：`chat.rs` / `/api/users/{me}/chats` 投影端点与 `user-chat-api.ts` 附件 helper **不能删**——mobile 仍是其调用方。它们的**物理删除 gate 在 mobile ticket**（P3 收尾），随该票一并移除，避免范围蔓延。
- **legacy 数据迁移作废**：`user_user` 迁 `chat_sessions` 已在 P0 完成，无需再为旧 inbox 存储写迁移。
- **AgentChatView 合并延后**：P2 采 Scoped 方案，物理合并留作未来 ticket。

## 落地状态（P0–P3）

- **P0**：桌面 + 后端完成并验证（`user_user` 收敛、`userChatStore`/`InboxPanel` 退役；backend + desktop 测试绿）。
- **P1**：后端用户侧成员增删路由 + 测试完成；前端 store 动作 + `NewGroupDialog` + 侧栏入口完成。
- **P2（Scoped）**：成员面板 kind 分派完成（`user_group` 走 participants 增删）。
- **P3**：本 ADR 记录收敛；`chat.rs` 投影端点与 `user-chat-api.ts` lib 的物理删除**归入 mobile ticket**，本轮不删。
