# ADR-070: acowork-doc 独立进程 + 目录树存储选型

**状态**：已决策（2026-09，D0–D4 实施完成时定案）
**决策者**：架构评审（用户定案：Gateway 零业务铁律 + 参考 runtime usecase 的 service 层）
**关联**：
- [ADR-064](./ADR-064-pm-standalone-process.md)（PM 独立进程——doc 的直接先例，本 ADR 将同一范式扩展到 doc）
- [ADR-061](./ADR-061-pm-storage-tree.md)（PM 目录树存储——doc 复用其「文件系统即真相」哲学）
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（Gateway 收敛为纯网络职责；远程 advertise 下发）
- [docs/design/zh/20-doc-online-document.md](../../design/zh/20-doc-online-document.md)（doc 设计 v1.0，本 ADR 记录其对 §2.2 数据目录的偏差修正）

---

## 背景

### Gateway 零业务铁律同样约束 doc

ADR-064 已确立：业务逻辑不得进入 Gateway（embed、LSP relay、PM 均已独立为子进程）。`acowork-doc` 的领域逻辑（目录树存储、版本号乐观并发、PR 式审核流、检索）若内嵌进 Gateway，将重蹈 PM 的全部问题：

| 问题 | 说明 |
|------|------|
| 业务逻辑入 Gateway | doc 领域（library.json 一致性、.trash/.requests 生命周期、审核状态机）编译进 Gateway 二进制 |
| 依赖重量 | doc 引入 chrono/uuid/serde_json/tokio/axum 等（gateway 已因业务历史超重） |
| 故障隔离 | doc panic（如损坏的 library.json 触发 reconcile panic）可能带崩 Gateway 单点 |
| 存储耦合 | doc 数据被塞进 `{gateway.data_dir}/acowork-doc`，与 Gateway 数据生命周期强耦合 |

### doc 与 pm 的差异（不变量不同）

| 维度 | pm | doc |
|------|----|----|
| 根实体 | 项目（task 恒为目录，ADR-061 决策 4） | 文档 = `.md` 文件（物理文件即权威） |
| 目录语义 | `children/` 隔离子目录 | **目录 = 文件夹**（直接对应文件系统目录） |
| 内容载体 | `task.json` + 附件目录 | Markdown 原文即内容（无独立元数据文件，元数据在 `library.json` 索引） |
| 并发 | 版本号 | 版本号（同款乐观并发，`base_version` 校验） |
| 修改路径 | 人类直改；Agent claim→submit 审核 | 人类 PUT 直改；Agent `doc_write`（改他人文档时策略返回 `pending_review` → PR 式审核） |

### 设计文档偏差（本 ADR 记录修正）

设计 §2.2/Q8 初稿写数据目录 `{data}/acowork-doc/`（嵌套 Gateway 数据目录）；实现时与 pm 对齐改为 `$HOME/.acowork/acowork-doc/`（平级独立），理由同 ADR-064 目标 3（存储生命周期与 Gateway 解耦）。本 ADR 将实际决策固化，设计 v1.0 已同步。

---

## 目标

1. **doc 独立进程**：独立二进制 `acowork-doc`、独立端口（默认 18081，冲突自动递增至 +20）、supervisor 生命周期管理
2. **doc 存储独立**：数据目录 `$HOME/.acowork/acowork-doc/`，与 gateway/node/pm 平级
3. **文件系统即真相 + 每目录索引**：目录 = 文件夹、文档 = `.md` 文件；每目录 `library.json` 为加速索引（可重建），物理布局为权威
4. **Gateway 仅保留**：spawn/monitor/restart（doc_supervisor）+ 反代 `/api/doc/*`（doc_proxy 注入可信 X-Actor / 校验 X-MCP-Actor）+ MCP catalog 注入 `doc_mcp_url`
5. **对外契约**：Desktop `{gw}/api/doc/*`；Agent `http://{advertise_host}:{gw_http_port}/api/doc/mcp`——两端无感知

---

## 可选方案

### 方案 A：独立进程（推荐，已实施）
`acowork-doc` crate 自带 `main.rs`，serve 完整 router（REST `/api/*` 内部路径 + `/mcp` + `/health`）。Gateway `doc_supervisor` spawn + 轮询 `/health` + 指数退避重启；启动失败不阻塞 Gateway（503 + Retry-After）。doc_proxy 透明代理 `/api/doc/{rest}` → `127.0.0.1:{doc_port}/{rest}`。

### 方案 B：doc 内嵌 Gateway（拒绝）
同 ADR-064 方案 A vs B 的论证：重复业务入网关、依赖膨胀、故障传播。不采纳。

### 方案 C：doc 服务内嵌到 pm 进程（拒绝）
两业务领域合并会制造耦合面（pm 目录树 vs doc 目录树语义不同，见差异表），且任一模块变更都要求整体重启。不采纳。

---

## 决策

### 决策 1：doc 为独立进程（方案 A）

- `core/acowork-doc` 产出 `acowork-doc.exe/bin`；`[doc]` 配置（enabled/port/data_dir/request_ttl_hours/auto_inject_mcp/mcp_http_path）由 Gateway config 透传 CLI。
- 端口默认 18081；占用则自动递增探测（最多 +20），端口写入 `doc.port` 供 doc_proxy 读取。
- 失败**非致命**：doc 不可用时 `/api/doc/*` 返回 503 + `Retry-After`，Desktop 显示离线面板而非白屏。

### 决策 2：文件系统即真相，目录 = 文件夹，文档 = `.md` 文件

- 物理布局 = 权威：目录对应文件夹、文档对应 `.md` 文件、文件名（去后缀）= 标题。
- 每目录 `library.json` 仅作**加速索引**（doc_id ↔ 物理名映射 + import 来源 + deleted 软删标记）；`reconcile`（启动时三 pass：rename 修复 → orphan 标记 → 补缺失）可重建，索引损坏不丢内容。
- 隐藏目录 `.trash/`（软删 + sidecar 恢复信息）、`.requests/`（PR 请求 JSON），均不进入目录树。
- 全部写操作原子替换（临时文件 + rename），无中间态。

### 决策 3：数据目录 = `$HOME/.acowork/acowork-doc/`（修正设计 §2.2）

与 `acowork-pm/` 平级独立，**不嵌套 Gateway 数据目录**；用户可通过 `[doc].data_dir` 覆盖。

### 决策 4：版本号 `u64` + 乐观并发

写库/审核合并均携带 `base_version` 校验；不匹配 → 409 `version_conflict`（e2e 语义 = git push 被拒）。修正早期 D0 草案把版本号字段错标为 `i64` 的问题（无符号语义：版本单调递增不为负）。

### 决策 5：人类与 Agent 身份由 Gateway 反代统一注入

- REST `/api/doc/*`：doc_proxy 丢弃客户端自报 `X-Actor`，注入可信 `human`（防伪造 `agent:xxx`）。
- MCP `/api/doc/mcp`：doc_proxy 校验 `X-MCP-Actor ∈ installed_agents` → 透传（受信）；否则剥离 → 匿名（仅只读工具 list/read/search/pull/check_request）。
- doc server 侧不自行维护 agent 白名单（信任决策收敛在 Gateway 单一鉴权点）。

### 决策 6：治理 = 策略（不由动词决定），体积 = 两个上限，替换 = 进回收站

- **写权限是策略不是动词**（`service/policy.rs`，设计 §5.1）：`doc_write` 一个动词
  覆盖 create/overwrite/append；改自己的文档（`creator == actor`）或管理员直接生效，
  改他人文档 / 无创建人记录 → `pending_review` + `request_id`。旧的三动词
  （`doc_add`/`doc_submit_update`/`doc_pull`）把治理编码进名字，使「create 但要审核」
  「overwrite 但不审核」不可表达。
- **结构性操作按所有权**（`move` / `remove`）：审核请求只表达「内容提案」，表达不了
  删除/改名，所以它们不进审核流。无创建人记录的历史文档例外地**不**按所有权拒绝，
  改由节点 ACL 裁决——没有审核通道时一律拒绝会让它永久不可达（死锁，不是保护）。
- **两个体积上限**（`limit.rs`）：`transport_ceiling`（线上 JSON 体积，含最坏转义系数）
  与 `ensure_document_size`（解码后正文，含 `append` 的**结果**大小）。只查前者会把
  「正文合法但转义膨胀」误拒；只查后者则放行无界内存。413 一律回
  `payload_too_large` JSON 信封（`DocJson` 自定义 extractor 绕过 axum 纯文本拒绝）。
- **替换 = 进回收站**：`move_doc` 的 overwrite 分支把被替换文档写入 `.trash/` 并保留
  正文以便回滚；先把目标删掉再移动（旧 MCP 实现）会让「移动失败」留下「目标已删、
  源未动」的中间态。

---

## 后果

### 正面
- Gateway 保持零业务：doc 领域逻辑与二进制完全隔离。
- Desktop 与 Agent 两端契约稳定（`/api/doc/*`、`/api/doc/mcp` 不随内部结构调整）。
- 存储 = 纯文件，备份 = 拷贝目录；崩溃恢复 = 重启 + reconcile。
- 身份伪造面收敛到 doc_proxy 单一注入点（doc_proxy 13 项单测覆盖）。

### 负面 / 成本
- 多一个常驻进程（~10 MB 级）；doc 启动有首次 reconcile 延迟（与库规模线性）。
- REST 与 MCP 双入口需保持语义一致（D3-5/D4-3 e2e 持续校验）。
- 每目录 `library.json` 需在结构变更后即时刷新，否则 reconcile 兜底（可接受，服务内单写者）。

### 回滚
- Gateway `[doc].enabled=false` 即整体停用（/api/doc/* → 503，不影响其它服务）。
- 数据目录切换：改 `[doc].data_dir` 后重启，原目录保留可拷贝回滚。
