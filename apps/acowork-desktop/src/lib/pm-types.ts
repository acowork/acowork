/**
 * PM 领域类型 — 对齐 acowork-pm 服务端契约（core/acowork-pm/src/types.rs）。
 *
 * 约定：字段名与 JSON 直接对应（snake_case），不做 camelCase 转换，
 * 与 Desktop 现有 store（agentStore 等）保持一致，避免映射层漂移。
 */

// ── 枚举 ──────────────────────────────────────────────────────────────

/** 项目状态 */
export type ProjectStatus = "active" | "archived" | "completed";

/** 任务状态（看板列）— 对齐服务端 TaskStatus */
export type TaskStatus =
  | "pending" // 待处理（看板第 1 列）
  | "in_progress" // 进行中（Agent claim 进入）
  | "submitted" // 已提交待审核（Agent submit 进入）
  | "done" // 已完成（review 通过）
  | "rejected" // 已拒绝（review 未通过）
  | "cancelled"; // 已取消

/** 审核状态 */
export type ReviewStatus =
  | "not_required"
  | "pending"
  | "approved"
  | "rejected";

/** 任务类型 */
export type TaskType =
  | "task"
  | "bug"
  | "feature"
  | "chore"
  | "checkpoint"
  | "milestone";

/** 优先级 */
export type Priority = "low" | "normal" | "high" | "urgent";

/** 附件种类 */
export type AttachmentKind = "image" | "file";

/** 依赖种类 */
export type DependencyKind = "blocks" | "relates" | "duplicates";

// ── 实体 ──────────────────────────────────────────────────────────────

/** 项目成员身份类型（ADR-076 §决策 11）。 */
export type PmMemberKind = "agent" | "user";

/** 项目成员（Agent 实例或人类账号，ADR-076 §决策 11）。 */
export interface PmProjectMember {
  /** Agent 实例 ID（UUID，ADR-073）或 User ID（ADR-076，按 `kind` 解释） */
  instance_id: string;
  /**
   * 身份类型。服务端 `#[serde(default)]` = `agent`，兼容旧数据（零迁移）；
   * 前端默认 `agent`。**REST 与 MCP 两条路径都返回**（Rust `ProjectMember`
   * 无 `skip_serializing_if`），消费方按它决定去 agentStore 还是登录账号
   * 解析显示信息 —— 见 `views/pm/pmMembers.ts`。
   */
  kind?: PmMemberKind;
  added_at: string;
  /**
   * Agent 角色（manifest 顶层 `role`，例："Senior Software Engineer"）。
   *
   * **可选字段**：仅在调用方走 MCP `pm_get_project` 路径时填充（PM 服务端
   * 经 `HttpAgentDirectory` join Gateway `AgentListResponse.role` 透出）。
   * REST `GET /projects/:pid` 只返回 `ProjectMember` 原生字段
   * （`instance_id` / `kind` / `added_at`），member 显示信息由前端 join
   * agentStore + 登录账号提供。
   *
   * - 缺失 / 未声明 manifest role → `null`
   * - `NoopAgentDirectory` 宽松模式 → `null`
   *
   * PM task t-2ee347c3：补 role 字段，让前端可在 UI 上展示成员角色而无需
   * 再 join agentStore（MCP 路径）。
   */
  role?: string | null;
}

/** 项目元数据 */
export interface PmProject {
  id: string;
  title: string;
  description: string;
  status: ProjectStatus;
  created_by: string;
  /**
   * 群主（owner）身份。转让群主时后端**只改此字段**，`created_by`
   * 保留“谁创建了项目”的审计语义（不变）。旧项目 / 未转让时为 `null`
   * （后端 `#[serde(default)]`，零迁移），解析时回退 `created_by`。
   * 所有“把 owner 折入成员集合”的地方（`withOwner`）必须走
   * `owner_id ?? created_by`，而非直接读 `created_by`。
   */
  owner_id?: string | null;
  created_at: string;
  updated_at: string;
  metadata: Record<string, unknown>;
  /**
   * 项目成员（Agent 实例列表）。服务端 `#[serde(default)]` + 前端默认 `[]`，
   * 兼容旧服务端/旧缓存数据（零迁移）。
   */
  members: PmProjectMember[];
}

/** 任务实体（不含派生字段） */
export interface PmTask {
  id: string;
  project_id: string;
  title: string;
  description: string;
  type: TaskType;
  status: TaskStatus;
  review_status: ReviewStatus;
  priority: Priority;
  assignee: string | null;
  due_at: string | null;
  /** 开始时间：PM 派发扫描器到期后向 assignee 推送 intent；null = 立即 */
  start_at: string | null;
  /** 派发幂等标记（服务端扫描器写入，只读） */
  dispatched_at: string | null;
  depends_on: PmDependency[];
  attachments: PmAttachmentMeta[];
  result: PmTaskResult | null;
  created_by: string;
  created_at: string;
  updated_at: string;
  claimed_at: string | null;
  submitted_at: string | null;
}

/** 任务完整响应（含派生字段，服务端计算，不写回存储） */
export interface PmTaskResponse extends PmTask {
  is_blocked: boolean;
  blocked_by: string[];
  depth: number;
  parent_id: string | null;
}

/** 依赖声明 */
export interface PmDependency {
  task_id: string;
  kind: DependencyKind;
}

/** Agent 提交结果 */
export interface PmTaskResult {
  text: string;
  attachment_ids: string[];
  submitted_by: string;
  submitted_at: string;
}

/** 附件元数据（二进制文件在服务端 attachments/{id}/ 目录） */
export interface PmAttachmentMeta {
  id: string;
  filename: string;
  kind: AttachmentKind;
  content_type: string;
  size: number;
  sha256: string;
  storage_path: string;
  thumb_path: string | null;
  width: number | null;
  height: number | null;
  uploaded_by: string;
  uploaded_at: string;
}

// ── 请求体 ────────────────────────────────────────────────────────────

export interface CreateProjectInput {
  title: string;
  description?: string;
  metadata?: Record<string, unknown>;
}

/** POST /projects/:pid/members 请求体 */
export interface AddProjectMemberInput {
  instance_id: string;
  /**
   * 身份类型（ADR-076 §决策 11）：`"user"` → `instance_id` 按 user_id
   * 解释，服务端跳过 Agent 存在性校验。服务端 `#[serde(default)]` =
   * `"agent"`，旧调用方不发该字段时行为不变。
   */
  kind?: PmMemberKind;
}

export interface UpdateProjectInput {
  title?: string;
  description?: string;
  status?: ProjectStatus;
  metadata?: Record<string, unknown>;
}

export interface CreateTaskInput {
  title: string;
  description?: string;
  type?: TaskType;
  priority?: Priority;
  parent_task_id?: string | null;
  depends_on?: PmDependency[];
  attachment_ids?: string[];
  /** 开始时间（RFC3339）；缺省 = 任务可执行后下一个扫描 tick 派发 */
  start_at?: string | null;
}

export interface UpdateTaskInput {
  title?: string;
  description?: string;
  type?: TaskType;
  status?: TaskStatus;
  priority?: Priority;
  assignee?: string | null;
  due_at?: string | null;
  start_at?: string | null;
  depends_on?: PmDependency[];
}

export interface ReparentInput {
  new_parent: string | null;
}

export interface ReviewInput {
  approved: boolean;
  /** 拒绝理由（可选，对齐服务端 ReviewTaskRequest._comment） */
  _comment?: string;
}

export interface SubmitInput {
  text: string;
  attachment_ids?: string[];
}

// ── 看板分组辅助 ───────────────────────────────────────────────────────

/** 看板列定义（对齐服务端 board_column + 审核语义） */
export const BOARD_COLUMNS: {
  status: TaskStatus;
  key: string;
  i18nKey: string;
}[] = [
  { status: "pending", key: "pending", i18nKey: "pm.board.pending" },
  { status: "in_progress", key: "in_progress", i18nKey: "pm.board.inProgress" },
  { status: "submitted", key: "submitted", i18nKey: "pm.board.submitted" },
  { status: "done", key: "done", i18nKey: "pm.board.done" },
];
