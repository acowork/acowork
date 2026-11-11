# acowork-pm — Schema 与 REST 契约

> 本目录是 acowork-pm 的**机器可读契约资产**：

| 文件 | 用途 | 权威来源 |
|------|------|----------|
| [`project.schema.json`](./project.schema.json) | 项目元数据 `project.json` 存储 Schema | `src/types.rs::Project` |
| [`task.schema.json`](./task.schema.json) | 任务元数据 `task.json` 存储 Schema | `src/types.rs::Task` |
| [`pm-openapi-3.1.json`](./pm-openapi-3.1.json) | **REST API 契约（OpenAPI 3.1）** | `src/api/routes.rs` + `src/types.rs` |
| [`README.md`](./README.md) | 本文件：路由表 + 错误码 + curl 示例 | 同上 |

**原则**：存储 Schema 与 OpenAPI 均以 `src/types.rs` / `src/api/routes.rs` 为**实现态权威**。
设计与本契约冲突时以本契约为准（设计文档 [`docs/design/zh/21-pm-project-management.md`](../../../docs/design/zh/21-pm-project-management.md) §5 为意图态）。

---

## 1. 基础约定

- **Base URL**（生产）：经 Gateway 反向代理 `{gw}/api/pm/...` → 剥 `/api/pm` 前缀转发到 pm 服务内部路径（不带 `/api` 前缀，见 §2）。PM 为**独立进程**（ADR-064），内部监听 `127.0.0.1:{pm_port}`（默认 18082）。
- **Base URL**（开发）：PM router 内部路径不带 `/api` 前缀，可直接独立 serve（`/projects`、`/tasks/:tid` 等）；`cargo run -p acowork-pm` 即起独立进程。
- **内容类型**：`application/json; charset=utf-8`（附件上传 / 下载除外）。
- **鉴权**：复用 Gateway Desktop 会话鉴权（Bearer Token，见 [`docs/protocols/zh/http.md`](../../../docs/protocols/zh/http.md) §1）。
- **Actor 标识**：写操作与 `claim` / `submit` / `review` 依赖 HTTP header `X-Actor`
  （Gateway 注入当前用户 / Agent ID）。缺失时 `create` 回退为 `"unknown"`；
  `claim` / `submit` / `review` 缺失返回 500。
- **ID 格式**：`p-` / `t-` / `att-` 前缀 + UUID 短片段，字符集 `[a-zA-Z0-9-]`，长度 3–64。
- **错误格式**：统一 `{"error": {"code": "...", "message": "..."}}`，见 §3。

---

## 2. 路由表

> 与 [`src/api/routes.rs`](../../../core/acowork-pm/src/api/routes.rs) `pm_router` 逐条对齐。
> 路径以**公开形式**（带 `/api/pm` 前缀）展示；PM router 内部路径**不带** `/api`，由 Gateway [`pm_proxy.rs`](../../../core/acowork-gateway/src/http/pm_proxy.rs) 反代时剥离前缀。

| 方法 | 路径 | 说明 | 实现 |
|------|------|------|------|
| GET | `/api/pm/projects` | 项目列表 | ✅ `src/api/projects.rs` |
| POST | `/api/pm/projects` | 新建项目 | ✅ |
| GET | `/api/pm/projects/{pid}` | 项目详情 | ✅ |
| PATCH | `/api/pm/projects/{pid}` | 更新项目 | ✅ |
| DELETE | `/api/pm/projects/{pid}?cascade=true` | 删除项目（级联任务） | ✅ |
| GET | `/api/pm/projects/{pid}/tasks?status=&assignee=&only_blocked=&sort=` | 项目内任务列表/检索 | ✅ `src/api/tasks.rs` |
| POST | `/api/pm/projects/{pid}/tasks` | 新建任务 | ✅ |
| GET | `/api/pm/tasks/{tid}` | 任务详情（含 `is_blocked`/`blocked_by`/`depth` 派生字段） | ✅ |
| PATCH | `/api/pm/tasks/{tid}` | 更新任务（全字段可选） | ✅ |
| DELETE | `/api/pm/tasks/{tid}?cascade=&promote_children=` | 删除任务（子树级联） | ✅ |
| PATCH | `/api/pm/tasks/{tid}/parent` | 移动任务（`new_parent=null` 提升为根；DFS 防环） | ✅ |
| POST | `/api/pm/tasks/{tid}/claim` | Agent 认领（pending → in_progress） | ✅ |
| POST | `/api/pm/tasks/{tid}/submit` | Agent 提交结果（in_progress → submitted） | ✅ |
| POST | `/api/pm/tasks/{tid}/review` | 人类审核（approved → done / rejected） | ✅ |
| GET | `/api/pm/tasks/{tid}/children` | 直接子任务列表 | ✅ |
| GET | `/api/pm/tasks/{tid}/attachments` | 附件元数据列表 | ✅ |
| POST | `/api/pm/tasks/{tid}/attachments` | 上传附件（multipart，≤10MB） | ✅ `src/api/attachments.rs` |
| GET | `/api/pm/attachments/{aid}?download=&thumb=` | 下载附件 | ✅ |
| DELETE | `/api/pm/attachments/{aid}` | 删除附件 | ✅ |

> 附件上传 / 下载 / 缩略图已完整实现（P1 收尾完成）。

### 与设计文档 §5 的差异（意图态 → 实现态）

| 设计文档（意图） | 实现（本契约） | 说明 |
|------------------|----------------|------|
| `PATCH /api/pm/tasks/:id/status` | `PATCH /api/pm/tasks/:id`（`status` 字段）+ `claim`/`submit`/`review` 专属端点 | 状态机收敛到领域操作，避免裸改状态绕过约束 |
| `POST /api/pm/tasks/:id/approve` / `reject` | `POST /api/pm/tasks/:id/review` `{approved: bool}` | 二合一 |
| `POST /api/pm/tasks/:id/notes` | 未实现（P1 起通过 `submit.text` 承载结果；备注功能后置） | 待 P2+ |
| `GET /api/pm/agents` | 未实现（P3 通过 Gateway 校验） | 待 P3 |
| `GET /api/pm/reviews` | 未实现（P3 需要） | 待 P3 |
| `GET /api/pm/tasks?project_id=` | `GET /api/pm/projects/{pid}/tasks`（项目维路径化） | 项目必选，简化查询 |

---

## 3. 错误码表

> 与 [`src/error.rs`](../../../core/acowork-pm/src/error.rs) `PmError::http_status()` / `error_code()` 逐条对齐。
> 响应体：`{"error": {"code": "<snake_case>", "message": "<人类可读>"}}`。

| HTTP | code | 触发条件 |
|------|------|----------|
| 404 | `project_not_found` | 项目不存在 |
| 404 | `task_not_found` | 任务不存在 |
| 404 | `attachment_not_found` | 附件不存在 |
| 400 | `invalid_id` | ID 格式非法（`FromStr` 解析失败） |
| 400 | `path_traversal` | 路径穿越尝试（物理路径越界） |
| 400 | `reserved_id` | 使用了保留 ID 名 |
| 400 | `max_depth_exceeded` | 任务树超过最大深度（默认 5） |
| 400 | `too_many_children` | 直接子任务超过上限（1000） |
| 400 | `too_many_attachments` | 附件数量超限 |
| 400 | `attachment_too_large` | 附件超过大小上限（10MB） |
| 400 | `attachment_mime_rejected` | MIME 不在白名单 |
| 400 | `invalid_state_transition` | 非法状态流转（如 done → pending） |
| 409 | `cycle_detected` | Reparent 形成环 |
| 409 | `dependency_cycle` | 依赖图存在环 |
| 409 | `dependency_not_satisfied` | claim 时被 `blocks` 依赖阻塞 |
| 500 | `io_error` | 文件系统错误 |
| 500 | `json_error` | JSON 序列化/反序列化错误 |
| 500 | `multipart_error` | multipart 解析错误 |
| 500 | `image_error` | 图片处理错误（缩略图） |
| 500 | `internal_error` | 内部错误 / 缺失 `X-Actor` |

---

## 4. curl 示例

> 以下示例经 Gateway 反代路径 `http://127.0.0.1:19876/api/pm/...`（生产）。
> 开发独立 serve 时路径不带 `/api` 前缀（如 `http://127.0.0.1:{pm_port}/projects`）。
> 携带 `X-Actor` 表示操作者。

```bash
# ── 项目 ─────────────────────────────────────────────
# 新建项目
curl -s -X POST http://127.0.0.1:19876/api/pm/projects \
  -H 'Content-Type: application/json' \
  -H 'X-Actor: human' \
  -d '{"title": "Q1 产品迭代", "description": "2025 Q1"}'

# 项目列表
curl -s http://127.0.0.1:19876/api/pm/projects

# 项目详情（pid 来自上一步返回）
curl -s http://127.0.0.1:19876/api/pm/projects/p-3f2a9c1e

# 删除项目（级联任务）
curl -s -X DELETE "http://127.0.0.1:19876/api/pm/projects/p-3f2a9c1e?cascade=true"

# ── 任务 ─────────────────────────────────────────────
# 新建根任务（人类创建 → 直接生效）
curl -s -X POST http://127.0.0.1:19876/api/pm/projects/p-3f2a9c1e/tasks \
  -H 'Content-Type: application/json' \
  -H 'X-Actor: human' \
  -d '{"title": "实现登录页", "type": "task", "priority": "high"}'

# 新建子任务（parent_task_id 指向根任务）
curl -s -X POST http://127.0.0.1:19876/api/pm/projects/p-3f2a9c1e/tasks \
  -H 'Content-Type: application/json' \
  -H 'X-Actor: human' \
  -d '{"title": "表单校验", "parent_task_id": "t-8b7d5e2f"}'

# 任务列表（按状态过滤）
curl -s "http://127.0.0.1:19876/api/pm/projects/p-3f2a9c1e/tasks?status=pending"

# 任务详情（含 is_blocked / blocked_by / depth）
curl -s http://127.0.0.1:19876/api/pm/tasks/t-8b7d5e2f

# 更新任务（指派 / 优先级）
curl -s -X PATCH http://127.0.0.1:19876/api/pm/tasks/t-8b7d5e2f \
  -H 'Content-Type: application/json' \
  -H 'X-Actor: human' \
  -d '{"assignee": "agent-1", "priority": "urgent"}'

# 移动任务到根（null 提升）
curl -s -X PATCH http://127.0.0.1:19876/api/pm/tasks/t-8b7d5e2f/parent \
  -H 'Content-Type: application/json' \
  -d '{"new_parent": null}'

# 子任务列表
curl -s http://127.0.0.1:19876/api/pm/tasks/t-8b7d5e2f/children

# ── 生命周期（Agent 操作，X-Actor=agent_id）──────────
# 认领
curl -s -X POST http://127.0.0.1:19876/api/pm/tasks/t-8b7d5e2f/claim \
  -H 'X-Actor: agent-1'

# 提交结果
curl -s -X POST http://127.0.0.1:19876/api/pm/tasks/t-8b7d5e2f/submit \
  -H 'Content-Type: application/json' \
  -H 'X-Actor: agent-1' \
  -d '{"text": "已完成登录页开发"}'

# 人类审核通过
curl -s -X POST http://127.0.0.1:19876/api/pm/tasks/t-8b7d5e2f/review \
  -H 'Content-Type: application/json' \
  -H 'X-Actor: human' \
  -d '{"approved": true}'

# ── 附件 ─────────────────────────────────────────────
# 上传附件（multipart）
curl -s -X POST http://127.0.0.1:19876/api/pm/tasks/t-8b7d5e2f/attachments \
  -H 'X-Actor: agent-1' \
  -F 'file=@./design.png' -F 'kind=image'

# 附件列表
curl -s http://127.0.0.1:19876/api/pm/tasks/t-8b7d5e2f/attachments

# 下载附件（强制下载）
curl -s -OJ "http://127.0.0.1:19876/api/pm/attachments/att-1c2d3e4f?download=1"

# 删除附件
curl -s -X DELETE http://127.0.0.1:19876/api/pm/attachments/att-1c2d3e4f

# ── 错误示例 ─────────────────────────────────────────
# 非法 ID → 400 invalid_id
curl -s http://127.0.0.1:19876/api/pm/tasks/not-a-task

# 任务不存在 → 404 task_not_found
curl -s http://127.0.0.1:19876/api/pm/tasks/t-00000000

# 重复依赖环 → 409 cycle_detected（reparent 示例）
curl -s -X PATCH http://127.0.0.1:19876/api/pm/tasks/t-child/parent \
  -H 'Content-Type: application/json' \
  -d '{"new_parent": "t-parent"}'
```

---

## 5. 与 MCP 工具的对应

| MCP 工具 | 底层 REST |
|----------|-----------|
| `pm_list_projects` / `pm_get_project` / `pm_create_project` | `GET/POST /api/pm/projects`、`GET /api/pm/projects/{pid}` |
| `pm_list_tasks` | `GET /api/pm/projects/{pid}/tasks` |
| `pm_get_task` | `GET /api/pm/tasks/{tid}` |
| `pm_create_task` | `POST /api/pm/projects/{pid}/tasks` |
| `pm_update_task` | `PATCH /api/pm/tasks/{tid}` |
| `pm_claim_task` | `POST /api/pm/tasks/{tid}/claim` |
| `pm_submit_task` | `POST /api/pm/tasks/{tid}/submit` |

> Agent 不直接调 REST（由 Gateway 注入 `X-Actor` 身份），走 MCP HTTP（`src/mcp/`）。
