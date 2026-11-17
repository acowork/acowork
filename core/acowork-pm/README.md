# acowork-pm

项目管理服务（Project & Task Management）。

## 能力

- **项目管理**：CRUD、状态流转（active / archived / completed）
- **任务管理**：CRUD、看板视图、生命周期（pending → in_progress → submitted → approved / rejected）
- **父子任务树**：`parent_id` + `depth` 显式建模，reparent 单事务完成
- **依赖图**：跨任务/跨项目显式声明，运行时计算 blocked 状态
- **附件管理**：图片/文件，缩略图生成（`image-thumb` feature），大小/MIME 白名单
- **REST API**：axum 提供，Gateway 反向代理
- **MCP tools**：HTTP Server 暴露 `pm_*` 工具，Agent 可直接调用

## 存储形态

**SQLite 单一权威（ADR-117）**。`pm.sqlite` 是唯一提交点：实体以原始 JSON
存在 `props` 列，旁边投影 `parent_id` / `depth` / `status` / `task_dir` 等列
供过滤、排序与派发扫描。附件与项目图标的**二进制仍留在文件系统**
（ADR-061 决策 5），按项目分组：

```text
{data_dir}/
├── pm.sqlite                                  # 唯一权威（WAL）
└── assets/projects/{pid}/
    ├── icon/{name}                            # 项目图标
    └── tasks/{tid}/attachments/{att_id}/     # 任务附件二进制
```

**核心不变量**：

- 空数据目录首次启动直接建立空的 v1 库；不读取任何 JSON 树，没有导入步骤。
- 父子树由 `parent_id` / `depth` 表达，跨项目父子关系被拒绝，因此
  `project_id` 在任务全生命周期内不变，资产目录无需跨项目搬移。
- 任务附件元数据（`storage_path` / `thumb_path`）是相对 `task_dir` 的路径，
  读盘前经 `safe_rel()` + `canonicalize()` + 根收敛校验，防路径越界。
- 删除顺序：先快照 DB-owned 资产目录 → 删 DB 行并提交 → best-effort 删物理
  目录。二进制删除失败只留孤儿字节，不会留下悬空引用。
- `cascade=false` 且仍有子任务 / 项目任务时返回 409；`true` 才级联删除。
- `GET /health` 的 `details.storage` 是实时 SQLite 诊断（schema 版本、
  integrity、各表行数、`props` 解码失败数）。

## 模块结构

| 模块 | 职责 |
|------|------|
| `types` | 核心领域类型（Project / Task / Attachment 等） |
| `error` | 统一错误类型 `PmError` 与 HTTP 状态码 / 错误码映射 |
| `config` | `PmConfig` 配置加载（TOML / env / CLI） |
| `store::contract` | `PmStore` / `PmMaintenance` trait，上层唯一边界 |
| `store::rules` | 与存储格式无关的 ID / 状态机 / 路径规则 |
| `store::sqlite` | 唯一后端：schema、业务方法、维护钩子、资产 I/O |
| `api` | axum 路由 + handlers |
| `mcp` | MCP HTTP Server + `pm_*` 工具 manifest |
| `server` | HTTP server 启动入口 |
| `health` | `/health` 探活与实时存储诊断 |

## 设计引用

| 文档 | 角色 |
|------|------|
| [`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) | 服务端设计（数据模型、API、MCP） |
| [`docs/design/zh/22-pm-desktop-ui.md`](../../docs/design/zh/22-pm-desktop-ui.md) | Desktop UX 设计 |
| [`docs/plan/zh/pm-dev-plan.md`](../../docs/plan/zh/pm-dev-plan.md) | 开发计划 |
| [`docs/adr/zh/ADR-061-pm-storage-tree.md`](../../docs/adr/zh/ADR-061-pm-storage-tree.md) | ADR-061 原存储选型（树模型，历史） |
| [`docs/adr/zh/ADR-117-pm-storage-read-path-and-sqlite-triggers.md`](../../docs/adr/zh/ADR-117-pm-storage-read-path-and-sqlite-triggers.md) | ADR-117 收敛到 SQLite 单后端（现行） |

## 构建与运行

```bash
# 编译
cargo build -p acowork-pm

# 编译（含图片缩略图生成）
cargo build -p acowork-pm --features image-thumb

# 测试
cargo test -p acowork-pm

# 独立运行（ADR-064：独立进程，默认端口 18082）
cargo run -p acowork-pm -- --data-dir ./data --port 18082

# 生产形态：由 Gateway supervisor 拉起 + 反向代理（无需手动启动）
cargo run -p acowork-gateway
```

PM 服务以**独立进程**运行（ADR-064）——由 Gateway supervisor 管理生命周期
（spawn / monitor / restart），`/api/pm/*` 经 Gateway 反向代理暴露。独立调试时
可 `cargo run -p acowork-pm` 直接 serve 全量路由（REST + MCP + `/health`）。

## 依赖关系

```text
acowork-pm ──uses──▶ acowork-core    (共享错误/health/supervisor 类型)
                ──▶ rusqlite        (bundled SQLite，唯一权威)
                ──▶ Gateway HTTP    (运行时经 HTTP 查询 /api/agents 校验 assignee)
```

PM 服务**不**依赖 acowork-memory —— 存储层自带 SQLite schema，独立演进。
Gateway **不编译** PM 代码（`cargo tree -p acowork-gateway` 不含 `acowork-pm`）。