# Intent / Chat Session 统一 — 回滚演练手册 / Rollback Drill

> 对应计划：[intent-chat-session-unification-plan §8](../plan/zh/intent-chat-session-unification-plan.md)。
> 对应设计：[设计 27](../design/zh/27-intent-messaging-and-group-chat.md)、
> [ADR-084](../adr/zh/ADR-084-user-standalone-process.md)、
> [ADR-086](../adr/zh/ADR-086-mobile-app-im-ia-and-multi-session.md)、[ADR-092](../adr/zh/ADR-092-user-message-identity-and-bounded-reads.md)。
>
> 本文是**面向操作者的演练手册**——每一层"关掉新路径"的杠杆在哪、怎么拨、期望看到什么、
> 数据会发生什么。为什么可以这么回滚（A1 写新读旧、旧行永不改写）看计划 §10；本文只讲"手怎么动"。

---

## 0. 一分钟版

| 出问题的层 | 回滚杠杆 | 数据后果 |
|---|---|---|
| 后端词汇表 / 写回 / 卡片（Wave 1–3） | `git revert` 对应 PR | **零**——旧行从未被改写（A1） |
| 可见性派生（Wave 4） | `git revert` | SQLite 里已落的 `Members` 行保留；旧构建读不懂该值时按最严解释（等同 Private） |
| 桌面主视图（Wave 5） | 从 Gateway 代理前缀表删 `"/api/chat-sessions"` 一行并重启 | 零。`openAgent` 失败 → AgentPage **自动**落回旧执行会话视图（旧入口保留一个版本周期） |
| 移动端（Wave 5.5） | 回退 APK 版本 | 零。纯客户端决策，不动协议与数据（ADR-086 §6.4） |
| 旧 `chat.rs` 布局收敛（Wave 6.2） | `git revert` + 重启 user service | 历史**不丢但旧构建看不见**（§4，唯一需要动数据的回滚，先读再拨） |
| 只想停投递、不停读 | Gateway 路由 `/api/intents/chat-message` 下线 | 零。会话仍可读，新消息不再进 |

任何一层回滚之后，跑 §5 的验证矩阵，全绿才算回滚完成。

---

## 1. 为什么回滚几乎总是免费的

计划 §10 决策 1 选了 **A1「写新读旧」**：新词汇表（`user_message` / `agent_message` /
`interaction_request` / `system_notice`）只**追加**写入 `chat-sessions/{sid}/messages.jsonl`，
从不改写、从不重写存量行。于是回滚 = 撤掉一个二进制/一个前端构建，而不是撤一次数据变换。
唯一真正动了存量数据的是 Wave 6.2 的一次性迁移（§4，它同样满足"旧行永不改写"——旧行是被
**复制**成新行，不是被改写）。

数据落点（multi_user 形态，`{data}` = user service 的 `data_dir`）：

```
{data}/chat-sessions/{session_id}/messages.jsonl   # 唯一写入口，append-only
{data}/chat-sessions/{session_id}/session.json     # kind / participants / unread / 计数
{data}/chat-sessions/{session_id}/files/           # 附件，与会话同生命周期
{data}/chat-sessions.pairs.json                    # pair key → session_id（含旧 min__max key）
{data}/chat-sessions.participants.json             # per-participant 索引（6.1）
{data}/chat-sessions.projects.json                 # 项目群索引
{data}/users/{lo}/chats/{hi}                       # 旧布局（6.2 迁移成功后已删除，见 §4）
```

---

## 2. Wave 1–3：后端语义（revert 即回滚）

**杠杆**：`git revert` 对应 PR → 重建 `acowork-user` / `acowork-gateway` / `acowork-runtime` → 重启 Gateway（supervisor 会带起新 user service）。

**演练步骤**（开发机，独立 target 目录避免锁 exe）：

```bash
cd core
env CARGO_TARGET_DIR=<独立目录> cargo build -p acowork-user -p acowork-gateway
env CARGO_TARGET_DIR=<独立目录> cargo test -p acowork-user --lib chat_sessions
env CARGO_TARGET_DIR=<独立目录> cargo test -p acowork-gateway --test chat_sessions_e2e
```

**期望**：revert 后旧测试集绿；新 kind 的行**仍在文件里**——旧构建读到时按旧三分类的容错路径
处理（corrupt-line 规则：占行号、不出气泡），不会崩、不会丢后续行。这正是 A1 的验收面。

**注意**：2B（词汇表换代）与 2A 是分开两个 PR 的（计划决策 4），回滚 2B 不需要回滚 2A。

---

## 3. Wave 5 / 5.5：前端主视图（有现成的自动降级）

### 桌面

**杠杆**：[user_proxy.rs](../../core/acowork-gateway/src/http/user_proxy.rs) 的 `user_proxy_routes()`
前缀数组里删掉 `"/api/chat-sessions"` 一行，重建重启 Gateway。

**原理**：代理下线后 `GET /api/chat-sessions/default` 变 404 → `chatSessionStore.openAgent` 失败
→ `AgentPage` 的 `resolved && defaultId` 门不满足 → **自动**渲染旧 `ChatPanel`（执行会话视图）。
这就是计划里"旧入口保留一个版本周期"的开关形态：不是 feature flag，而是一条代理路由——
关掉它，降级路径就是被演练过的生产路径本身。

**演练步骤**：

1. 拨杠杆、重启 Gateway。
2. Desktop 打开 Agent 页 → 期望：会话照常可用，界面是旧执行会话视图，无红屏无死转圈。
3. 恢复杠杆 → 期望：Agent 页回到 Chat session 视图，历史消息（含降级期间的写入）完整可见
   ——降级期间 Runtime 的写回走的是 Runtime→user service 的内部通道，不经过这条代理。

### 移动端

**杠杆**：回退 APK（`dev/android/build_mobile.ps1` 出旧版本安装）。

**原理与边界**：5.5 全部是客户端改动（新 `chatSessionStore` + `AgentChatScreen` + 路由表一行），
不动任何协议与数据。ADR-086 §6.4 的原话：这个决策错了也不产生需要清理的迁移。旧版 APK 的
入口指向执行会话视图（`chat/detail`），后端两条面都还在，直接可用。

**演练步骤**：装旧 APK → 打开 Agent → 期望落在执行会话视图、能收发；`历史` 语义的入口本来就在旧版主页面上。

---

## 4. Wave 6.2：旧 chat.rs 布局收敛（唯一要动数据的回滚，先读再拨）

6.2 把 human↔human 聊天从 `users/{lo}/chats/{hi}/` 迁进 `chat-sessions/`，
由 [chat_sessions.rs](../../core/acowork-user/src/chat_sessions.rs) 的 `migrate_legacy` 在
user service 启动时执行（multi_user 形态）。**迁移成功后旧目录被删除**（删除失败才封存为
`.migrated`），所以回滚不是"把目录改回来"那么简单。先认清三个事实：

1. **数据没有销毁**。旧行全部以新行形态存在于 `chat-sessions/{sid}/messages.jsonl`，
   `message_id` 是确定性的 `migrated:{lo}__{hi}:{行号}`；附件复制进 `{sid}/files/`。
2. **旧构建看不见它们**。revert 后的 `acowork-user` 只读 `users/{lo}/chats/{hi}/`——目录没了，
   人类↔人类会话显示为空。**这是可见性回退，不是数据丢失**；再向前（re-apply）后一切照旧。
3. **再向前是幂等的**。新构建重新部署时 `migrate_legacy` 重跑：旧目录已删 → 无事可做；
   降级期间若有旧构建写入（不应发生，见下）→ 按 `migrated:` id 去重补齐，不产生重复行。

### 回滚决策树

```
6.2 出问题
 ├─ 症状是"适配器行为错"（路由/错误码/未读） → 优先修，别回滚：wire 契约有 9 条测试钉着，
 │   新内核有 chat_sessions_e2e 第 9 条（human↔human 全链路）钉着，修复面小。
 ├─ 症状是"迁移错"（数据进错会话/漏行） → revert + 重启；新 store 里错误的 migrated 行
 │   可整段删除该 pair 会话（session_id 从 pairs.json 摘掉 + 删目录），旧目录若已删则走下面
 │   的导出配方从 .migrated 或备份恢复。
 └─ 必须让旧构建读得到历史（长时间回退、跨版本共存） → revert 之前先执行下面的导出配方。
```

### 导出配方（新 store → 旧布局，回滚前的"备份桥"）

前置：`jq`；user service 已停（避免导出与写入赛跑）。对每个 `user_user` 会话：

```bash
SID=...; LO=...; HI=...            # LO/HI 从 chat-sessions/{SID}/session.json 的 participants 读
OUT="{data}/users/$LO/chats/$HI"
mkdir -p "$OUT/files"
# 1) 行：user_message 按原 ts/from/body 还原成旧 ChatMessage 形状
jq -c 'select(.kind=="user_message") | {seq, ts, from, kind:"text", body, attachments}' \
    "{data}/chat-sessions/$SID/messages.jsonl" > "$OUT/messages.jsonl"
# 2) 附件：sidecar 原样复制（文件名即 id）
cp "{data}/chat-sessions/$SID/files/"* "$OUT/files/" 2>/dev/null || true
# 3) conversation.json：unread 直接搬，计数与时间戳按导出结果重算
```

**纪律**：这是回滚桥，不是双写通道。导出后旧构建接管该 pair；新构建重新上线时 `migrate_legacy`
会按 `migrated:` id 去重——**导出时行序与首次迁移一致（同一文件、同一顺序）才能命中去重键**，
中途手工改过行序会导致重复行，宁可整会话重建。

### 演练步骤（单机 multi_user）

1. 停 Gateway；`git revert` 6.2 提交；重建 `acowork-user`。
2. （可选）跑导出配方。
3. 起 Gateway → 旧构建登录两个账号 → 期望：人类↔人类会话可读（若跑了导出）或为空（未导出），
   Agent 会话不受影响（它本来就在 `chat-sessions/`）。
4. `git revert --no-commit <revert-of-6.2>` 恢复新构建 → 重启 → 期望：日志出现
   `legacy chat layout migrated (Wave 6.2)`（若做过导出）或无迁移行；历史完整。

---

## 5. 验证矩阵（每层回滚后必跑）

| 检查 | 命令（core/ 下，独立 CARGO_TARGET_DIR） | 期望 |
|---|---|---|
| 新内核全链路 | `cargo test -p acowork-gateway --test chat_sessions_e2e` | 9/9（含 local 模式负面与 human↔human 旧路由） |
| 可见性读路径 | `cargo test -p acowork-runtime --test session_visibility_e2e` | 1/1 |
| user 域单测 | `cargo test -p acowork-user` | 全绿（含 6 条迁移往返） |
| 红线 | `bash dev/ci.sh check` | 9 条全过（fs 红线在 revert 后尤其要看：6.2 删掉的旧存储不许复活） |
| lint | `cargo clippy --workspace --all-targets -- -D warnings` | 0 警告 |
| 前端（若动了桌面） | `cd apps/acowork-desktop && npx vitest run` | 全绿 |
| 前端（若动了移动） | `cd apps/acowork-mobile && npx tsc --noEmit && npx vitest run` | tsc 0 错、212+ 绿 |

---

## 6. 禁止事项

- **不许**为了"回滚"删除 `chat-sessions/` 目录树——那是全部新词汇表历史的唯一副本。
- **不许**手工改写 `messages.jsonl` 存量行（append-only 是幂等与去重的前提，§4.6）。
- **不许**把 6.2 的导出配方当同步工具长期双跑——它是回滚桥，双写会造出两个真相源。
- **不许**在回滚窗口内启用注册自建号后直接前滚而不重跑 `migrate_legacy`（新 pair 无旧目录可迁，
  重跑是 no-op，但跳过它意味着没人核对 `.migrating` 残留——启动时它自动续跑，别绕开启动路径）。

---

## 7. 版本偏差：Gateway 与 Runtime 混部（v2.8 / ADR-102 起必读）

Intent 信封 `kind` 自 v2.8 起是四值词汇表（`user_message` / `pm_task` / `request` / `response`），消费端由 `TurnOrigin` 策略表解释。**新 Gateway → 旧 Runtime** 的混部窗口里，旧 Runtime 不认识 `pm_task`，按旧词汇表兜底成 responder 语义（agent-to-agent 应对方）：PM 派发轮会被裁掉 `ask_user_question` / `intent_send`，且信封按旧规则渲染成 JSON 进 transcript——正是 v2.8 修掉的行为。

| 混部方向 | 后果 | 处置 |
|---|---|---|
| 新 Gateway + 旧 Runtime | `pm_task` 轮退回 responder 裁剪 + JSON 渲染（功能性回退，不丢数据） | **多节点部署先升 Runtime（或同批），再升 Gateway**；单桌面 bundle 内同二进制版本，无此窗口 |
| 旧 Gateway + 新 Runtime | 旧 Gateway 发 `kind="request"` 的 PM 派发 → 新 Runtime 兜底 `AgentRequest`，行为等同旧 Runtime（不劣化） | 可接受；升级完成后新派发即恢复正确语义 |
| 新 PM + 旧 Gateway | PM 传 `assignment_brief` / `message_id` 字段，旧 Gateway 忽略未知字段 → 派发仍成功，但 playbook 仍在正文里（旧显示行为），重试不幂等（旧 Gateway 每次铸新 message_id） | 尽快补齐 Gateway；窗口内重复派发靠 chat 行 `meta.message_id` 幂等去重兜底 |

回滚同理：`git revert` Gateway 侧 v2.8 提交后，Runtime 若留在新版，PM 派发按 `request` 兜底运行（旧行为），无需同步回滚 Runtime。
