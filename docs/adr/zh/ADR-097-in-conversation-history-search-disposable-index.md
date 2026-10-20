# ADR-097：会话内聊天记录分类查询——每会话可弃搜索索引

- **编号**：ADR-097
- **状态**：已接受
- **日期**：2026-10-20
- **决策者**：ACowork 架构组
- **关联**：ADR-096（请求路径禁止全量读会话——本 ADR 是它「内容检索需求到来」的兑现触发点）、ADR-092（user chat 有界读取与 per-session 单写者）、ADR-082（SQLite FTS 存储与 trigram 约定）；设计文档 27 §4.6

## 摘要

微信式的「在这段对话里按 文本 / 图片 / 文件 翻记录」需要一个可查询的索引。ADR-096 把「迁 chat 到 SQLite」推迟到*内容检索成为需求时*——本期正是该触发点。但检索需求不能反过来破坏 ADR-096 的铁律：搜索**绝不**能在请求路径上扫 `messages.jsonl`。

本 ADR 确立一个**每会话、可弃、非承重**的搜索索引模型：

1. **搜索索引与分页脊柱解耦。** 新增独立文件 `{data_dir}/chat-sessions/{sid}/search.sqlite`；`lines.index` 原样不动，仍是分页承重件；`messages.jsonl` 仍是唯一事实源。
2. **写时增量维护、查询只读侧表。** `append_message` 在既有 per-session `session_lock` 内顺带更新 `search.sqlite`；`search_messages` 只读它，永不打开 `messages.jsonl`。
3. **可弃且自愈。** 侧表损坏/缺失/说谎只降级搜索本身，最坏触发一次性重建，绝不牵连聊天读写。重建是唯一被豁免的全量读——与 `ensure_index` 首触同级，非每请求、非每写。

## 背景

- **依赖现状**：`acowork-user` 此前对 SQLite **零依赖**（[chat_sessions.rs](../../../core/acowork-user/src/chat_sessions.rs) 只有 fs/JSONL/`lines.index`）。本期为搜索引入 `rusqlite`（workspace 级 0.37，bundled，FTS5 + `trigram` tokenizer 可用），与 `acowork-sqlite` 同源同版本。
- **不能靠扫 log**：若搜索走 `messages.jsonl` 全量读，直接违反 ADR-096 红线（`run_session_full_read_redline`）。因此必须有一个独立的、写时维护的查询面。
- **为什么不并入 `lines.index`**：`lines.index` 是承重的分页脊柱（零依赖、被读热路径每请求使用）。把可弃的全文搜索塞进同一个文件会让「搜索坏」升级为「聊天坏」——两者损坏语义、重建代价、锁域都不同，必须分开。

## 决策

### D1 — 每会话一个可弃 `search.sqlite`，不是全局单库

搜索索引以**会话目录**为粒度：`chat-sessions/{sid}/search.sqlite`。否决「整个 acowork-user 一个索引 DB」：

- ADR-092 的不变量是 **per-session 单写者、非全局串行点**；单一全局 DB 会把所有会话的写 serialize 到一个文件锁上，与该不变量冲突。
- 会话作用域本就是查询作用域（本期只做单会话内查询），per-session 文件与查询边界天然对齐。
- 删除生命周期一致：`delete_session` 删会话目录时连 `search.sqlite` 一并删（目录级清理），无需跨库引用计数。**顺序是契约的一部分**：必须先释放缓存的句柄（`search_index::forget`）再 `remove_dir_all`——Windows 上 SQLite 文件不带 `FILE_SHARE_DELETE`，句柄未关时删目录会因共享冲突失败，把「可弃侧表」变成「删不掉会话」。

### D2 — schema 与坐标

- 单张 FTS5 表 `search`（内嵌 content，`tokenize = 'trigram'`）：主列 `body`（全文），其余 `seq / ts / from_id / kind / cat / attachments` 全 `UNINDEXED`；`rowid = line_pos`。
- `line_pos` 与 `lines.index` **同一坐标**的 0-based 升序物理行号（blank 行占字节不占位、corrupt 行占位不入库），保证前端把命中行换算成 `read_messages` offset 时两套索引一致。
- `cat` 是位掩码：bit0 文本（speech 且 body 非空）、bit1 图片（附件 mime `image/*`）、bit2 文件（其余附件）；一行可携多类。附件 JSON 存 `attachments` 列供文件类目按 filename 命中、前端渲染缩略图/文件条。
- `meta(indexed_lines)` 存水位线：已索引的物理行数，用作一致性判据。

### D3 — 写路径：best-effort 增量，失败不回滚消息

`append_message` 在 `commit_row` 之后调 `index_append(dir, line_pos, msg)`：水位线恰好等于 `line_pos` 时开事务 `INSERT` + 抬水位；否则退化为一次 `rebuild`。**索引写失败只记 warn，绝不 `?` 上抛**——log 才是事实源，消息落盘成功即算写入成功；侧表落后由下次查询/写的首触重建愈合。搜索永远是聊天的一层可弃附件。

### D4 — 重建是唯一 sanctioned 全量读，靠水位线触发

`ensure(dir, log_line_count)`：打开句柄后，若 `meta.indexed_lines ≠ 权威行数`（fresh / 说谎 / 崩溃窗口 / 拷贝或缺失文件）→ 从 `messages.jsonl` 整体重建一次并抬水位。这与 [ADR-096 D4](./ADR-096-no-full-session-reads.md) 的首触协调同级豁免——每进程每会话至多一次，非每请求、非每写。打开失败（corrupt）时删侧表 + `-journal` 重试一次，从干净重建。

### D5 — 查询：三类目共用游标分页，只读侧表

`search_messages(data_dir, sid, category, q, before_line, limit)` → 取 `session_lock` → `ensure_index` 拿权威 total → `search_index::ensure(total)` → `query`：

- **Text**：`q` 词元 ≥ `TRIGRAM_MIN`（=3，与 `acowork-sqlite` 一致）走 FTS `MATCH`（phrase），短词/退化场景回退 `body LIKE`；`q` 空则列全部 `cat & text`。
- **Image**：`cat & image`，`q` 被忽略。
- **File**：`cat & file`，`q` 命中 `attachments` 的 filename 子串。
- 统一 `ORDER BY rowid DESC`，`before_line` 作向更旧翻的游标（`rowid < before_line`）。返回 `hits / total_lines / next_before_line`（满页则末行 `line_pos` 为续翻游标，否则 `None`）。

### D6 — HTTP 与权限：与读消息同一道参与校验

`GET /api/chat-sessions/{sid}/search`，参数 `category(text|image|file，缺省 text)`、`q`、`limit(默认 DEFAULT_PAGE、封顶 MAX_PAGE)`、`before_line`。权限镜像 `read_messages_guarded`：非参与者与未知会话一律 `not_a_participant` / not-found，由 offload 映射成隐藏 404；admin 可搜自己未加入的项目群。Gateway 的 `user_proxy` 对 `/api/chat-sessions/{*rest}` 是通配转发，**新增子路径无需改 Gateway**。

### D7 — 强制方式：搜索写失败不得影响消息、查询不得读 log

以单测固定而非约定：① append→search 往返一致；② image/file mime 正确分列；③ FTS 命中 + 短词 `LIKE` 回退；④ `before_line` 游标续翻；⑤ 删/损坏 `search.sqlite` → 首触重建 → 结果与逐行等值；⑥ **搜索写失败不影响消息落盘**；⑦ 非参与者被拒。`search_messages` 全程不出现 `messages.jsonl` 读（除 D4 重建），不触 ADR-096 红线。

## 后果

**正面**

- 单会话内三类记录查询成立，点击命中项可凭 `line_pos` 换算 offset 定位到消息（前端跳转见设计文档 27 §6 的窗口游标改造）。
- 搜索是彻底可弃的旁路：坏、缺、落后都只降级搜索，聊天读写零牵连；读热路径仍不碰 `messages.jsonl`。
- 与 ADR-096 正交：本期不并入 `lines.index`、不改分页脊柱、不动 wire 契约；分页语义逐字节不变。

**负面**

- `acowork-user` 新增 `rusqlite` 依赖与一个派生状态存储（`search.sqlite` + `meta` 水位线），其正确性由水位线 reconcile 与每会话锁固定。
- 崩溃窗口：一条已落盘但索引写失败的行，在下次 `ensure`/`index_append` 触发重建时愈合，而非该次写入时。
- 本期只做单会话内关键词/类目检索；语义/拼音检索、跨会话（Inbox 用户↔用户）检索分别留给后续（接 `acowork-embed`）与 Phase 5。

## 已考虑的替代方案

1. **整个 acowork-user 一个全局索引 DB**——否决：违反 ADR-092 per-session 单写者（全局文件锁串行化所有会话写），且删除/作用域不对齐（D1）。
2. **把搜索并入 `lines.index`**——否决：让可弃搜索与承重分页共生死，「搜索坏」升级成「聊天坏」；两者损坏语义与锁域不同。
3. **搜索时扫 `messages.jsonl`**——否决：直接违反 ADR-096 红线。
4. **把 `messages.jsonl` 整体迁成 SQLite 主存**——否决：ADR-096 替代方案 2 已把该迁移推迟到「多进程写成为需求时」；本期是只读检索需求，用一个可弃派生索引即可满足，不值得改动事实源与分页脊柱。

## 参考

- ADR-096（请求路径禁止全量读会话——本 ADR 兑现其「内容检索到来时可引入小型 SQLite 侧表」的预留，并复用其首触重建豁免）
- ADR-092（user chat 有界读取、per-session 单写者——搜索写落在既有 `session_lock` 内，无新增全局锁）
- ADR-082（SQLite 向量 + FTS 存储、`TRIGRAM_MIN` 约定——本索引沿用同一 trigram 词元下限）
- 设计文档 27 §4.6（chat 会话存储与读纪律）
