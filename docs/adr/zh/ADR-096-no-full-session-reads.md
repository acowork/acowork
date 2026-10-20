# ADR-096：请求路径禁止全量读会话文件——写时派生状态与有界读取

- **编号**：ADR-096
- **状态**：已接受
- **日期**：2026-10-20
- **决策者**：ACowork 架构组
- **关联**：ADR-092（user chat 有界读取；本 ADR 将其纪律推广到全实现）、ADR-024（会话 meta / token 用量）、ADR-082（SQLite 向量与 FTS 存储）、ADR-084（user 独立进程）、ADR-076（chat 门铃）；设计文档 27 §4.6

## 摘要

聊天记录与执行会话记录都会无限增长。催生 ADR-092 的那次评审实测：10 万行时全文 `read_messages` 约 69 ms；同一类读放大在*另一个*会话表面上同样存在——Agent 执行会话（`acowork-runtime`），以及列会话、数行数、解析幂等 id、取最近一次 compaction 摘要这些请求路径。

本 ADR 确立一条实现级铁律，以及满足该铁律的派生状态模型：

1. **任何 HTTP 请求路径、任何工具读路径都不得对会话文件做全量读**——既不得 `read_to_string` 全文读取，也不得仅仅为了派生行数、seq、幂等 id、尾行预览或 compaction 摘要而逐行全量扫描/解析。同时覆盖 `messages.jsonl`（user chat）与 `{sid}.jsonl`（执行会话）。
2. **一切统计量都来自写时维护的派生状态**——会话 meta（`message_count`、`last_message_preview`、`last_compaction_summary`）、内存态日志索引（带字节偏移的 span 与 data 行计数），或一张小型 SQLite 侧表（`conversation_state`）。
3. **全量扫描只允许作为每进程每会话一次的首触协调 / open 校准 / 迁移 / 启动 backfill**——低频、非实时，绝不发生在每次请求、每条写入时。

wire 契约不变：`total`、`offset`、分页与预览语义逐字节保持一致，前端不改动。

## 背景

### 1. 两个会话表面

- **user chat**（`core/acowork-user/src/chat_sessions.rs`）：`chat-sessions/{id}/messages.jsonl` + `session.json`。经 Gateway 以 HTTP 服务 Desktop/Mobile；空载荷门铃（ADR-076 §决策 8）把每个事件都变成一次读。
- **执行会话**（`core/acowork-runtime/src/conversation.rs`）：`conversations/{sid}.jsonl` + 一份 SQLite `SessionMeta`。在用户分页、增量轮询（`read_messages_since*`）、handoff 路径（最近一次 compaction 摘要）被读，并由后台 tailer（`conversation_index.rs`）索引。

ADR-092 已经修好了 user chat 的*读取*热路径（seq 身份 + 尾部反向分块 seek + `message_count`）。跨两个表面残留的，是与"服务一页"无关的*辅助*全量读：计数、去重查找、预览投影、列表扇出、水印恢复。

### 2. 残留的全量读点（改动前）

| 表面 | 位置 | 为何全扫 | 代价量级 |
|---|---|---|---|
| user chat | `append_message` 每写 ×2 | 重数行数 + 为幂等查找 `message_id` | 每写 O(history) |
| user chat | 会话列表（`chat_api`、`attach_group_previews`、`list_project_groups`） | 读最后一条消息以构造预览 | 每次列表 O(sessions × history) |
| user chat | `read_messages` | `total` 靠数行得到；窗口在全量读后再切片 | 每页 O(history) |
| 执行会话 | `read_messages_paginated` 前向 offset | 解析每一行以跳到某个 offset | 每页 O(history) |
| 执行会话 | `read_messages_since*` | 从第 0 行 `enumerate()` 逐行跳到游标 | 每轮询 O(history) |
| 执行会话 | 最近一次 compaction 摘要（T2 handoff） | 全文件正向扫描 | 每次 handoff O(history) |
| 执行会话 | 索引 `sweep`（每 2s）+ `index_session` | `read_to_string` 全文以找追加行 | 每 tick O(history)，全部会话 |
| 执行会话 | open 时 `recover_watermarks` | `MAX(message_index)` 全表扫 + `NOT IN` 去重 pass | 每次 open O(rows) |

这些都不是*内容*需求，全都是*统计量*——写入方在追加的那一刻本就知道。

## 决策

### D1 — `total` / 行数来自派生状态，绝不靠数行

- user chat：`read_messages` 的 `total` 取自 `session.json` 的 `message_count`，由写路径增量维护。
- 执行会话：`read_messages_paginated` 的 `total` 活动会话取 writer 的 `committed_lines`，冷会话取 `SessionMeta.message_count`。`message_count` 由 open 时一次性数行 pass（D4）校准，之后靠增量维持。

### D2 — 窗口读为尾部反向分块 seek

两个表面都从 EOF 反向分块 seek（初块 64 KiB，遇超长行按需倍增）读一页，只反序列化落在窗口内的行。前向 offset `[K, K+limit)` 换算为 tail-relative（跳过 `total−K−limit` 条）后走同一反向读取器。corrupt 行跳过与 participants 归一逻辑保留；遗留 header 行在校准时一次性剥离重写，不再逐行判断。

### D3 — 增量轮询用字节偏移游标，而非跳行

writer 在内存记录 line→byte-offset 索引（与 `committed_lines` 同步增长）。`read_messages_since` / `read_messages_since_cursor` 把游标行号换算为字节偏移、`seek` 后只读追加字节。删除从第 0 行 `enumerate()` 的跳行循环与 `count_jsonl_lines` 回退分支。

### D4 — 首触协调 / open 校准是唯一允许的全量点

- user chat：一个进程内"已协调"集合（按会话路径键控、每会话加锁）。会话在本进程*首次写入前*做一次全量 pass：校准 `message_count`（覆盖行已落盘但 meta 保存失败的 crash 窗口）、重建幂等 id 集合、补写缺失的 `last_message_preview`（顺带完成旧数据迁移）。之后的追加都是纯增量与 O(1) id 查找。
- 执行会话：会话打开时做一次只数 `b'\n'`（不反序列化）的字节扫描校准 `message_count`，之后全部走增量。每进程每会话至多一次。

### D5 — 幂等 id 是索引集合，而非扫描

user chat 每会话保留一个小型 sidecar `ids.index`（逐行 `message_id\tseq`）加上首触后填充的内存 `HashSet`。写路径查内存（O(1)）；sidecar 在首触时一次性重建，超过尺寸上限后压缩重写（旧 id 在窗口外无重试语义）。

### D6 — 列表表面直接投影存储字段，不碰日志

`last_message_preview` 变为 `session.json` 的写时持久字段（其 doc 语义从"读路径投影"改为"写时派生状态"）。会话列表、群预览附加、项目群列表都直接返回该存储值；`list_project_groups` 遍历既有 `chat-sessions.projects.json` 索引而非扫会话目录。全量 `scan_all_sessions` 只保留在启动 / backfill 路径。

### D7 — compaction 摘要入 `SessionMeta`

`append_compaction_event` 写盘时把摘要文本存入 `SessionMeta.last_compaction_summary`，与既有 offset 并存。handoff 读为 O(1)。对于 meta 早于该字段的会话，首次读做一次 legacy 扫描并把值自愈回写进 meta；无 meta 行的会话返回 `None` 而非每请求重扫（活动会话必有 meta）。

### D8 — 会话索引器保留 `conversation_state` 尾指针

- 新 SQLite 表 `conversation_state(session_id, next_line, byte_offset)`，与索引写入同事务更新。
- `sweep` 用 `fs::metadata().len()` 与持久 `byte_offset` 比较：未增长的日志零内容读直接跳过；有增长时只从 `byte_offset` 往后读。`index_session` 不再做全文 `read_to_string`。
- open 时 `recover_watermarks` 改读 `conversation_state`（O(#sessions)），不再全表 `MAX`/`NOT IN` 扫。junk/去重清理移入低频后台维护任务（写路径本身已是幂等 upsert，正确性不依赖它）。升级老库（有索引行但无 state 行）触发每会话一次性重建，作为后台迁移。

### D9 — 写路径代价保持 O(1)；协调不是每写都做

writer 提交追加行时把已知的 `(offset, len)` span 直接 push 进日志索引——O(1)，不重开文件、不重扫。仅在 offset desync 或外部截断（罕见）时回退全量协调。每写重开重扫被明确否决：它拖慢异步 writer 以致与裸文件测试读产生竞态，并把 O(history) 代价重新引入写热路径。

### D10 — 强制方式：红线守卫，而非约定

`dev/ci.sh` 新增 `run_session_full_read_redline`。它扫描两个日志属主文件，在非白名单函数体内出现 `read_to_string` / `count_jsonl_lines` 时失败（白名单为首触 / 校准 / 迁移 / 扫描 / sidecar / meta 加载函数，跳过测试模块）。随调用点移除，上限只会收紧。

## 后果

**正面**

- 读写代价不再随历史长度增长：页读为 O(页 + 窗口字节)，增量/轮询为 O(追加字节)，计数/预览/摘要为 O(1)，列表扇出不读日志，索引 sweep 为 O(追加字节)、open 为 O(#sessions)。
- "禁止全量读会话"这一性质由 CI 红线强制，并有单测背书（10k 合成行的前向/尾部换算等价、字节游标 since 读、compaction meta 往返、`conversation_state` reopen 恢复），不会静默回退。
- 前端与 wire 契约不受影响。

**负面**

- 写路径现在维护若干派生状态存储（session meta 字段、内存日志索引、`ids.index`、`conversation_state`）；每个都是新状态，其正确性由首触协调 / open 校准与每会话锁固定。
- 崩溃窗口一致性策略从"每次写全量校准"改为"每进程首触校准一次"；行已落盘但 meta 保存失败的崩溃，在下一进程首触时愈合，而非下一次追加时。
- SQLite schema 新增 `sessions.last_compaction_summary` 列与 `conversation_state` 表（migration 5）。

## 已考虑的替代方案

1. **保留每写全量重校准**（改动前）——否决：每写两次 O(history) pass 正是铁律禁止的放大，且随最大线程规模增长。
2. **现在就迁移 chat 到 SQLite**——否决：ADR-092 决策 7 将其推迟到内容检索或多进程写成为需求时；派生状态模型在扁平文件上即提供 O(1) 统计，无需该代价。
3. **每请求惰性计数 + 缓存 total**——否决：任何"需要时才数"的缓存会在每进程首次 miss 时重开 O(history) 路径；计数属于写路径，在那里它是免费的。
4. **每写重开并全扫文件以协调日志索引**——按 D9 否决（写热路径回退 + 读者竞态）；在 committed-lines 不变量下，直接 push span 既 O(1) 又同样正确。

## 参考

- ADR-092（user 消息身份与有界读取——本 ADR 把其读纪律推广到两个表面的每一处辅助会话读）
- ADR-024（会话 meta + token 用量；`SessionMeta` 派生字段落于此）
- ADR-082（SQLite 向量 + FTS 存储；承载 `conversation_state`）
- ADR-084（user 独立进程）、ADR-076（chat 门铃——为何列表/读路径高频）
- 设计文档 27 §4.6（串行化 chat 会话写入——现亦为写时预览契约）
