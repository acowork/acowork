# ADR-117：PM 存储演进 — 树存储仍为权威，先修读路径，SQLite 迁移按触发条件启动

**状态**：已定案（读路径修复 + D4 防损坏三层已实施；SQLite 迁移未启动，触发条件见 §3 D5）
**日期**：2026-11-16
**决策者**：大鱼
**前置**：
- [ADR-061：acowork-pm 存储选型 — 目录树 + 物理嵌套即权威 + 零冗余字段](./ADR-061-pm-storage-tree.md)
- [ADR-082：记忆存储后端迁移至 SQLite](./ADR-082-memory-storage-sqlite-vector-fts.md)（迁移方法论与教训来源）
- [ADR-064：PM 独立进程](./ADR-064-pm-standalone-process.md)

---

## 1. 决策摘要

### 1.1 一句话

**不迁 SQLite，先把读路径修对。** 树存储（ADR-061）继续作为 PM 的唯一权威；`SQLite` 被定义成一份**带量化触发条件的迁移预案**，而不是「以后再说」。

### 1.2 关键决策表

| # | 决策 | 结论 |
|---|---|---|
| D1 | 存储权威 | **树存储保留**。`task.json` / `project.json` + 物理嵌套仍是唯一真相，ADR-061 的四条理由（字面=逻辑 / 单一原子目录操作 / 零冗余无双写 / walkdir 重建幂等）全部继续成立。本轮不引入第二存储 |
| D2 | 读路径 | **读必须走内存索引**。任何「拿索引筛完再用 `get_task()` 补一刀」的实现都视为缺陷：索引已经持有该字段时，读盘是纯浪费。已修 `compute_blocked_by` 与项目徽章计数 |
| D3 | 新契约 | 新增 `GET /projects/counts`（index-only，返回每项目 `total` / `submitted`）。首页徽章不再 N+1 拉全量任务实体 |
| D4 | 防损坏 | **与存储格式无关，已实施（见 §5）**。三层：坏文件**留原地**并复制取证副本到 `.corrupt/`、写前保留上一版合法内容（`.bak`）、**shell 工具受保护前缀守卫**（`~/.acowork/`，路径由 `acowork-core::workspace` 单点提供）。第三层是真正的止损点，前两层只保证「坏了不丢、能恢复」 |
| D5 | 迁移触发 | SQLite 迁移**只在下列任一条件成立时启动**，见 §3 D5 的量化门槛。不因「感觉会变慢」而启动 |
| D6 | 迁移方式 | 若触发：照 ADR-082 的路子——`PmStore` trait 边界不动，新增 SQLite 后端，**双后端测试双跑**、灰度切换、再删旧实现。禁止一次性重写 |

### 1.3 明确不做

- 不把附件（`attachments/`）迁进 DB。二进制永远留在文件系统，元数据在 `task.json`（ADR-061 决策 5 不变）。
- 不在本轮引入「树 + SQLite 双写」。双写是双份真相，违反 ADR-061 零冗余的初衷。
- 不为防损坏而换存储格式（理由见 §2.3）。
- 不给读路径加 TTL 缓存。缓存会把「索引已经不是真相」这个前提引入，收益远小于一致性风险。

---

## 2. 背景与问题

### 2.1 导火索一：单文件损坏导致整个 PM 起不来

`projects/p-66e68d1a/tasks/t-96685f5d/task.json` 尾部多出 3 个字符（JSON 在第 24 行已闭合），`walk_tasks` 用 `read_json(...)?` 把单文件解析错误冒泡成 `rebuild_index` 失败 → `PmService::build` 失败 → PM 启动即退出 → 前端「项目全空」。

已修（两层）：

1. **容错**：`walk_tasks` / `rebuild_index` 对目录、条目、ID、JSON 全部降级为 `warn/error + continue`，日志带 project / task / 绝对路径。日志汇总改为 `PM index rebuilt count=N skipped=M`。
2. **确认自我写入不可能产生坏 JSON**：PM 所有写路径统一走 `atomic_write_json`，内容是 `serde_json::to_string_pretty` 的输出，结构上必然是合法 JSON；目录内也没有 `.tmp` 残留。**因此损坏来自外部进程直接写入 PM 数据目录。**

### 2.2 导火索二：点 PM 导航超过 1 秒（冷缓存）

这不是存储格式问题，是读路径每次都重新打盘，而且为两个整数把全量数据拉了一遍。实测账单（真实库：4 项目 / 83 任务 / 143 个 JSON / 1.6 MB）：

| 步骤 | 位置 | 磁盘打开 |
|---|---|---|
| `GET /projects` → `list_projects()` 每次 `read_dir` + 逐项目 `read project.json` | `store/tree.rs` `list_projects` | 4 |
| 前端对**每个项目**调 `listProjectTasks` → `find_tasks()` 索引筛完后逐条 `get_task()` | `apps/acowork-desktop/src/stores/pm/projectStore.ts` `refreshCounts` | 82（最大项目） |
| 同一批任务再做 `compute_blocked_by()`，里面**又 `get_task()` 一次** | `api/tasks.rs` `list` → `store/tree.rs` `compute_blocked_by` | 82 |
| 其余 3 个项目 | | 1 |

**≈ 170 次文件打开 + 503 KB 传输，只为算出每个项目的 `total` 和 `submitted`。** 冷缓存下每次打开还叠 Windows Defender 首触扫描，1 秒完全对得上；热缓存约 155 ms。

更讽刺的一处：`compute_blocked_by` 已经在用索引查依赖状态，却先花一次读盘把整个 task 读出来只为拿 `depends_on` —— 而这份前向边早已作为 `TaskEntry::blocks` 存在索引里（派发扫描器 `tree.rs` due-set 的依赖闸门就是这么用的）。

### 2.3 一个需要拆穿的误判

「换成 SQLite 就能杜绝这类损坏」——**不成立，但也不是完全反向**，需要拆成两件事：

| 命题 | 判定 |
|---|---|
| SQLite 能阻止外部进程把数据写坏 | **否**。`echo 'x' >> pm.sqlite` 会让**整库** `database disk image is malformed`，而不是单条任务失效。爆炸半径从「坏 1/83」变成「坏 83/83」 |
| SQLite 让外部写入**更不容易发生** | **是**。误操作的形态由介质决定：JSON 是文本，Agent 顺手 `echo >>`；SQLite 是二进制，Agent 要么去用 `sqlite3`（有语法、schema、类型三层拦截），要么不动手。触发概率低一个量级 |
| SQLite 的损坏可检测 | **是**。WAL 帧校验 + `PRAGMA integrity_check` 让它显式失败，不像文本那样静默返回垃圾。但 PM 现在也已经「显式失败 + 降级」，差距不大 |

**结论**：换存储格式不是防损坏的正确工具。真正的止损点是**访问控制**——`shell` 工具不在 `PathGuardedTool::is_filesystem_tool()` 的名单里（只有 `file_read` / `file_write` / `file_edit` / `glob_search` / `content_search` / `doc_reader`），因此它没有路径守卫，一条 `echo ... >> ~/.acowork/acowork-pm/.../task.json` 就能写坏；反过来 `file_write` 反而挡得住。**堵这里，成本约 60 行。**

### 2.4 为什么不是现在就迁

- **规模不支撑**。4 项目 / 83 任务 / 1.6 MB，`rebuild_index` 毫秒级。ADR-082 迁 memory 是被**实测 7-9 秒启动**逼出来的，有数据；这里没有。
- **原子性反而更差**。`reparent_task` 现在是**一次 `rename`、0 文件写**（ADR-061 零冗余的直接兑现），不输 SQLite 的 `UPDATE`。
- **代价重**。`PmStore` trait 31 个方法、`store/tree.rs` 4143 行、PM 侧 164 个测试。
- **运维退化**。用户修数据从「打开文本编辑器」变成「装 sqlite3 CLI」。数据放在 `~/.acowork/` 的桌面应用，这是实打实的损失。
- **会掩盖而不是修掉热点**。SQLite 不修 N+1，也不修「为两个整数传 503 KB」。先迁再修，等于拿 4000 行迁移掩盖一个 60 行就能修的问题，而且迁移期性能数据会被这个热点污染，无法判断迁移本身有没有用。

---

## 3. 决策

### D1：树存储保留为权威

ADR-061 不变。`task.json` / `project.json` 与物理嵌套仍是唯一真相，索引仍是派生数据（可重建、幂等）。

### D2：读路径必须走内存索引

判据：**如果索引已持有该字段，读盘即缺陷。**

已修两处：

- `compute_blocked_by` 改为纯索引实现：用 `TaskEntry::blocks`（= `depends_on` 过滤 `kind == Blocks`）替代 `get_task()` 后再取 `depends_on`，依赖状态也从索引查。阻塞判定与派发扫描器的闸门保持**逐字一致**：`Done` / `Cancelled` 满足前置，其余（含悬挂引用）阻塞。
- 项目徽章计数改为单次索引聚合，见 D3。

### D3：新增 `GET /projects/counts`

- 响应：`[{ project_id, total, submitted }]`，从 `TaskIndex::by_id` 一次遍历得出，**零读盘、不物化任何 task**。
- 只返回**至少有一个任务**的项目；调用方对缺失项目按 `{0, 0}` 处理（前端已按此 seed）。
- 路由是静态段，与 `/projects/{pid}` 共存（`matchit` 静态优先），已加路由级测试锁死。
- 与既有端点边界：`enable/disable` 那套属于 App 准入，跟这个无关；这里只是列表投影。

### D4：防损坏按「介质无关」三层做（**已实施**）

1. **坏文件取证**：`walk_tasks` 遇到不可解析的 `task.json` 时**复制**一份到 `{data_dir}/.corrupt/{project_id}/{task_id}/task.json`，**原文件留在权威树里不动**——移走会让任务从树上消失，与 ADR-061「树是权威」直接冲突。固定路径（而非 `{ts}/`）保证重启不累积副本；索引继续，记录（project / task / 原路径 / 副本路径 / 错误 / 时间）经 `GET /health` 的 `corrupt_files` / `corrupt_count` 暴露，每次 `rebuild_index` 从零刷新，修好即消失。同时把 `skipped` 补齐为「所有被跳过的条目」并消掉 `rebuild_index` / `walk_tasks` 残留的 `?`，`walk_tasks` 因此不再返回 `Result`——IO 层失败不再能打挂启动或丢掉整个项目。
2. **写前保留上一版**：`atomic_write_json` 覆盖前，**仅当目标当前是合法 JSON** 才把旧内容写成 `*.json.bak`（单文件一份、覆盖式、不轮转，且同样走 `tmp` + `rename` 原子替换——备份自身写一半也不能毁掉唯一好版本）。目标已坏时不动 `.bak`，避免把唯一好备份顶掉；备份失败只告警、绝不阻断写。`.bak` 不会被只找 `task.json` / `project.json` 的扫描读到。**边界**：这里的「合法」只指 JSON 语法可解析，不等于能反序列化成 `Task`；字段类型错误的 JSON 仍会被当成好版本覆盖 `.bak`（见 §6）。
3. **shell 受保护前缀守卫**：`ShellTool::execute` 在任何 spawn 之前扫描 `command`，命中 `~/.acowork/`（含 `$HOME` / `$USERPROFILE` / `%USERPROFILE%` / 由 `acowork_core::workspace::platform_data_dir()` 展开的绝对路径，分隔符与大小写归一化）即拒绝并指向「平台数据目录，请用对应 App 的 API」。三个 shell 工具名（`bash` / `powershell` / `shell`）共用这一处实现，路径常量放在 `acowork-core`，避免把 PM 数据目录硬编码进 Runtime。

**边界要说清**：字符串扫描挡不住变量拼接、`cd` 后的相对路径与解释器一行流，它是**防误伤**而不是安全边界。威胁模型是「Agent 误写」，不是「恶意攻击者」；真正的最终形态是 OS 级沙箱（Windows Job Object / AppContainer、Linux namespace），本轮不做。另：`shell` 虽被 `PathGuardedTool` 包装，但不在 `is_filesystem_tool()` 名单里，所以守卫落在工具自身而不是路径校验层。

> 这三层与存储格式无关。即使将来迁了 SQLite，第 1、2 层语义照搬（坏行隔离、写前快照），第 3 层一字不改。

### D5：SQLite 迁移的触发条件

**边界（先说清）**：这些是**发布前由人判定的立项门槛**，不是运行时自动检测。PM 进程不监控任务数、不评估 `elapsed_ms`、不会自行导入数据或切换存储后端；日志里的 `elapsed_ms` 只是让门槛「可评估」。D6 的每一步同样是开发 / 运维动作，需单独排期与人工确认。**本 ADR 不包含任何运行时自动迁移或自动切换机制**——自动迁移等于把一次不可逆的数据搬迁绑到运行时瞬时指标上，故障域过大（一次 Defender 冷启动扫描就够让「> 1 秒」误触发）。

T1–T4 任一成立才启动（T1 内部两个条件须同时满足）；全部不成立则不启动：

| # | 触发条件 | 理由 |
|---|---|---|
| T1 | 单库任务数 **≥ 10 000**，且 `PM index rebuilt count=N` 的启动耗时**超过 1 秒** | 树结构冷启动是 O(N) 读盘；SQLite open 是 O(1)，与库大小无关（ADR-082 的核心理由） |
| T2 | 需要**跨项目全文搜索 / 聚合统计**（FTS5 成为刚需） | 现在跨项目搜索靠内存索引 + 标题子串，能力有限 |
| T3 | 出现**第二个进程**要同时读写 PM 数据 | 文件树没有写锁，多进程必然打架 |
| T4 | 需要**字段级历史审计**（谁在何时改了哪个字段） | 现在只有 `activity.json` 的活动流，不是字段 diff |

现在请先量一次 T1 的基线并记档（`rebuild_index` 耗时随 `count` 的变化），作为将来判断的依据。

### D6：迁移方式（若触发）

沿用 ADR-082 的成功路径：

1. `PmStore` 31 个方法的 trait 边界不动，新增 `SqlitePmStore`。
2. **双后端测试双跑**——现有 164 个测试对两个实现都要绿。
3. 一次性导入：`walkdir` 扫树 → 建库，导入器幂等。
4. 灰度开关 → 观察 → 删旧实现。
5. 附件仍留在文件系统，DB 只存 `storage_path`。

**禁止**一次性重写后再切。

---

## 4. 被否决的方案

| 方案 | 否决理由 |
|---|---|
| 现在全量迁 SQLite | 规模不支撑（1.6 MB / 83 任务）；原子性不更优（reparent 已是单次 rename）；代价重（31 方法 / 4143 行 / 164 测试）；运维退化；且会掩盖 §2.2 的热点，使迁移效果无法归因 |
| 只把索引持久化到 SQLite，树仍是权威 | 收益仅是省掉冷启动 walk。当前 143 个文件毫秒级，收益为零；却引入「索引与文件可能不一致」的新失败模式，违背 ADR-061「索引是缓存、随时可重建」的定位 |
| 保留 N+1 但给 `/projects/{pid}/tasks` 加 HTTP 缓存 | 缓存会把「索引不是真相」的前提引入；且首页要的本来就不是任务实体。修调用点 60 行，加缓存要处理失效，得不偿失 |
| 把 `total` / `submitted` 直接塞进 `project.json` | 违反 ADR-061 零冗余字段（会引入双写与不一致）。计数是派生数据，必须留在索引侧 |
| 用文件 ACL 挡住外部写入 | 挡不住同用户的 Agent 进程（同一身份），只挡其他用户。对本威胁模型无效 |
| 始终双写（写 JSON + SQLite、读只读 DB、JSON 作备份） | 写确为单任务级轻量操作（单进程单写者，多一次事务约 1–3 ms），**性能不是问题**；但两个介质各有一个提交点且组合不原子——文件 `rename` 原子、DB 事务原子，二者一起并不原子，崩溃窗口数 = 写次数，写越频繁窗口越多。要可靠就得加 `write_seq` + 启动对账，JSON 从「备份」变成带版本的第二主，违反 ADR-061 零冗余（§1.3 已否决双写）。退一步改成「DB 单写 + 单向导出 JSON」虽只有一个提交点，却等于**无条件触发 D5**：读路径已修（155 ms → 8 ms）、`rebuild_index` 55 ms 非瓶颈，当前规模零收益，还要放弃「文本编辑器修数据」与目录树即权威。另外 PM 的写并非全是单任务：`reparent_task` 改整棵子树 `depth`/`dir_path`、`delete_task` 删子树、`delete_project` 整体归档、派发 tick 批量写 `dispatched_at`。结论：性价比不足，维持树权威 |

---

## 5. 实施记录（本轮已完成）

| 文件 | 改动 |
|---|---|
| `core/acowork-pm/src/store/tree.rs` | `compute_blocked_by` 改纯索引；新增 `project_counts`（index-only）；`tree.rs` 加 `HashMap` 导入 |
| `core/acowork-pm/src/types.rs` | 新增 `ProjectCounts`（`project_id` / `total` / `submitted`） |
| `core/acowork-pm/src/api/projects.rs` | 新增 `counts` handler |
| `core/acowork-pm/src/api/routes.rs` | 注册 `GET /projects/counts` |
| `apps/acowork-desktop/src/lib/pm-types.ts` | 新增 `PmProjectCounts` |
| `apps/acowork-desktop/src/lib/pm-api.ts` | 新增 `listProjectCounts()` |
| `apps/acowork-desktop/src/stores/pm/projectStore.ts` | `refreshCounts` 改单次调用，不再 N+1 |
| `apps/acowork-desktop/src/stores/pm/projectStore.counts.test.ts` | 新增：单次调用 / 缺失即零 / 失败不白屏 |
| `core/acowork-pm/src/store/tree.rs` `rebuild_index` | 日志新增 `elapsed_ms`——§7 开放问题 1 落地，D5-T1 的门槛从此可评估 |
| `apps/acowork-desktop/src/stores/pm/projectStore.ts` | 新增 `errorStatus`（保留 HTTP 状态），用于区分「服务未就绪 503」与真错误 |
| `apps/acowork-desktop/src/views/ProjectsView.tsx` | 首屏四态：加载中 / 服务启动中 / 真错误 / 真空库；骨架改用 `surface-strong` 并加 spinner + 文案；503 与网络错误按 1→2→4→8s 退避自动重试（上限 15 次） |
| `apps/acowork-desktop/src/views/pm/ProjectBoard.tsx` | 骨架同样改 `surface-strong`；外层按「该项目尚未成功加载」判定 loading |
| `apps/acowork-desktop/src/views/ProjectsView.boot.test.tsx` | 新增 5 个用例：加载中不可达空态 / 503 显示启动中 / 4xx 也不落空态 / 成功且为空才显示空态 / 切项目时看板为 loading |
| `src/i18n/locales/{zh-CN,en}.json` | 新增 `pm.loadingProjects` / `pm.bootingTitle` / `pm.bootingDesc` / `pm.bootingRetry`；顺手把 zh-CN 里 `pm.offlineHint` 的英文原样修正为中文 |
| `core/acowork-pm/src/store/corrupt.rs` | D4-1 新增：`CorruptRecord` / `CorruptRegistry` / `quarantine_copy`（固定路径、幂等、原文件不动）+ 2 个单测 |
| `core/acowork-pm/src/store/tree.rs` | D4-1：`TreePmStore` 持有坏文件 registry；`rebuild_index` / `walk_tasks` 消掉残留 `?`（`walk_tasks` 不再返回 `Result`）、补齐 `skipped`、坏文件复制取证并记录；新增 `corrupt_files()` / `corrupt_registry()` + 1 个端到端单测（损坏 → 取证 → 修复 → 清单清空） |
| `core/acowork-pm/src/health.rs`、`server.rs` | D4-1：`/health` 新增 `corrupt_files` / `corrupt_count`，与 store 共享同一 registry |
| `core/acowork-pm/src/store/atomic.rs` | D4-2：覆盖前仅当目标可解析时保留 `*.json.bak`（`tmp` + `rename` 原子替换、单份、覆盖式、失败只告警）+ 2 个单测 |
| `core/acowork-core/src/workspace.rs` | D4-3：新增 `PLATFORM_DATA_DIR` / `platform_data_dir()`，受保护前缀的单一来源 |
| `core/acowork-runtime/src/tools/builtin/shell.rs` | D4-3：`protected_prefix_hit()` + spawn 前拒绝（bash / powershell / shell 三工具共用）+ 4 个单测 |

**D4 本轮验证**：`cargo test -p acowork-pm --lib` → **169 passed**（+5）；`cargo test -p acowork-runtime --lib shell` → **77 passed**（+4）；`cargo clippy -p acowork-core|acowork-pm|acowork-runtime --lib -- -D warnings` 三个 crate 全通过；`./dev/ci.sh redlines` 14 条全绿。全 workspace `--all-targets -D warnings` 仍被 `acowork-gateway` 的 8 个**既有** clippy 错误（`supervisor.rs` / `ticket.rs` / `auth_middleware.rs`）拦下，与本轮改动无关。

**首屏状态机（为什么这是 bug 而不只是动画）**

原来的首个分支是 `projects.length === 0 && !loadingProjects`，把两件完全不同的事合在一起：

- **冷启动**：Gateway 先接受 `/api/pm/*`、PM 还在重建索引 → 首次请求合法返回 `503` → `loading` 归 `false`、`projects` 为空；
- **真空库**。

两者都渲染「还没有项目」。冷启动时这既像卡死，又像**数据丢失**——而这个应用已经因为一次单文件损坏被误判为「项目全没了」（§2.1）。所以现在增加 `projectsUpdatedAt === null` 作为「从未成功加载」的判据，先给可见的启动反馈并自动重试，**只有成功返回且确实为空才显示空态**。

**骨架不可见是另一半原因**

原骨架块是 `bg-surface`(#27272a) 压在 `bg-page-bg`(#131316) 上，对比度约 **1.25:1**，再叠 `animate-pulse` 的 50% 透明度——深色主题下基本等于没有。改成 `surface-strong`(#52525b) 并加 spinner 与状态文案后，即使骨架仍偏暗也有明确的可感知反馈。

**测试**：`cargo test -p acowork-pm --lib` → **164 passed**（含新增 2 个：两者都用「删掉磁盘文件后结果不变」来证明不再读盘，而不是靠 I/O 探针）；`vitest src/stores/pm src/views/ProjectsView.boot.test.tsx` → **24 passed**（含新增 2 + 5 个）。

**实测（同机、同库、热缓存、82 任务项目）**：

| 指标 | 改前 | 改后 |
|---|---|---|
| 首页徽章数据来源 | 4 × `listProjectTasks` | 1 × `GET /projects/counts` |
| 首页传输量 | 503 KB（最大项目）+ 小项目 | **106 B** |
| 首页磁盘打开 | ≈ 170 | **4**（`list_projects` 的 4 个 `project.json`） |
| 首页耗时（热缓存） | ≈ 155 ms | **≈ 8 ms** |
| `GET /projects/{pid}/tasks`（82 任务） | 145–148 ms | **75–92 ms**（−48%，payload 不变） |

隔离实例 A/B：新旧二进制各跑一份数据副本，端口 18082 / 18183，未触碰用户正在使用的进程。

**冷启动基线（新增 `elapsed_ms` 后首次测得，85 任务 / 热缓存）**：

| 指标 | 实测 |
|---|---|
| `rebuild_index` 耗时 | **55 ms** |
| 进程启动 → `/health` 200（端到端） | **415 ms** |

结论：**当前规模下索引重建（55 ms）不是瓶颈**，「启动慢」的体感主要来自读路径（已修）与冷 I/O，以及把「服务未就绪」误渲染成空态。但代价随任务数线性：按 0.65 ms/任务外推，10 000 任务约 **6.5 s**（冷缓存更差），这正是 D5-T1 的由来，也是首屏启动态必须存在的原因。

---

## 6. 后果

**正面**

- 首页耗时与任务总量解耦：徽章是索引聚合，任务从 83 长到 10 000 也只是多扫一遍内存。
- `compute_blocked_by` 从 O(1) 读盘降到 O(依赖数) 内存，所有调用方（列表、`only_blocked` 过滤）同步受益。
- 阻塞判定与派发闸门共用同一份索引数据源，减少「两处逻辑漂移」的可能。
- 树的权威性没有被削弱：索引仍然只是缓存，删掉照样能 `walkdir` 重建（新增测试正是这么验的）。

**负面 / 代价**

- `/projects/{pid}/tasks` 仍然返回全量实体（503 KB）。这是看板需要的，不是缺陷；但**不能**再被首页徽章复用。
- `project_counts` 每次都是 O(N) 内存遍历。10 000 任务时约数毫秒，可接受；再往上就该走 D5 的 T1 了。
- D4 三层已实施，但第 3 层只是字符串级防误伤：变量拼接、`cd` 后的相对路径、解释器一行流仍可绕过；将来若迁库，shell 也同样能写 `pm.sqlite`。OS 级沙箱仍是最终形态。
- D4 第 2 层的「合法」只到 JSON 语法层：`{"id": 123}` 这类字段类型错误仍会被当作好版本写进 `.bak`，顶掉真正的上一版。要做到 schema 级校验，需给 `atomic_write_json` 加 `DeserializeOwned` 约束，而 `activity.json` 目前以 `&[TaskActivity]` 切片写入、不满足该约束，本轮未改。`.bak` 本身已改成原子替换，所以备份不会再被自身写一半毁掉，但「语义损坏被当好版本」这个口子还在。

**中性**

- 新端点让前端多一个契约面；它足够窄（三个字段），不会变成第二个任务列表。

---

## 7. 开放问题

1. ~~**T1 基线怎么量**：`rebuild_index` 只打印 `count` / `skipped`，没有耗时。~~ **已解决**：日志新增 `elapsed_ms`，首次测得 85 任务 55 ms（见 §5）。后续应在每次发布前记一次该值，形成曲线。
2. ~~**`.corrupt/` 要不要在 UI 暴露**~~ **部分解决**：坏文件清单（含原路径与副本路径）已通过 `/health` 的 `corrupt_files` 暴露；Desktop UI 尚未接，倾向在项目列表给一条非阻断提示。
3. **shell 守卫的兜底**：字符串扫描之外，是否给 PM 数据目录加一个「进程自己持有的 `.lock`」用于事后归因（记录最后写入者）？只解决可观测，不解决阻断。
4. **`/projects` 本身仍是 4 次读盘**：项目数少时不值得改；若项目数上百，`list_projects` 也该走索引（索引里目前没有 project 条目，需要新增 `by_project_meta`）。
5. **attachment 元数据在 `task.json` 里**：单文件损坏会连带丢附件索引。`attachments/` 目录本身还在，可重建，但重建规则尚未定义。

---

## 8. 关联文档

- [ADR-061：PM 存储选型 — 目录树 + 物理嵌套即权威 + 零冗余字段](./ADR-061-pm-storage-tree.md)
- [ADR-082：记忆存储后端迁移至 SQLite](./ADR-082-memory-storage-sqlite-vector-fts.md)
- [ADR-064：PM 独立进程](./ADR-064-pm-standalone-process.md)
- [设计文档 21：PM 项目管理](../design/zh/21-pm-project-management.md)
