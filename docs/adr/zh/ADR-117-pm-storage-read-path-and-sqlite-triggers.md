# ADR-117：PM 存储演进 — 先修读路径，最终收敛到 SQLite 单后端

**状态**：已定案并终态化（2026-11-16 启动 SQLite 迁移并落地三态灰度 ①–⑥；2026-11-17 收敛为 SQLite 单后端并完成 ⑦⑧；**同日深夜按用户「用完即弃」指示完成 ⑨ 最终收口**：删除启动期 JSON 导入、归档收口、整套树实现（`tree.rs` / `index.rs` / `atomic.rs` / `corrupt.rs`）、恒空坏文件 registry、遗留配置守卫与全部孤儿依赖；`PmStore` / `PmMaintenance` 迁入 `store/contract.rs`、规则 helpers 迁入 `store/rules.rs`；测试夹具全迁 `SqlitePmStore`；`cascade` 契约补齐 409、删除从未生效的 `promote_children`；`/health` 改报实时 SQLite 诊断。当前 `pm.sqlite` 是唯一数据源，空目录首次启动直接建空 v1 库，**不存在任何迁移 / 兼容 / 灰度代码**。读路径修复与 D4 三层是树时代历史记录，见 §3 / §5）
**日期**：2026-11-16
**决策者**：大鱼
**前置**：
- [ADR-061：acowork-pm 存储选型 — 目录树 + 物理嵌套即权威 + 零冗余字段](./ADR-061-pm-storage-tree.md)
- [ADR-082：记忆存储后端迁移至 SQLite](./ADR-082-memory-storage-sqlite-vector-fts.md)（迁移方法论与教训来源）
- [ADR-064：PM 独立进程](./ADR-064-pm-standalone-process.md)

---

## 1. 决策摘要

### 1.1 一句话

**先把读路径修对，再把 SQLite 做成可配置的第二实现，最终收敛到 SQLite 单后端。** 读路径与 D4 防损坏是树时代的历史修复；迁移原是带量化触发条件的预案，2026-11-16 由用户显式无条件启动，先落地 `storage_backend = filetree | db | dual` 三态灰度，全程硬约束：**任一时刻只有一个提交点**、不搞等权双写。**2026-11-17 用户明确不再保留 `filetree`**；因项目尚未正式发布、除工作区外几乎没有用户，随后按「用完即弃」把三态配置、`dual` 投影、`migrate` / `reconcile` 子命令、一次性布局迁移、启动期 JSON 导入、归档收口与整套树实现全部删除。当前运行期只装配 `SqlitePmStore`：`pm.sqlite` 存实体，`assets/projects/{pid}/` 存二进制，空目录首次启动直接建空 v1 库。

### 1.2 关键决策表

| # | 决策 | 结论 |
|---|---|---|
| D1 | 存储权威 | **SQLite 单后端（2026-11-17 终态）**。`pm.sqlite` 是唯一提交点，`assets/projects/{pid}/` 存二进制资产；⑨ 后连 `projects.retired` 也不再被任何运行代码读取。ADR-061 的树模型、三态配置、`dual` 投影与全部迁移代码均已删除，硬约束始终是**同一时刻只有一个真相** |
| D2 | 读路径 | **树时代结论：读必须走内存索引**（历史）。已修 `compute_blocked_by` 与项目徽章计数；现无内存索引，`/projects/counts` 由单条分组 SQL 聚合 |
| D3 | 新契约 | 新增 `GET /projects/counts`（返回每项目 `total` / `submitted`）。树时代由索引遍历得出，现由单条分组 SQL 得出；首页徽章不再 N+1 拉全量任务实体 |
| D4 | 防损坏 | **树时代三层（历史，见 §5）**：坏文件留原地 + `.corrupt/` 取证副本、写前 `.bak`、**shell 受保护前缀守卫**。⑨ 随树实现删除前两层（SQLite 事务 + `integrity_check` 取代）；第三层仍在 `acowork-runtime` 生效，`GET /health` 现报实时 SQLite 诊断而非坏文件清单 |
| D5 | 迁移触发 | 原定 T1–T4 量化门槛**已被 2026-11-16 用户的显式指令越过**：迁移无条件启动，T1–T4 降级为规模基线。⑨ 后已无任何迁移触发机制，见 §3 D5（历史） |
| D6 | 迁移方式 | `PmStore` / `PmMaintenance` trait 迁入 `store/contract.rs` 保持对象安全，规则 helpers 迁入 `store/rules.rs`。三态灰度框架、导入器、归档收口与整套树实现已在 ⑧⑨ 全部删除；运行期只装配 `SqlitePmStore`，**空目录首次启动直接建空 v1 库，无导入步骤**。导入契约与历史灰度设计见 §3 D6（历史） |

### 1.3 明确不做

- 不把附件二进制迁进 DB。二进制留在 `assets/projects/{pid}/tasks/{tid}/attachments/`，元数据在 SQLite `attachments` 表（ADR-061 决策 5 不变）。
- 不引入「树 + SQLite **等权双写**」。双写是双份真相，违反 ADR-061 零冗余的初衷。历史上 D6 的 `dual` 不是双写而是**单向投影**（树提交后尽力同步进 DB，投影失败只告警）；该实现已随树后端在 ⑧⑨ 删除（见 §3 D6）。
- 不为防损坏而换存储格式（理由见 §2.3）。
- 不给读路径加 TTL 缓存（树时代结论；现由 SQLite 查询直接回答，同样无需缓存）。

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

### D1：树存储保留为权威（历史，⑨ 已废弃）

树时代 ADR-061 不变：`task.json` / `project.json` 与物理嵌套是唯一真相，索引是派生数据。⑨ 后两者与内存索引全部删除，`pm.sqlite` 成为唯一真相；本条仅作为演进起点保留。

### D2：读路径必须走内存索引（历史，树时代）

判据（树时代）：**如果索引已持有该字段，读盘即缺陷。** 现无索引层，等价形态是「不得为聚合重复拉全量任务实体」。

已修两处：

- `compute_blocked_by` 改为纯索引实现：用 `TaskEntry::blocks`（= `depends_on` 过滤 `kind == Blocks`）替代 `get_task()` 后再取 `depends_on`，依赖状态也从索引查。阻塞判定与派发扫描器的闸门保持**逐字一致**：`Done` / `Cancelled` 满足前置，其余（含悬挂引用）阻塞。
- 项目徽章计数改为单次索引聚合，见 D3。

### D3：新增 `GET /projects/counts`（端点仍在）

- 响应：`[{ project_id, total, submitted }]`。树时代从 `TaskIndex::by_id` 一次遍历得出，**零读盘、不物化任何 task**；现由单条分组 SQL 得出，同样不物化实体。
- 只返回**至少有一个任务**的项目；调用方对缺失项目按 `{0, 0}` 处理（前端已按此 seed）。
- 路由是静态段，与 `/projects/{pid}` 共存（`matchit` 静态优先），已加路由级测试锁死。
- 与既有端点边界：`enable/disable` 那套属于 App 准入，跟这个无关；这里只是列表投影。

### D4：防损坏按「介质无关」三层做（树时代实施；⑨ 删除前两层）

1. **坏文件取证**：`walk_tasks` 遇到不可解析的 `task.json` 时**复制**一份到 `{data_dir}/.corrupt/{project_id}/{task_id}/task.json`，**原文件留在权威树里不动**——移走会让任务从树上消失，与 ADR-061「树是权威」直接冲突。固定路径（而非 `{ts}/`）保证重启不累积副本；索引继续，记录（project / task / 原路径 / 副本路径 / 错误 / 时间）经 `GET /health` 的 `corrupt_files` / `corrupt_count` 暴露，每次 `rebuild_index` 从零刷新，修好即消失。同时把 `skipped` 补齐为「所有被跳过的条目」并消掉 `rebuild_index` / `walk_tasks` 残留的 `?`，`walk_tasks` 因此不再返回 `Result`——IO 层失败不再能打挂启动或丢掉整个项目。**本轮 review 补修**：`children/` 下钻移到读 `task.json` 之前（坏父记录曾让 `continue` 吞掉整棵健康子树），深度用 `saturating_add`；取证副本改 `tmp` + `rename` 原子落盘；`/health` 只返回脱敏投影（项目 / 任务 ID、相对路径、是否隔离、时间、粗分类原因），不再回显绝对路径与原始 IO 错误。
2. **写前保留上一版**：`atomic_write_json` 覆盖前，**仅当目标当前是合法 JSON** 才把旧内容写成 `*.json.bak`（单文件一份、覆盖式、不轮转，且同样走 `tmp` + `rename` 原子替换——备份自身写一半也不能毁掉唯一好版本）。目标已坏时不动 `.bak`，避免把唯一好备份顶掉；备份失败只告警、绝不阻断写。`.bak` 不会被只找 `task.json` / `project.json` 的扫描读到。**边界**：这里的「合法」只指 JSON 语法可解析，不等于能反序列化成 `Task`；字段类型错误的 JSON 仍会被当成好版本覆盖 `.bak`（见 §6）。**本轮补修**：`rename` 失败时清理暂存的 `*.json.bak.tmp` 与主写路径的 `*.json.tmp`，失败不再留孤儿文件。
3. **shell 受保护前缀守卫**：`ShellTool::execute` 在任何 spawn 之前扫描 `command`，命中 `~/.acowork/`（含 `$HOME` / `$USERPROFILE` / `%USERPROFILE%` / 由 `acowork_core::workspace::platform_data_dir()` 展开的绝对路径，分隔符与大小写归一化）即拒绝并指向「平台数据目录，请用对应 App 的 API」。三个 shell 工具名（`bash` / `powershell` / `shell`）共用这一处实现，路径常量放在 `acowork-core`，避免把 PM 数据目录硬编码进 Runtime。**本轮补修**：改为「写意图 + 保护前缀」双条件，`ls` / `cat` / `grep` / `Get-Content` 等只读检查不再被误杀；保护范围补上 `ACOWORK_PM_HOME` / `ACOWORK_PM_DATA_DIR` 自定义数据目录及其展开后的绝对路径。

**边界要说清**：字符串扫描挡不住变量拼接、`cd` 后的相对路径与解释器一行流，它是**防误伤**而不是安全边界。威胁模型是「Agent 误写」，不是「恶意攻击者」；真正的最终形态是 OS 级沙箱（Windows Job Object / AppContainer、Linux namespace），本轮不做。另：`shell` 虽被 `PathGuardedTool` 包装，但不在 `is_filesystem_tool()` 名单里，所以守卫落在工具自身而不是路径校验层。写意图是粗粒度关键字判据（重定向、`rm`/`cp`/`mv`、`*-Item` / `Set-Content`、`sqlite3` 等），可能误拒也可能漏判；它只服务「降低误写概率」，不承担安全边界职责。

> 树时代判断：三层与存储格式无关。⑨ 落地后第 1、2 层由 SQLite 事务与 `PRAGMA integrity_check` 取代，第 3 层（shell 守卫）一字未改、仍在 `acowork-runtime` 生效。

### D5：SQLite 迁移的触发条件

**状态变更（2026-11-16）**：用户已显式无条件启动迁移（「立即启动，双后端 + 灰度切换」），因此 **T1–T4 不再是立项门槛**，降级为规模基线与回归判据：每次发布前仍记录 `count` / `elapsed_ms` 形成曲线，用于判断 `db` 是否值得切为主，但不阻挡本次实施。原「缺一条就不启动」的措辞随之废止。

**边界（仍然有效）**：这些指标是**发布前由人判定的依据**，不是运行时自动检测。PM 进程不监控任务数、不评估 `elapsed_ms`、不会自行导入数据或切换存储后端；日志里的 `elapsed_ms` 只是让门槛「可评估」。D6 的每一步同样是开发 / 运维动作，需单独排期与人工确认。**本 ADR 不包含任何运行时自动迁移或自动切换机制**——自动迁移等于把一次不可逆的数据搬迁绑到运行时瞬时指标上，故障域过大（一次 Defender 冷启动扫描就够让「> 1 秒」误触发）。⑨ 后连「启动时选择后端」都不复存在：只有一个后端，没有任何选择或切换机制。

T1–T4（现为规模基线 / 回归判据；T1 内部两个条件须同时满足）：

| # | 触发条件 | 理由 |
|---|---|---|
| T1 | 单库任务数 **≥ 10 000**，且 `PM index rebuilt count=N` 的启动耗时**超过 1 秒** | 树结构冷启动是 O(N) 读盘；SQLite open 是 O(1)，与库大小无关（ADR-082 的核心理由） |
| T2 | 需要**跨项目全文搜索 / 聚合统计**（FTS5 成为刚需） | 现在跨项目搜索靠内存索引 + 标题子串，能力有限 |
| T3 | 出现**第二个进程**要同时读写 PM 数据 | 文件树没有写锁，多进程必然打架 |
| T4 | 需要**字段级历史审计**（谁在何时改了哪个字段） | 现在只有 `activity.json` 的活动流，不是字段 diff |

现在请先量一次 T1 的基线并记档（`rebuild_index` 耗时随 `count` 的变化），作为将来判断的依据。

### D6：迁移方式（历史：三态灰度 → SQLite 单后端 → ⑨ 用完即弃）

**配置（历史，⑧ 已删除 `StorageBackend` / `storage_backend` / `--storage-backend`）**：`PmConfig.storage_backend` 三态，来源优先级 `CLI --storage-backend` > 环境变量 `ACOWORK_PM_STORAGE_BACKEND` > TOML `storage_backend` > 默认 `filetree`。当时由工厂一次性装配，**运行中不可切换**。**本轮补上真实加载层**：`main.rs` 新增 `--config`（未指定时若 `./acowork-pm.toml` 存在则加载），`ACOWORK_PM_DATA_DIR` 接入；`StorageBackend` 的 serde alias 与 `FromStr` 统一，CLI 与 TOML 接受同一套拼写；无效值、非法 TOML、显式 `--config` 指向的缺失文件一律 `exit(1)`。⑨ 后 `config.rs` 只留 `STORAGE_BACKEND = "db"` 这个 `/health` 报告常量：`deny_unknown_fields` 对任何未知键（含遗留 `storage_backend`）直接报错，遗留 env 守卫与兼容类型一并删除——按「用完即弃」不再为旧部署残留代码。

| 值 | 语义 | 提交点 | 业务读 |
|---|---|---|---|
| `filetree` | 现状，`TreePmStore` | 树 | 树 + 内存索引 |
| `dual` | 灰度观察态：树主写，SQLite 同步投影 | **树（唯一）** | 树 + 内存索引，与 `filetree` 完全一致；DB 仅供离线对账 |
| `db` | 目标态：`SqlitePmStore` | SQLite | SQLite |

**`dual` 的四条铁律**（历史设计，实现已随 ⑧ 删除；与 §4 被否决的「始终双写」划清界限）：

1. 树提交成功后才投影；写返回值**只由树决定**，投影失败仅 `warn` + 计数，绝不返回错误、绝不回滚树。
2. DB 是**可丢弃的派生数据**：启动时若 DB 缺失 / 版本落后 / 校验不通过，从树全量重建（55 ms 量级）。不要求 `write_seq`、不做启动对账、不做冲突解决。
3. DB 永不参与任何业务读；「DB 落后」不影响用户可见状态，只影响离线对账的新鲜度。
4. 切到 `db` 前必须有一次**停机窗口**：停写 → 全量导入 → `MigrationReport` 校验通过 → 改配置重启。禁止在 `dual` 运行期间直接把读切到 DB。

**trait 边界**：`PmStore` 的 31 个业务方法保持对象安全，HTTP / MCP / dispatch 一律依赖 `Arc<dyn PmStore>`。侦察时发现业务层历史上还直接调用了 15 个**只挂在具体 `TreePmStore` 上**的方法（启动重建 / `start_at` 回填 / 派发候选与状态记录 / `depth`、 `parent_id`、项目任务数等派生读 / 项目图标读写 / 坏文件 registry / `shutdown`），这些都不是业务变更，统一收进 `PmMaintenance` 侧接口（`PmStore: PmMaintenance`），各后端用自己真正拥有的东西回答；当时的 `DualPmStore` 把维护钩子委托给树实现（⑧ 已删除）。这样 DB 后端不必实现 `walkdir` 类操作，也不必伪造 `TaskEntry`。图标按 ADR-061 决策 5 始终留在文件系统，不进 DB 投影。

**JSON → SQLite 导入契约**（补齐初版 D6 的空白；**以下 1–13 条全部为历史记录，⑨ 已删除 `import.rs` 与全部实现**）：曾由导入器 / 灰度框架执行，现无任何运行代码引用。

1. **dry-run 先行**：只读扫描全树，产出 `MigrationReport`（项目数 / 任务数 / 附件数 / 跳过项 / 每条不一致的原因），不写任何文件；报告存在阻断级问题即中止。
2. **一致性校验**：目录名 ↔ `task.id` ↔ `project_id` 三者必须一致；`define_id!` 的 `#[serde(transparent)]` Deserialize **不调用 `parse()`**，所以导入器必须显式 `parse::<TaskId>()` / `parse::<ProjectId>()`，不能信任 JSON 内的 ID 字符串；`#[serde(default)]` 字段要区分「合法缺省」与「语义损坏」，缺省值须显式记进报告。
3. **模型反转**：物理 `children/` 嵌套 → `parent_id` / `depth`，校验无环、深度不超过 `max_task_depth`。
4. **sidecar 归属**：`activity.json`、`project.json`、图标、`attachments/`、`children/`、`.trash/`、`.corrupt/`、`.bak` 逐一列明归属；只导入业务实体，取证副本与备份不进 DB。
5. **附件基准路径**：`AttachmentMeta.storage_path` / `thumb_path` 是**相对任务目录**的路径，DB 必须同时存 `task_dir` 基准；二进制仍留文件系统（ADR-061 决策 5）。
6. **写入安全**：参数绑定、单事务 all-or-nothing、目标非空即跳过（幂等门）、`PRAGMA integrity_check` + `foreign_key_check`、`user_version` 记 schema 版本。可复用 `acowork-memory/src/sqlite/legacy_import.rs` 与 `acowork-user/src/chat_sessions.rs` 的 stage → commit → report 范式；**ADR-082 §4 的「用完即删导入代码」不适用于 PM 生产数据**。
7. **回滚窗口**：`dual` 期间随时改回 `filetree`（DB 只是投影，删掉即可）；切到 `db` 后保留树的只读归档至少一个发布周期，回滚 = 停机 → 改回 `filetree` → 从归档恢复。
8. **嵌套 ID 与路径安全**：`depends_on[].task_id`、`result.attachment_ids[]`、`TaskActivity.id` 等嵌套 ID 同样必须显式 `parse()`；`storage_path` / `thumb_path` 逐 `Component` 校验为安全相对路径（`safe_rel`），绝对路径与 `..` 一律阻断，读盘时再做 `canonicalize()` + 根收敛。
9. **legacy 活动流**：缺 `activity.json` 但带 `result` 的任务，树端 `load_activity` 会内存补种一条 `Submitted`；导入器必须调用同一个 `seeded_activity()` 补上，否则切 `db` 后历史活动凭空消失，而 `reconcile` 两侧都 0 行还会给出假绿灯（真实数据直接命中 23 个）。
10. **目录 I/O 不静默**：`read_dir` 错误、`p-` 开头但 ID 解析失败、`project.json` 缺失而 `tasks/` 非空、`strip_prefix(data_dir)` 失败，全部是阻断项而非 sidecar 跳过——静默漏项目比启动失败更危险。
11. **两套损坏策略**：一次性 `import` / `dry_run` / `reconcile` 用 `Blocking`（要创建新权威，坏文件必须拦住）；`dual` 全量重建投影用 `SkipAndRecord`（树已跳过，投影也跳过，一个坏文件不能让灰度永久停摆），但 `reconcile` 仍拒绝给绿灯。
12. **backend 硬闸**：`rebuild_into()` 断言 `storage_backend == dual`——`db` 模式下 DB 是权威，绝不能被树扫描覆盖；打开投影失败时把 `pm.sqlite`（含 `-wal` / `-shm`）rename 到 `.quarantine-<ts>`，绝不删除可能是健康的库。
13. **对账不止比 `props`**：`parent_id` / `depth` / `status` / `task_dir` 等投影列一并比对（DB 后端实际读这些列），否则列漂移也能拿到假绿灯。

**灰度与验收顺序**：① 三态配置 + 工厂 ✅ → ② `PmStore` 对象化 + `PmMaintenance` ✅ → ③ `SqlitePmStore` + 导入器 ✅ → ④ `DualPmStore` ✅ → ⑤ 现有测试对 `filetree` / `db` / `dual` **三跑** ✅ → ⑥ `dual` 灰度观察、离线对账能力 ✅ → ⑦ 切 `db` ✅（2026-11-17 改为启动期自动一次性导入，不再需要停机窗口与手工命令） → ⑧ 删旧实现 ✅（2026-11-17 完成，见下文「⑧ 过渡代码清理」）。推分支前的全量 review 已把核实确凿的问题全部修完（见 §5）；⑦ 的自动化不改「任一时刻只有一个提交点」的硬约束，⑧ 只删退出运行路径的代码；⑨ 进一步把 ⑦ 的启动期导入判为一次性过渡代码并删除。

**单后端落地（2026-11-17，历史形态）**：当时 `PmConfig::default()` 改为 `db`；`build_store()` 只装配 `SqlitePmStore`，首次启动用只读探针判空后自动执行 `dry_run` → 单事务 `import`，有阻断项则拒绝启动。⑨ 已删除 `ensure_sqlite_authority()` 与整套导入 / 归档逻辑，改为直接 `SqlitePmStore::open()`。随后每次启动都幂等执行 `retire_tree_archive()`：把项目图标与任务附件二进制复制到 `assets/projects/{pid}/` 下（`icon/` 与 `tasks/{tid}/` 同属一棵子树）、改写 `tasks` / `attachments` 的 `task_dir`、校验无行再指向 `projects/`，然后把 `projects/` 重命名为 `projects.retired`（只改名不删除，保留可恢复快照）。此后 `pm.sqlite` + `assets/` 是唯一数据源。`FileTree` / `Dual` 降级为配置兼容类型，装配时硬拒绝；`dual` 投影与 `reconcile` 当时退出运行路径，随后在 ⑧ 中连同实现一并删除。

**资产目录约定（2026-11-17 修订）**：`assets/` 按项目分组——`assets/projects/{pid}/icon/` 存项目图标、`assets/projects/{pid}/tasks/{tid}/attachments/` 存任务附件二进制，一个项目的全部二进制落在同一棵 `{pid}` 子树里，`delete_project` 可一次清理、不留孤儿目录。初版用平铺的 `assets/tasks/{tid}/`，与图标分属两个平级目录，删项目时无法按项目归集；存量行由一次性的 `import::normalize_asset_layout()` 启动期迁移完成（真实库 93 行已于 2026-11-17 23:29 归位），该函数随 ⑧ 删除；全新安装只会生成规范布局。该布局成立的前提是 `create_task_as` / `reparent_task` 都拒绝跨项目父子关系，`tasks.project_id` 全生命周期不变，物理目录无需跨项目搬移。

③ 的落地边界（历史，⑦/⑧ 已取代）：`SqlitePmStore` 已实现全部 31 个 `PmStore` 业务方法 + 15 个 `PmMaintenance` 维护钩子，`db` 已可由工厂装配并服务于 REST / MCP / dispatch；导入器是**显式运维命令** `acowork-pm --data-dir <dir> migrate`（缺省只读 dry-run，`--apply` 才写库），不是启动时的运行时副作用。工厂对「空 DB + 非空树」硬失败，强制先走停机窗口；对 `db` 模式下新上传的附件写入 `{data_dir}/assets/` 下（现行按项目分组，见上文），不污染作为回滚源的树归档。**本轮 review 加固**：图标目录从 `projects/{pid}/icon`（回滚源）移到 `assets/projects/{pid}/icon` 并保留归档只读回退；`schema::open()` 把版本闸门前置到 WAL / DDL 之前，拒绝一个看不懂的库时不留任何写入（`user_version=0` 但已有 PM 表同样拒绝），`integrity_check` 改为收集全部结果行；`db` 启动拒绝路径先跑 dry-run 再决定是否建库，绝不凭空造出 `pm.sqlite` 让后续 `--apply` 误判「目标非空」。

④⑤⑥ 的落地边界（历史，实现已随 ⑧ 删除）：`DualPmStore` 把 31 个业务方法与 15 个维护钩子全部委托给树——读用 `dual_read!`、写用 `dual_write!`（树提交成功后才投影，投影结果被丢弃）。投影采用**全量扫描 + 单事务全量替换**，而非 31 个手写增量钩子，代价是每次写 O(tree)、收益是灰度期不可能漏字段 / sidecar / 子树；一把 `projection_lock` 串行化「扫描 → 替换」，防止并发写时较旧快照后提交覆盖较新快照。DB 打不开时降级为纯树服务，绝不阻断业务。`reconcile` 是**只读** gate：只读打开 `pm.sqlite`（不调用会建表 / 设 WAL / 改 `user_version` 的 `schema::open`），缺库、版本倾斜、完整性失败一律不给绿灯，逐字比对实体 key 集合与 `props` blob 及投影列，不一致时 CLI 退出码非零。`/health.details.projection` 由**实例级** provider 提供 `ok` / `failed` / `consecutive_failures` / `last_error` / `last_report`，把「DB 落后」变成对账信号而非服务故障。**本轮 review 加固**：`tree_write_seq` 让「扫描期间发生树写」的投影自动重扫，避免把半移动的子树状态提交进 DB；投影 DB 打不开时 quarantine 而非删除；非 `dual` backend 直接拒绝打开投影；`dispatch_identity` 与树端一致（已有身份直接返回、不写盘、不触发投影），`migrate_null_start_at` / `shutdown` 不再制造无谓投影；两后端的 `reparent_task` 现在都校验整棵子树平移后不超 `max_task_depth`，SQLite 的 `depth` 更新用 `MAX(0, …)` 防止负值回绕成 `u8` 巨值。

**⑧ 过渡代码清理（2026-11-17）**：真实环境完成切库、归档作废与资产布局归位后，把只在迁移期有用的代码全部删除——`StorageBackend` 三态枚举与 `PmConfig.storage_backend`、`--storage-backend` CLI 参数、`store/dual.rs`（`DualPmStore` / 投影指标 / `dual_read!` / `dual_write!`）、`PmMaintenance::projection_health`、`rebuild_into()` / `rebuild()` / `reconcile()` / `ReconcileReport`、`CorruptPolicy` 双策略、`migrate` / `reconcile` 子命令及相关 re-export、`/health.details.projection`、图标对 `projects/` 归档的只读回退、一次性 `normalize_asset_layout()`。`config.rs` 仅保留 `STORAGE_BACKEND = "db"` 常量与「遗留配置必须响亮失败」的守卫；`ensure_sqlite_authority()` 只保留「只读判空 → dry-run → 单事务 import → `retire_tree_archive()`」这条唯一活路径。⑧ 当时保留 `store/tree.rs` / `index.rs` 承载 trait 与规则 helpers；⑨ 已把它们全部删除——trait 迁入 `store/contract.rs`、规则迁入 `store/rules.rs`、测试夹具全迁 `SqlitePmStore`。

**⑨ 用完即弃终态收口（2026-11-17 深夜）**：用户明确「项目尚未正式发布、除工作区外几乎没有用户，留太多兼容性代码最后多是死代码」，据此把 ⑧ 之后仍残留的过渡层一次删净——`ensure_sqlite_authority()` / `store/sqlite/import.rs`（启动期 JSON 导入、`retire_tree_archive()` 归档收口）、`store/tree.rs` / `index.rs` / `atomic.rs` / `corrupt.rs`（`TreePmStore`、`TaskIndex`、原子写、坏文件 registry 与恒空的 `/health.corrupt_files`）、`FileTree` / `Dual` 兼容类型、遗留 env 与 TOML 守卫，以及删代码后遗留的 6 个孤儿依赖（`notify` / `ignore` / `regex` / `parking_lot` / `tokio-stream` / `anyhow`）。`PmStore` / `PmMaintenance` 抽到 `store/contract.rs`，`safe_rel` / `parse_task_id` / `validate_transition` 等规则抽到 `store/rules.rs`，`rebuild_index` / `corrupt_registry` 两个恒空钩子删除、换成 `storage_diagnostics()` 实时诊断；`lib.rs` 导出面收敛到 `SqlitePmStore` + 两个 trait。5 个测试套件与 src 内夹具全部改用 `SqlitePmStore`；`cascade=false` 且有子任务 / 项目任务时返回新 `PmError::Conflict`（409 `conflict`），删除从未生效的 `promote_children` query 参数并同步 OpenAPI / README。当前 `build_store()` 只有一行 `SqlitePmStore::open(config)`：空目录建空 v1 库，版本倾斜由 `schema::open()` 硬拒绝，**没有任何迁移代码可回头加**。

---

## 4. 被否决的方案

| 方案 | 否决理由 |
|---|---|
| 现在全量迁 SQLite | 规模不支撑（1.6 MB / 83 任务）；原子性不更优（reparent 已是单次 rename）；代价重（31 方法 / 4143 行 / 164 测试）；运维退化；且会掩盖 §2.2 的热点，使迁移效果无法归因 |
| 只把索引持久化到 SQLite，树仍是权威 | 收益仅是省掉冷启动 walk。当前 143 个文件毫秒级，收益为零；却引入「索引与文件可能不一致」的新失败模式，违背 ADR-061「索引是缓存、随时可重建」的定位 |
| 保留 N+1 但给 `/projects/{pid}/tasks` 加 HTTP 缓存 | 缓存会把「索引不是真相」的前提引入；且首页要的本来就不是任务实体。修调用点 60 行，加缓存要处理失效，得不偿失 |
| 把 `total` / `submitted` 直接塞进 `project.json` | 违反 ADR-061 零冗余字段（会引入双写与不一致）。计数是派生数据，必须留在索引侧 |
| 用文件 ACL 挡住外部写入 | 挡不住同用户的 Agent 进程（同一身份），只挡其他用户。对本威胁模型无效 |
| 始终双写（写 JSON + SQLite、读只读 DB、JSON 作备份） | 写确为单任务级轻量操作（单进程单写者，多一次事务约 1–3 ms），**性能不是问题**；但两个介质各有一个提交点且组合不原子——文件 `rename` 原子、DB 事务原子，二者一起并不原子，崩溃窗口数 = 写次数，写越频繁窗口越多。要可靠就得加 `write_seq` + 启动对账，JSON 从「备份」变成带版本的第二主，违反 ADR-061 零冗余（§1.3 已否决双写）。退一步改成「DB 单写 + 单向导出 JSON」虽只有一个提交点，却等于**无条件触发 D5**：读路径已修（155 ms → 8 ms）、`rebuild_index` 55 ms 非瓶颈，当前规模零收益，还要放弃「文本编辑器修数据」与目录树即权威。另外 PM 的写并非全是单任务：`reparent_task` 改整棵子树 `depth`/`dir_path`、`delete_task` 删子树、`delete_project` 整体归档、派发 tick 批量写 `dispatched_at`。结论：性价比不足，维持树权威。**这不等于给 §3 D6 的 `dual` 翻案**：`dual` 只有一个提交点（树），DB 不参与写成功判定、不参与业务读、可全量重建，属于单向投影而非双写 |

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
| `core/acowork-pm/src/config.rs` | D6-①：`StorageBackend` 三态（`filetree` / `db` / `dual`）+ `storage_backend` 字段，CLI / 环境变量 / TOML 三来源，默认 `filetree` |
| `core/acowork-pm/src/store/tree.rs` | D6-②：新增 `PmMaintenance` 侧接口，`PmStore: PmMaintenance` 保持对象安全；`TreePmStore` 实现维护钩子 |
| `core/acowork-pm/src/api/{mod,routes,tasks}.rs`、`mcp/{mod,tools}.rs`、`dispatch.rs`、`server.rs` | D6-②：`Arc<TreePmStore>` → `Arc<dyn PmStore>`；`index_entry().depth` 改用 `task_depth()` |
| `core/acowork-pm/src/health.rs` | D6-①：`/health` 暴露当前生效的 `storage_backend`，便于灰度期确认配置真的生效 |
| `core/acowork-pm/Cargo.toml` | D6-③：复用 workspace 的 `rusqlite`（bundled），无需系统 SQLite |
| `core/acowork-pm/src/store/sqlite/schema.rs` | D6-③：`projects` / `tasks` / `attachments` / `activity` / `meta` 五表 + 投影列（`parent_id` / `depth` / `task_dir`）；WAL、`foreign_keys=ON`、`user_version=1`；`integrity_check` + `foreign_key_check`；新版本明确拒绝打开 |
| `core/acowork-pm/src/store/sqlite/import.rs` | D6-③：`dry_run` / `import` + `MigrationReport`；显式 `parse()` ID、目录名 ↔ `task.id` ↔ `project_id` 三方一致、缺省字段记账、`children/` → `parent_id`/`depth`、sidecar 归属、单事务 + 非空幂等门 + 校验后提交；5 个单测 |
| `core/acowork-pm/src/store/sqlite/mod.rs` | D6-③：`SqlitePmStore` 连接层 + 实体原语 + 15 个 `PmMaintenance` 方法；dispatch 候选语义与树端对齐（单次持锁，避免 Mutex 重入死锁）；图标目录复用 `icon/`；2 个单测 |
| `core/acowork-pm/src/store/sqlite/business.rs` | D6-③：31 个 `PmStore` 业务方法；复用树端 `validate_transition` / `ensure_creation_approved` / `parse_task_id` / `priority_rank` / `seeded_activity` 单一规则源；活动流与任务记录同事务提交；2 个单测 |
| `core/acowork-pm/src/store/tree.rs` | D6-③：5 个规则辅助函数改 `pub(super)`，供两个后端共用，杜绝逻辑漂移 |
| `core/acowork-pm/src/server.rs` | D6-③：`build_store()` 装配 `db`；空 DB 且树非空时硬失败并提示 `migrate --apply`；`dual` 仍明确拒绝（④ 待做）；3 个单测 |
| `core/acowork-pm/src/main.rs`、`lib.rs` | D6-③：新增 `migrate [--apply]` 运维子命令（缺省 dry-run，打印 JSON 报告）+ 导入器 / `SqlitePmStore` re-export |
| `core/acowork-pm/src/store/dual.rs` | D6-④⑤⑥：新建 `DualPmStore`；树唯一提交点 + 写后全量投影；`dual_read!` / `dual_write!` 委托 31 个 `PmStore` + 15 个 `PmMaintenance` 方法；`projection_lock` 防旧扫描后提交；DB 不可打开时降级为树-only；6 个单测 |
| `core/acowork-pm/src/store/sqlite/import.rs` | D6-④⑥：抽 `insert_batch()` 供一次性导入与投影重建共用；新增 `rebuild_into()` / `rebuild()` 单事务全量替换与 `reconcile()` / `ReconcileReport`（实体 key 集合 + `props` 逐字比对）；对账强制只读打开，缺库 / 版本倾斜 / 完整性失败一律不给绿灯；+3 个单测 |
| `core/acowork-pm/src/store/sqlite/mod.rs` | D6-④：`SqlitePmStore::rebuild_projection()`，仅供 `dual` 覆盖派生数据，`db` 模式不调用 |
| `core/acowork-pm/src/store/mod.rs` | D6-④：导出 `dual` 模块并更新模块文档 |
| `core/acowork-pm/src/server.rs` | D6-④：`build_store()` 三态全接线；`dual` 启动时重建投影；把原「明确拒绝 `dual`」测试换成「可装配且启动即对账一致」 |
| `core/acowork-pm/src/health.rs` | D6-⑥：`dual` 时暴露 `details.projection`（ok / failed / last_error / last_report），投影落后是对账信号而非服务故障 |
| `core/acowork-pm/src/main.rs`、`lib.rs` | D6-⑥：新增 `reconcile` 只读子命令（不一致时 exit 1）+ `DualPmStore` / `rebuild` / `reconcile` / `ReconcileReport` re-export |
| `core/acowork-pm/src/main.rs`、`config.rs` | Review-A：补 `--config` / 默认 `./acowork-pm.toml` 加载层与 `ACOWORK_PM_DATA_DIR`；`StorageBackend` serde alias 与 `FromStr` 统一；`migrate --apply` 失败时补打印只读 dry-run 报告 |
| `core/acowork-pm/src/store/tree.rs` | Review-A/C：`children/` 下钻移到读 `task.json` 之前（坏父不再吞健康子树）；新增 `PmMaintenance::projection_health` 默认实现；`reparent_task` 校验整棵子树平移后的最大深度 |
| `core/acowork-pm/src/store/sqlite/import.rs` | Review-A/B：legacy `seeded_activity()` 补种、`CorruptPolicy` 双策略、`read_dir` / 嵌套 ID / `p-*` 解析错误处理、`safe_rel` 路径校验、`reconcile` 投影列比对、`assert_dual_projection` 硬闸 |
| `core/acowork-pm/src/store/sqlite/business.rs` | Review-A/B：`delete_task` 改为「先删 DB 行、后 best-effort 删资产」；附件路径入库前 + 读盘前双重校验；`create_task_as` / `write_attachment` 失败时回滚已写的 FS 目录；子树深度闸 + `MAX(0, …)` |
| `core/acowork-pm/src/store/sqlite/schema.rs` | Review-C：版本闸门前置到 WAL / DDL 之前；`user_version=0` 且已有 PM 表明确拒绝；`integrity_check` 收集全部结果行 |
| `core/acowork-pm/src/store/sqlite/mod.rs` | Review-B/C：图标目录移出树归档并保留归档只读回退；`dispatch_identity` 与树端一致（已有身份直接返回、不写盘）；补图标回归用例 |
| `core/acowork-pm/src/store/dual.rs` | Review-C：投影指标实例级 + `consecutive_failures`；`tree_write_seq` 并发收敛；投影 DB 打不开时 quarantine 而非删除；非 `dual` backend 拒绝打开投影 |
| `core/acowork-pm/src/store/{corrupt,atomic}.rs`、`health.rs`、`server.rs`、`acowork-runtime/src/tools/builtin/shell.rs` | Review-C：取证副本原子化、失败清理 `.tmp`、`/health` 坏文件脱敏、shell 守卫改为写意图 + 自定义数据目录、`db` 拒绝启动路径零建库副作用 |
| `core/acowork-pm/src/config.rs` | 单后端：`StorageBackend::default()` 改为 `Db`；`FileTree` / `Dual` 保留类型仅供旧 TOML 反序列化；字段注释改为唯一后端说明，默认断言用例同步更新 |
| `core/acowork-pm/src/server.rs` | 单后端：`build_store()` 只装配 `SqlitePmStore` 并在首次启动自动 `ensure_sqlite_authority()`（只读判空 → dry-run → 单事务 import → 版本闸门），每次启动再幂等收口归档；非 `db` 配置硬拒绝；4 个装配用例改写为「退役后端拒绝 / 首次启动自动导入且归档改名后文件数不变 / 二次启动幂等 / 零配置默认 db」 |
| `core/acowork-pm/src/store/sqlite/import.rs` | 归档作废：新增 `retire_tree_archive()`（图标搬到 `assets/projects/{pid}/icon/`、附件搬到 `assets/projects/{pid}/tasks/{tid}/`、改写 `task_dir`、校验无残留引用、`projects/` → `projects.retired`）；崩溃后下次启动可续跑；新增「二进制搬迁 + 改名 + 幂等」回归用例 |
| `core/acowork-pm/src/store/sqlite/{mod,business,import}.rs`、`server.rs`、`api/icon.rs` | 资产目录按项目分组：`task_asset_rel()` 统一生成 `assets/projects/{pid}/tasks/{tid}`，`is_db_owned_asset_dir()` 严格形状判定；`delete_project` 一次清理整棵 `{pid}` 子树；新增 `normalize_asset_layout()` 在启动期幂等升级存量平铺行（含 `attachments` 表与物理目录 rename）并接入 `ensure_sqlite_authority()`；新增 2 个回归用例（布局迁移 + 删项目清理） |
| `config.rs` / `main.rs` / `health.rs` / `lib.rs` / `server.rs` / `store/{mod,dual,tree}.rs` / `store/sqlite/{mod,import,schema,business}.rs` | ⑧ 过渡代码清理：删三态配置、`DualPmStore`、投影 / 对账 / 重建、`migrate` / `reconcile` 子命令、归档图标回退与一次性布局迁移；`/health` 固定报告 `db`；新增「遗留 `storage_backend` 配置硬拒绝」回归用例 |
| `Cargo.toml` / `store/{contract,rules,sqlite}.rs` / `tree.rs` / `index.rs` / `atomic.rs` / `corrupt.rs` / `import.rs` / `health.rs` / `server.rs` / `error.rs` / 5 个测试套件 / `README.md` / `schemas/*` | ⑨ 用完即弃终态收口：删启动期导入、归档收口、整套树实现、恒空坏文件 registry、遗留配置守卫与 6 个孤儿依赖；trait 迁 `contract.rs`、规则迁 `rules.rs`；测试夹具全迁 SQLite；`cascade` 契约补 409 `Conflict`、删 `promote_children`；`/health` 改实时 SQLite 诊断；文档与 OpenAPI 同步 |

**迁移进度（最终状态）**：D6 ①–⑧ 全部完成，⑨ 完成「用完即弃」终态收口。2026-11-17 22:25 真实环境重启后自动导入并收口归档；22:45 UI 新建任务实测只落 `pm.sqlite` + WAL；23:29 再次重启完成 93 行资产布局归位；随后 ⑧⑨ 删除全部过渡与迁移实现。当前 `pm.sqlite` + `assets/projects/{pid}/` 是唯一数据源，运行期没有任何第二提交点、没有导入器、没有归档读写、没有树实现。①–⑥ 的实现细节与两轮 review 记录保留在上表作为历史。

**推分支前 review 与真实数据复验（本轮）**：真实 `data_dir` = `C:\Users\nicholas\.acowork\acowork-pm`，先做完整备份到 `.workbuddy/backups/pm-data-20261116-2300`（robocopy 157/157 文件；Bash `cp` 两次被环境中断，改用 PowerShell 成功），再跑只读命令。初始 dry-run：`projects=4 tasks=91 activity_rows=168 skipped=[] blocking=[]`，精确盘点发现 **39 个任务缺 `activity.json`、其中 23 个带 `result`**——正是 `seeded_activity()` 的 legacy 形态，构成「切 db 后历史活动消失 + `reconcile` 假绿灯」。修完后复跑（真实数据期间自然增长到 92 任务 / 53 个 `activity.json` / 175 行 / 159 文件）：`activity_rows=198`，与 `175 + 23` 精确吻合；`skipped=[] blocking=[]`；前后文件数均 159、无 `pm.sqlite` 产生。`reconcile` 对纯树安装 `exit=1`、`blocking` 明确写「no projection to reconcile」，同样零写入。（该轮当时）⑦/⑧ 未执行，也未对真实目录做任何 `--apply` 或其它写操作；后续 ⑦⑧ 已于 2026-11-17 完成。

**Review 轮验证**：`cargo test -p acowork-pm` 全绿（lib **213 passed** + 9 个集成 / 文档套件共 42 例，合计 255）；`cargo test -p acowork-runtime --lib shell` 79 passed；`cargo clippy -p acowork-pm --all-targets -- -D warnings` 与 `cargo clippy -p acowork-runtime --lib -- -D warnings` 干净；`./dev/ci.sh redlines` 14 条全绿；`git diff --check` 干净；改动的 13 个 Rust 文件按 `--edition 2024 --config skip_children=true` 格式化（`tree.rs` 单独还原后精确重放，避免 694 行无关重排）。中英 ADR 同批更新并保持镜像。

**⑧ 清理验证**：`cargo check -p acowork-pm --all-targets` 通过；`cargo test -p acowork-pm` 全绿（lib **199 passed** + 42 个集成 / 文档用例）；`cargo clippy -p acowork-pm --all-targets -- -D warnings` 干净；`./dev/ci.sh redlines` 14 条全绿；`git diff --check` 干净；新增 / 改动文件按 `--edition 2024 --config skip_children=true` 格式化。副本端到端冒烟：用 `projects.retired` 快照重建一份纯树安装，新二进制启动后自动导入 + 归档改名 + `/health` 报告 `db`，二次启动无导入日志；真实 `data_dir` 全程只读复核，未做任何写操作。中英 ADR 同批更新并保持镜像。

**⑨ 终态验证**：`cargo check -p acowork-pm --all-targets`、`cargo test -p acowork-pm`、`cargo clippy -p acowork-pm --all-targets -- -D warnings`、`./dev/ci.sh redlines`、`git diff --check` 与 rustfmt 全绿；用 `CARGO_INCREMENTAL=0` 避开用户正在运行的 `cargo run` 所占增量 query cache 锁。副本端到端冒烟：空数据目录首次启动直接建空 v1 库、`/health` 报 `db` 且 `details.storage` 有实时诊断、新建任务落 `pm.sqlite` + `assets/projects/{pid}/tasks/{tid}`、`projects/` 从不创建；遗留 `storage_backend` 配置与 `--storage-backend` 均被拒绝、`--help` 不再有 `migrate` / `reconcile`；二次启动幂等。实测数字：`cargo test -p acowork-pm` 为 lib **146 passed** + 集成 / 文档 **42 passed**（合计 188；较 ⑧ 的 199 少 53，正好是随 `tree.rs` 删除的树测试），clippy `-D warnings` 与 redlines 14 条全绿。旧树副本启动后 62 → 65 文件，仅新增 `pm.sqlite` / `-shm` / `-wal`，零删除、零修改，`/projects` 为空且 `projects/` 未改名；空目录实例写项目 + 任务后 `/health.details.storage.rows` 实时从 0 变 1，`cascade=false` 返回 409 `conflict`、`true` 返回 204。真实 `data_dir` 全程只读复核（165 文件不变），未做任何写操作、未重启用户进程（18082 仍 200）。中英 ADR 同批更新并保持镜像。

**D6 ③ 验证（上一轮）**：`cargo test -p acowork-pm` 全量通过（lib **188 passed** + 9 个集成 / 文档测试套件共 42 例全绿）；`cargo clippy -p acowork-pm --all-targets -- -D warnings` 干净；`./dev/ci.sh redlines` 14 条全绿；`acowork-pm --data-dir <tmp> migrate` 实跑 dry-run 输出报告且目录内零文件产生。该轮新增 11 个用例：schema / 连接 2（幂等打开 + 校验、新 schema 版本拒绝）、导入器 5（dry-run 只读、全量往返 + 父子反转 + 幂等门、伪造 ID 阻断、坏 JSON 阻断、新附件不写归档）、业务流 2（31 方法端到端冒烟、依赖存在时 dispatch 不死锁）、server 2（`db` 对象安全可装配、跳过导入时硬失败）。

**D6 ④⑤⑥ 验证（本轮）**：`cargo test -p acowork-pm` 全量通过（lib **197 passed** + 集成 / 文档测试共 42 例全绿，合计 239 例）；`cargo clippy -p acowork-pm --all-targets -- -D warnings` 干净；`./dev/ci.sh redlines` 14 条全绿；`git diff --check` 通过。本轮落地 10 个新用例（`dual.rs` 6：三后端同一断言集、篡改 DB 后业务读仍走树且对账发现漂移、投影失败不影响树提交、启动时重建 stale 投影、并发写后对账一致、混合写流后逐字一致；`import.rs` 3：对账不建库、导入后对账一致、版本倾斜拒绝；`server.rs` 1：`dual` 可装配且启动即一致），其中 `server.rs` 那条替换了原「拒绝 `dual`」测试，故相对 ③ 的 lib 计数净增 9。

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

- 首页耗时与任务总量解耦：树时代徽章是索引聚合，现由单条分组 SQL 聚合，任务从 83 长到 10 000 也只是多一次表扫描。
- `compute_blocked_by` 从 O(1) 读盘降到 O(依赖数) 内存，所有调用方（列表、`only_blocked` 过滤）同步受益。
- 阻塞判定与派发闸门共用同一份索引数据源，减少「两处逻辑漂移」的可能。
- 树时代：树的权威性没有被削弱，索引只是缓存、删掉照样能 `walkdir` 重建；⑨ 后树与索引均已删除，权威性由 `pm.sqlite` 单独承担。

**负面 / 代价**

- `/projects/{pid}/tasks` 仍然返回全量实体（503 KB）。这是看板需要的，不是缺陷；但**不能**再被首页徽章复用。
- `project_counts` 每次都是 O(N) 内存遍历。10 000 任务时约数毫秒，可接受；再往上就该走 D5 的 T1 了。
- D4 前两层（坏文件取证 / `.bak`）已随树实现删除，SQLite 由事务 + `integrity_check` 兜底；第 3 层仍只是字符串级防误伤：变量拼接、`cd` 后的相对路径、解释器一行流仍可绕过；将来若迁库，shell 也同样能写 `pm.sqlite`。OS 级沙箱仍是最终形态。
- （树时代历史）D4 第 2 层的「合法」只到 JSON 语法层：`{"id": 123}` 这类字段类型错误仍会被当作好版本写进 `.bak`，顶掉真正的上一版。要做到 schema 级校验，需给 `atomic_write_json` 加 `DeserializeOwned` 约束，而 `activity.json` 目前以 `&[TaskActivity]` 切片写入、不满足该约束，本轮未改。`.bak` 本身已改成原子替换，所以备份不会再被自身写一半毁掉，但「语义损坏被当好版本」这个口子还在。
- 三态配置（已随 ⑧ 删除）曾把「后端选择」变成**启动期决策**，切换必须重启并走停机窗口；现在没有后端可选，这个代价消失，但也彻底失去在线 AB 与运行时回滚能力。
- `PmStore` 对象化后所有业务调用多一次动态分发（相对 31 个方法里的文件 I/O 可忽略），但 `Arc<dyn PmStore>` 也意味着编译期不再能看出具体实现，回归必须靠 SQLite 后端自身的测试覆盖，不能靠类型系统（三后端三跑已随 ⑧ 删除）。
- SQLite 后端把「单写者」从操作系统文件语义搬到了一把进程内 `Mutex<Connection>`：所有语句都在临界区内同步执行，正确但牺牲了并发读（WAL 现在只用于让外部只读查看不阻塞写）。若将来出现多进程访问同一 `pm.sqlite`，这把锁不再够用，必须改成连接层事务 / busy timeout 策略。
- `db` 模式下附件二进制不进 DB（ADR-061 决策 5），按 `assets/projects/{pid}/tasks/{tid}/` 与图标同属一棵 `{pid}` 子树；`delete_project` 一次清理整棵子树，`delete_task` 只清理任务自己的 DB-owned 目录。代价是 `task_dir` 是 DB 里的路径字符串，未来布局变更需要新的显式迁移（一次性 `normalize_asset_layout()` 已随 ⑧ 删除）；跨项目父子关系被拒绝，所以 `{pid}` 段在任务全生命周期内稳定。
- `dual`（已随 ⑧ 删除）曾用「每次写后全量扫树 + 单事务全量替换 DB」换灰度期最关键的「不可能漏字段 / sidecar / 子树」，代价是每次写 O(tree) 写放大；单一后端下该代价完全消失。
- 投影的 `projection_lock` / `tree_write_seq`（已随 ⑧ 删除）当时解决并发写下的半移动子树问题；现在单一 SQLite 事务天然保证一致性，不再存在跨介质的并发窗口。
- `/health.details.projection` 的实例级计数（已随 ⑧ 删除）曾是「DB 落后」的对账信号；单一后端下 DB 就是权威，不存在「落后」这个状态，`/health` 固定报告 `db`。
- `db_err()` 把全部 `rusqlite` 错误压成 `PmError::Internal`（HTTP 500）。这是有意的：新增错误变体会牵动 `http_status` / `error_code` 的客户端契约，而当前 SQLite 是单进程单写者，约束冲突几乎都意味着真实内部故障而不是用户输入错误。代价是客户端暂时无法区分「唯一键冲突」与「磁盘故障」，日志里才有细节。
- 投影列中的时间戳用 `DateTime<Utc>::to_rfc3339()` 统一写 TEXT，`ORDER BY start_at` 依赖其字典序等于时间序。写路径统一经过同一序列化，**前提是所有时间都是 UTC 且由该函数产出**；若将来接入外部手写的时间字符串，必须先把这一列迁成整数 epoch 或加归一化约束。
- `create_task_as` / `write_attachment` 仍是 FS 先写、DB 后提交。本轮给两者的失败路径加了 best-effort 目录回滚，但进程在两次操作之间被 kill 仍可能留空目录 / 孤儿二进制；彻底解决需要启动期 GC 或把二进制也纳入事务，本轮不做；⑧ 只删过渡代码，未引入启动期 GC，这个遗留仍在。
- `schema::open()` 现在拒绝看不懂的库时不写任何字节，代价是「版本倾斜」与「空库」的恢复都必须由人显式介入（quarantine 后重建），不能再靠一次启动自动收敛；这是把「静默迁移」换成「显式报错」的必然取舍。
- D4 第 3 层的写意图判据是有损的：包含 `>` 的只读文本、字符串里出现 `cp ` 之类 token，都可能被误拒；反过来变量拼接的写仍可绕过。它只是把事故概率压低，绝不写进任何安全承诺。
- 收敛为单后端后不再有「改回 `filetree` 继续用」的运行时回滚路径：运行代码不再读取 `projects.retired`，它只是用户工作区里留存的一份历史快照，与当前二进制无关；旧二进制、旧树、配置开关都已不存在，因此也没有可切换的回滚目标。

**中性**

- 新端点让前端多一个契约面；它足够窄（三个字段），不会变成第二个任务列表。

---

## 7. 开放问题

1. ~~**T1 基线怎么量**~~ **已失效**：`rebuild_index` 与内存索引已随树实现删除，不再有可测的启动重建耗时；SQLite open 是 O(1)。
2. ~~**`.corrupt/` 要不要在 UI 暴露**~~ **已失效**：坏文件取证与 `corrupt_files` 已删除；`/health.details.storage` 现报实时 SQLite 诊断（schema 版本 / integrity / 各表行数 / `props` 解码失败数）。
3. **shell 守卫的兜底**（仍有效）：字符串扫描之外，是否给 PM 数据目录加一个「进程自己持有的 `.lock`」用于事后归因（记录最后写入者）？只解决可观测，不解决阻断。
4. ~~**`/projects` 本身仍是 4 次读盘**~~ **已失效**：项目与任务都在 SQLite 表中，`list_projects` / `project_counts` 均为查询，不再逐目录读 `project.json`。
5. ~~**attachment 元数据在 `task.json` 里**~~ **已失效**：元数据在 `attachments` 表，`props` 解码失败数已进 `/health` 诊断；二进制仍在 `assets/projects/{pid}/tasks/{tid}/attachments/`。

---

## 8. 关联文档

- [ADR-061：PM 存储选型 — 目录树 + 物理嵌套即权威 + 零冗余字段](./ADR-061-pm-storage-tree.md)
- [ADR-082：记忆存储后端迁移至 SQLite](./ADR-082-memory-storage-sqlite-vector-fts.md)
- [ADR-064：PM 独立进程](./ADR-064-pm-standalone-process.md)
- [设计文档 21：PM 项目管理](../design/zh/21-pm-project-management.md)
