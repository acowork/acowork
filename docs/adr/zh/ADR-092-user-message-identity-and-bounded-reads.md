# ADR-092: 用户消息的稳定身份与有界读取 — user chat 存储与刷新模型重构

- **编号**: ADR-092
- **状态**: Accepted
- **日期**: 2026-10-12
- **修订**: v2（评审修订）——决策 3 崩溃自愈由"读路径重编号"改为"追加时尾检"（F1）；决策 5 增补用户级 index 锁（F2）；决策 1/3/4 钉死物理行号基准与计数器初始化点（F3/F4）；背景 §1 布局草图更正（F5）；决策 3 锁范围收窄、决策 4 补实现注记、决策 5 注明重建成本（F6/F7/F8）
- **决策者**: ACowork 架构组
- **关联**: ADR-076（修订其 §决策 8 v2 第 3、4 条）、ADR-084（user 独立进程）、ADR-050（聊天列表数据驱动）、ADR-082（SQLite 选型先例）；设计文档 25 §8

## 摘要

用户↔用户聊天子系统当前的三类缺陷（刷新误判、并发丢失、读放大）共享同一个根因：**消息没有稳定身份，读取没有有界路径**。客户端被迫用计数差去"猜测发生了什么"，服务端被迫用全文件扫描去"回答任意一页"。本 ADR 决定：给 `ChatMessage` 增加服务端分配的线程内单调序号 `seq`；客户端刷新规则从 total-delta 启发式改为 **seq 后缀差**（内容判定，非计数猜测）；服务端热路径读取改为**尾部反向分块 seek**；`conversation.json` 增加派生计数器 `message_count` 并以**每线程写锁**统一所有读改写，崩溃窗口的计数漂移由**追加时尾检**自愈（v2 修订）；push/poll 边沿处理折叠为一个状态机。门铃信号的隐私契约（空载荷）与"HTTP 是唯一数据源"契约不变。

## 背景

### 1. 现状（代码事实）

存储形态（`core/acowork-user/src/chat.rs`；v2 更正——原草图 `pair_{min_id}__{max_id}/` 有误，实际布局如下）：

```
data_dir/
  users/{lo}/chats/{hi}/          # lo/hi = 两个 user_id 的字节序排序（pair_dir）；wire 上的 chat_id 为 `{lo}__{hi}`
    conversation.json   # participants, last_active_at, unread(BTreeMap), version
    messages.jsonl      # append-only，一行一条
    files/{id} + files/{id}.json  # 附件 blob 与元数据（ADR-076 §决策 9）
```

`ChatMessage` 字段为 `{ts, from, kind, body, attachments}` —— **没有 id，没有 seq**。`Conversation` 没有消息计数。读路径 `read_messages` 是 `read_to_string` 全文件 + 逐行解析后切片；`list_for` 每次全量扫描 fleet 的 pair 目录；`append_message` 对 `conversation.json` 是无锁的 load→改→save。

### 2. 触发事件：门铃把 O(全历史) 的代码推上了热路径

ADR-076 §决策 8 v2 引入空载荷门铃后，消息读取频率从"5s 兜底轮询"提升到"每个事件一次"。同时本轮代码 review 确认了两个根因：

- **根因 ①（客户端，结构性）**："上沿 ⇒ 恰好一次主动重读"这条不变式（§决策 8 v2 第 3 条定义的唯一纠偏点）在 Desktop `chatStore`、Desktop `userChatStore`、Mobile `presence` 三处各自手写。注释声明了契约，但注释不是执行机制——Mobile 已经漂移（`reportChat(true)` 上沿缺 catch-up wake，违反 ADR 自身契约）。
- **根因 ②（数据模型，缺失身份）**：因为消息没有稳定身份，客户端只能用 `d = total − knownTotal` 猜测"发生了什么"。每一个猜测都是一个 bug 族：老服务端（无 `total`）路径下 unseen 计数膨胀整页长度、`ackedThrough` 用 ts 做身份导致同秒已读回执碰撞、send echo 与 wake 重读双加、`sameMessages` 引用相等恒 false 导致无谓 resync。

### 3. 实测数据

10 万条消息 / 12 MB 的 `messages.jsonl`（Windows，release profile）：

| 操作 | 耗时 |
|------|------|
| `read_messages` 现状（全文件读 + 全量解析） | ~69 ms |
| 仅数行（找换行符，不解析） | ~10.8 ms |
| 尾部 64 KiB 反向分块 seek（本 ADR 决策 4） | ~1.4 ms |

`append_message` 的 JSONL 追加本身是 O(1)（`O_APPEND` 单写），瓶颈全在读侧与元数据侧。

### 4. 为什么 agent 会话的"分块机制"救不了 user chat

分块加载有三个正交层：L1 客户端协议分块（默认拉尾部、上翻再拉）、L2 服务端物理读分块（每次请求只碰那一页的字节）、L3 文件本身有界。微信的分块成立靠 L2（会话库按 (thread, seq) 索引 seek，代价与历史长度无关）。我们的 agent 会话其实**没有 L2**——`read_messages_paginated`（`core/acowork-runtime/src/conversation.rs`）同样是全文件读入后切片——它活得很好是因为 L3 兜住了：compaction + `.jsonl.archive` 归档 + `prune_excess_sessions` 把文件压成有界，且只有用户翻页才读。

user chat 两个前提都没有：产品承诺"翻旧账"，历史不可删（无 L3）；门铃让每个事件都触发读（高频）。所以必须补 L2，而不是照搬"agent 也是这个机制"的表象。

## 决策

### 决策 1：`seq` — 服务端分配的线程内单调序号

- `ChatMessage` 增加 `seq: u64`（wire + 持久化各一处）。由 `append_message` 在**每线程写锁**（决策 3）内分配：`seq = message_count + 1`（`message_count` 先经追加时尾检校准，见决策 3 v2），从 1 起，严格递增。
- 历史行无 seq：`#[serde(default)]` 读为 0，`read_messages` 按 1-based 行号回填。回填合法，因为历史行按时间顺序追加、ts 非降——行号就是一个有效的 seq。不做重写迁移。
- **行号基准钉死为物理行号（v2）**：`read_messages` 现状会跳过空行与损坏行，回填、`message_count` 与决策 4 的 offset/行数统计必须统一按**物理行**计（损坏行占号），而非按解析成功的消息下标计——否则存在损坏行时 append 分配的 seq 与回填值错位。seq 不要求稠密：损坏行留下永久空洞，作为身份合法；副作用是决策 2 的空洞判定（`page[0].seq ≤ heldMaxSeq+1`）在损坏行边界会触发一次安全的 resync，需契约测试钉住。
- wire 面：`GET …/messages` 每条带 `seq`；`POST …/messages` 的 echo 响应带 `seq`。`total` 字段保留，但**降级为可选优化**（展示、判断可上翻量），不再参与正确性判定。

### 决策 2：客户端刷新规则 = seq 后缀差（修订 ADR-076 §决策 8 v2 第 4 条）

`planPage(held, page)` 改为：

```
heldMaxSeq = max(seq in held)   // 空窗 = 0
append     = page.filter(m ⇒ m.seq > heldMaxSeq)

append 为空            → none（内容级判定，"重新解析但内容未变"不再误触发 resync）
无空洞（held 空，或 page[0].seq ≤ heldMaxSeq+1） → 追加 append，unseen 恰为 append.length
有空洞                → resync，unseen = page 中 seq > ackedSeq 的条数（仍是精确值）
```

配套细则：

- `ackedThrough: Map<peer, seq>` 取代 ts 版——同秒已读回执碰撞按构造消失。`markThreadRead` 确认到 `heldMaxSeq`。
- `send()` 直接追加服务端 echo（携带分配的 seq）；随后自己的门铃触发的 wake 里 `seq ≤ heldMaxSeq` → none，**天然幂等**，不再需要 `messagesTotal` 记账。
- 旧服务端降级：响应缺 `seq` 时（Mobile 可能连到未升级的 Gateway），退回 §决策 8 v2 第 4 条的 total-delta 规则，标注 deprecated；Desktop 与 Gateway 同包发布，不受影响。**注意：降级路径上计数猜测的 bug 族原样存续——这是对旧行为的显式延续，不是修复**；身份无法凭空补出，彻底消除以服务端升级（决策 1）为前提。
- "不得位移"纪律原样继承：页恒为"最新 N 条正序"，追加只动尾部，已渲染气泡引用不变。

### 决策 3：`message_count` 派生计数器 + 每线程写锁 + 追加时尾检自愈（v2 修订）

- `Conversation` 增加 `message_count: u64`，基准为**物理行数**（与决策 1 一致）。`read_messages` 的 `total` 直接取它，O(1)，不再数行。
- **初始化点覆盖任何 load 路径（v2 钉死）**：`read_messages`、`append_message` 的 load、load 失败后 `Conversation::new` 的重建分支——任何路径遇到 `message_count = 0 且 messages.jsonl 非空`，都先按物理行数一次性初始化（每线程一次 O(N)，此后 O(1)）。漏掉 append 路径会让升级后第一条消息拿到 seq=1，与历史行 1 碰撞。
- **每线程写锁**：按 pair 目录键控的互斥注册表。`append_message`、`mark_read` 等所有对 `conversation.json` 的读改写必须在同一把锁内完成。review 发现的并发发送丢 unread 计数的问题由锁修复，不由写入顺序技巧修复。（v2 收窄：附件 sidecar 写入不改 `conversation.json`，不在本锁范围内；其对 append 的可见性由锁内 `resolve_attachments` 读取保证——未完成上传的 id 解析失败即报错，客户端重试。）
- **崩溃窗口自愈 = 追加时尾检（v2 修订，取代原"读路径重编号"方案）**：JSONL 追加成功、meta 保存失败（`save` 是 tmp+rename 原子写，meta 只会回退为旧值、不会撕裂）→ `message_count` 落后于物理行数。原方案"读路径检测到 seq 为 0 或不单调时按行号重编号"经评审证明不成立，两个原因：① 尾部窗口的异常检测是**窗口局部**的——窗口滑过重复行之后，磁盘上偏移的 seq 以"单调"面貌对外服务，与此前重编号后已下发的 seq 冲突，同一 seq 跨两次读取指向不同消息，且 `planPage` 的"append 为空 ⇒ none"会把新消息静默吞掉；② 重编号需要绝对行号，而绝对行号依赖的正是被崩溃破坏的 `message_count`——修复路径循环依赖被修复对象。修正为把自愈点移到**写路径**：
  - `append_message` 在锁内、分配 seq 前，用决策 4 的机制**反向尾读最后一条可解析行**（起始 4 KiB，亚毫秒）：若 `last_seq ≥ message_count` 则令 `message_count = last_seq`；随后分配 `seq = message_count + 1`。
  - 效果：**重复 seq 永不落盘**，磁盘 seq 恒等于物理行号，任何崩溃造成的计数漂移在下一次 append 即追平。历史 seq=0 行使 `last_seq = 0`，永不触发修复，与决策 1 的回填天然兼容。
  - 同一次尾读顺带校验**文件是否以换行结尾**：若上次崩溃留下撕裂行，先补一个 `\n` 再追加——否则新消息会拼接进垃圾行，一次损失两行（现状代码的既有缺陷，尾读顺带加固，零额外成本）。
  - 读路径职责相应收窄：只负责历史 seq=0 行按物理行号回填（决策 1）；"不单调"检测保留为**损坏告警**（warn 日志），正常运行不触发。原"已接受风险：客户端按 seq 去重"条款删除——修复后该窗口不存在。

### 决策 4：有界读取 — 尾部反向分块 seek

- `offset = 0`（热路径，门铃 wake 全走这里）：从 EOF 反向读块（起始 64 KiB，×8 扩展直到拿到 ≥ limit+1 个换行或触到文件头），只解析尾部 ≤ limit 行；`total` 来自 `message_count`。实测 ~1.4 ms @ 100k 条（全扫 ~69 ms）。
- `offset > 0`（上翻分页）：继续反向 seek，按换行扫描跳过 `offset + limit` 行——字节扫描，不 JSON 解析。代价 ∝ 翻过的字节数，符合"极少翻到第 5 万条"的访问分布。
- 上界保证：**任何路径都不再出现 O(全部消息) 的解析/分配**；最坏（翻到最早页）退化为 O(文件字节) 的换行扫描。
- 实现注记（v2）：行数与 offset 的基准一律为**物理行**（与决策 1/3 一致）；反向分块会切断 UTF-8 多字节字符（中文 body 是常态），块边界的残行必须拼接后再解析——JSONL 只写 `\n`，无 CRLF 歧义。跨块中文消息必须有单测覆盖。

> **ADR-096 后续（契约注记）。** 本决策的 `read_messages` 契约形状不变（反向尾部分块窗口读、`total` 取自 `message_count`）。ADR-096 把同一“禁止全量读会话”纪律推广到 ADR-092 未盖到的*辅助*读，并调整两个频次：① `message_count` / seq 崩溃自愈从“每次写的追加时尾检”改为“每进程一次的首触协调”（进程内首次写入做一次全量 pass，之后追加都是纯增量）；② 幂等 id 成员判定从每次写全量扇描，改为首触 pass + 内存集合，由一个小型 `ids.index` sidecar 做后端；③ 列表预览（`last_message_preview`）从读路径投影改为会话 meta 的写时持久字段。读路径本身不受影响。

### 决策 5：`chats.index` — list_for 从 O(fleet) 到 O(自己)（v2 修订：用户级 index 锁）

- 每用户维护 `users/{uid}/chats.index.json`：反规范化的线程摘要数组（`chat_id, peer, last_active_at, last_message_preview, unread`）。随每次变更（append / mark_read）同步更新两位参与者的 index。
- **index 写入加第二把按 uid 键控的用户锁（v2 修正）**：加锁顺序固定为**线程锁 → 用户锁**（单向层级，无死锁）。仅有每线程写锁是不够的：线程 A-B 与线程 A-C 持**不同的**线程锁，却都读改写 `users/A/chats.index.json`——并发下整文件重写丢更新，产出**静默陈旧但外观完好**的 index，"损坏即重建"的兜底不会触发，"目录是真相"也救不了一个看起来健康的坏缓存。index 文件复用 `save` 的 tmp+rename 原子写，`list_for` 无锁读安全（只会读到旧值或新值，不会撕裂）。
- `list_for` 只读自己的 index 文件。index 缺失或损坏 → 回退全目录扫描并重建（首次启动即迁移路径）。**目录是唯一真相，index 是可丢弃缓存**——这条定位使它永远不会成为第二个需要一致性维护的真相源。重建成本为 **O(fleet)**（用户作为 `hi` 方的线程散落在其他用户的目录下，须全扫才能收齐），等于现状 `list_for` 的单次代价，作为低频兜底可接受（v2 注明）。

### 决策 6：push/poll 边沿处理折叠为一个状态机（客户端，两端，修订 ADR-076 §决策 8 v2 第 3 条的执行机制）

- 把"上沿 ⇒ 恰好一次主动重读"从三处手写折叠进 `setChatPushChannel(up)` 内部：`up` → 停兜底轮询 + 发一次 coalesced `wake()`；`down` → 重新武装轮询。调用方（三个边沿来源）退化为**只上报事实**，不再各自决定刷新行为。
- Desktop `applyConnectionTransition` 的显式 wake 移除；Mobile `presence.reportChat` 的缺失随结构收敛自动修复——不是补一行补丁，而是让漂移在类型上不可能。
- 两端各加**转移表契约测试**钉住不变式：`down→up` 恰好一次 wake；`up→up` 零次；`up→down` 重新武装轮询。
- push/poll 互斥原则（v2 第 5 条）不变，只是执行点收拢到状态机内。
- Desktop 与 Mobile 目前无共享 TS 包：过渡期保留两份同构实现 + 镜像契约测试；共享包成立后把 freshness 状态机与 `planPage` 收拢为单一实现（本 ADR 记录意图，不承诺时间表）。

### 决策 7：演进路径（明确暂不做）

- **滚动分段**：单线程文件超阈值（初值 10 MB）时把最老段封存为 `messages.{n}.seg.jsonl`，读取跨段拼接，**永不删除**（"翻旧账"是产品承诺，L3 在这里只能是"变冷"不能是"丢弃"）。触发条件：决策 4 落地后的分页 p99 延迟或文件大小阈值。
- **SQLite**：当出现**内容搜索**或**多进程写**需求时，按 ADR-082 先例迁 `acowork-sqlite`（FTS5 / 索引是这两类查询的唯一解，平面文件给不了任何 O(log n) 答案）。本 ADR 拒绝预支该成本。

### 不变的契约

- 门铃保持**空载荷**（ADR-076 §决策 8 v2 第 2 条：`chat_id` 进门铃 = 把社交图谱发布出去，隐私决策不因重构动摇）；
- **HTTP 响应是唯一真相**，信号不携带内容、不缓存；
- 持久化格式仍是 JSONL + `conversation.json`，无重写迁移。

## 后果

**正面：**

- 客户端刷新决策全部基于内容（seq 后缀差），四个 bug 族（unseen 膨胀、同秒碰撞、echo 双加、误触发 resync）按构造消失，而非逐个打补丁；
- 读取代价不再随历史长度增长：热路径 O(页大小)，上翻 O(翻过的字节)；
- 边沿不变式由机制（状态机 + 契约测试）而非注释保证，第四处漂移点无法出现；
- 并发发送不再丢 unread 计数；`list_for` 不再随 fleet 规模退化。

**负面：**

- wire 模型变更：messages 响应与 send 响应新增 `seq`（向后兼容：旧客户端忽略未知字段；新客户端需保留 legacy 降级分支）；
- 写路径多一个派生字段（`message_count`）、两个锁注册表（线程锁 + 用户 index 锁）与每次 append 一趟亚毫秒尾读：注册表本身是新状态，需要随 pair 目录 / 用户生命周期清理；
- 两端仍是两份同构 TS 实现，契约测试只是防漂移的机制而非消除重复的方案。

**风险：**

- ~~崩溃窗口内 seq 重用（决策 3）~~——v2 已由追加时尾检消除：重复 seq 不落盘、计数漂移在下一次 append 追平；残余代价仅为每次 append 一趟亚毫秒尾读；
- `chats.index` 与目录扫描分叉——缓解：index 定位为可重建缓存，损坏即重建，不做双真相源同步。

## 替代方案（考虑但未采用）

1. **补丁级修复**（给 Mobile 补一行 wake、给 unseen 膨胀加 clamp）——拒绝：都是症状级修复；不变式仍无执行机制，计数猜测模型仍会持续产生新 bug 族。本 ADR 的直接动因就是 review 中"补丁方案被用户驳回"。
2. **直接迁 SQLite**——拒绝：当前需求（追加 + 尾部/分页读）JSONL + 有界读取完全够；搜索是唯一真正需要索引的需求，尚不存在。预支迁移与双写成本违背 YAGNI，路径已在决策 7 留门。
3. **每条消息 UUID 而非 seq**——拒绝：UUID 无序，后缀差与 `ackedThrough` 仍需依赖 ts 排序，同秒碰撞问题原样保留；线程内单调整数是最小充分身份。
4. **客户端按内容哈希去重**——拒绝：同秒相同 body 是合法消息（连发两条"好的"），内容不能做身份。

## 参考

- ADR-076 §决策 8 v2（门铃与刷新策略；本 ADR 修订其第 3 条的执行方式与第 4 条的规则本体）
- ADR-084（user 独立进程；门铃 fire-and-forget 纪律同源）
- ADR-050（聊天列表数据驱动重构）
- ADR-082（记忆存储迁 SQLite；决策 7 的选型先例）
- 设计文档 25 §8（多端消息同步与门铃）
- 本轮代码 review 记录（两个根因与实测数据出处）
