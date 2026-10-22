/**
 * Doc REST wire types — 对齐 acowork-doc 服务端
 * (`core/acowork-doc/src/types.rs` + `api/dto.rs`)。
 *
 * 字段名 / 形状与服务端 serde 序列化输出一一对应；时间戳为 RFC 3339 字符串。
 * 人类操作经 Gateway 反代 `/api/doc/*`（doc_proxy 注入可信 `X-Actor: human`）。
 */

/** 根目录固定 id（服务端 `types::ROOT_DIR_ID`） */
export const DOC_ROOT_DIR_ID = "root";

// ── 目录 / 文档元数据 ────────────────────────────────────────────────

/**
 * Add-to-doc 来源（Agent 快照导入时记录，展示「来源标记」）。
 *
 * ADR-073: `instance_id` 是 **runtime instance identity**（UUID），由
 * `X-MCP-Actor` header（Gateway catalog 注入 `{instance_id}` 模板）
 * 透传而来。它是运行时的唯一标识 — 不是包 id（reverse-DNS），同一个包
 * 可以装多个实例。UI 通过 agentStore（按 instance_id 索引）解析为人类
 * 可读的 display name，而不是直接显示原始 UUID。
 */
export interface DocImportSource {
  instance_id: string;
  workspace_path: string;
}

export interface DocMeta {
  doc_id: string;
  name: string;
  version: number;
  import?: DocImportSource | null;
  /** RFC 3339 */
  created_at: string;
  /** RFC 3339 */
  updated_at: string;
  deleted: boolean;
  /** 创建者用户 id（权限 manage 门，设计 §9） */
  creator?: string | null;
  /** 显式 ACL；缺省 = 继承父目录链（设计 §9） */
  acl?: Ace[] | null;
}

export interface DirMeta {
  dir_id: string;
  name: string;
  /** RFC 3339 */
  updated_at: string;
  deleted: boolean;
  /** 创建者用户 id（权限 manage 门，设计 §9） */
  creator?: string | null;
  /** 显式 ACL；缺省 = 继承父目录链（设计 §9） */
  acl?: Ace[] | null;
}

// ── 权限（ACL，设计 §9）──────────────────────────────────────────────

/** 权限级别：`edit` 隐含 `view` */
export type AclRole = "view" | "edit";

/** 单条授权：principal 为用户 id（`u-…`） */
export interface Ace {
  principal: string;
  role: AclRole;
}

/** `GET /api/permissions/{kind}/{id}` 响应 */
export interface PermissionsView {
  /** 节点自身的显式 ACL；null = 继承 */
  explicit: Ace[] | null;
  /** 实际生效的 ACL（继承解析后）；null = 公开 */
  effective: Ace[] | null;
  /** 当前调用者是否有权修改该节点权限（创建者 / admin） */
  can_manage: boolean;
  creator: string | null;
}

/** 文档全文读取：meta + Markdown 原文 + 库内相对路径 */
export interface DocRead {
  meta: DocMeta;
  content: string;
  /** 相对库根路径，如 `项目A/PRD.md` */
  path: string;
}

/** `GET /api/tree?dir_id=` — 某目录的直接子项（非递归） */
export interface DocTreeNode {
  dir_id: string;
  name: string;
  path: string;
  files: DocMeta[];
  dirs: DirMeta[];
}

// ── 更新请求（PR 式审核流，设计 §5）──────────────────────────────────

export type RequestStatus = "pending" | "approved" | "rejected" | "expired";

export interface UpdateRequest {
  request_id: string;
  doc_id: string;
  /** 文档相对路径（提交时解析，供展示） */
  path: string;
  /** Agent 编辑所基于的版本（乐观并发基准） */
  base_version: number;
  content: string;
  /** 提交者标识（runtime instance UUID, ADR-073），如
   *  `inst-aaaa-bbbb-...`。UI 通过 agentStore 解析为 display name。 */
  submitted_by: string;
  status: RequestStatus;
  /** RFC 3339 */
  created_at: string;
  reviewed_at?: string | null;
  reviewed_by?: string | null;
  review_note?: string | null;
}

/** `POST /requests/:id/approve` 响应：已审请求 + 合并后的文档版本 */
export interface ApproveResult {
  request: UpdateRequest;
  doc_version: number;
}

// ── 回收站 / 检索 ────────────────────────────────────────────────────

export interface TrashEntry {
  trash_id: string;
  /** 目录级删除时为 null */
  doc_id?: string | null;
  /** 删除时所在目录（restore 目标） */
  original_dir_id: string;
  /** 删除时标题（文件名去后缀） */
  original_name: string;
  /** RFC 3339 */
  deleted_at: string;
  file_size_bytes: number;
}

export interface SearchHit {
  doc_id: string;
  name: string;
  /** 相对库根路径 */
  path: string;
  snippet: string;
  score: number;
}

// ── 请求体（对齐 dto.rs 字段名）──────────────────────────────────────

export interface CreateDocInput {
  parent_dir_id: string;
  title: string;
  content?: string;
  /** 手工创建无来源；Agent add-to-doc 才带（服务端/MCP 填） */
}

export interface UpdateDocInput {
  base_version: number;
  /** 保存时同步改名（通常 null，标题不变） */
  title?: string | null;
  content: string;
}

export interface RenameDocInput {
  base_version: number;
  new_title: string;
}

export interface MoveDocInput {
  target_dir_id: string;
  overwrite?: boolean;
}

export interface CreateDirInput {
  parent_dir_id: string;
  name: string;
}

export interface RenameDirInput {
  new_name: string;
}

export interface ReviewInput {
  /** 展示名或 `human:xxx`（缺省服务端填 `human:desktop`） */
  reviewed_by?: string;
  /** 拒绝原因 / 通过说明 */
  note?: string;
}
