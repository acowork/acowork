/**
 * Doc REST API 客户端 — 对齐 acowork-doc 服务端（core/acowork-doc/src/api）。
 *
 * 路径前缀：`{gw}/api/doc/...`（Gateway doc_proxy 反代挂载，剥 `/api/doc`
 * 前缀转发到 doc 进程；见 acowork-gateway/src/http/doc_proxy.rs）。
 * 错误格式：`{"error": {"code": "...", "message": "..."}}`（见 acowork-doc/src/api/mod.rs）。
 * 人类操作默认带 `X-Actor: human`（Gateway 会覆盖为可信 human，防伪造 agent:xxx）。
 */

import { getGatewayUrl } from "./config";
import { NOT_AUTHORIZED, permissionMessage } from "./api-error";
import type {
  Ace,
  ApproveResult,
  CreateDirInput,
  CreateDocInput,
  DirMeta,
  DocMeta,
  DocRead,
  DocTreeNode,
  MoveDocInput,
  PermissionsView,
  RenameDirInput,
  ReviewInput,
  SearchHit,
  TrashEntry,
  UpdateDocInput,
  UpdateRequest,
} from "./doc-types";

/** 当前人类操作者标识（服务端约定：human，doc_proxy 注入） */
const HUMAN_ACTOR = "human";

export class DocApiError extends Error {
  readonly code: string;
  readonly status: number;
  /** For denials: the capability tier that refused (`use`/`manage`/…). */
  readonly required?: string;

  constructor(status: number, code: string, message: string, required?: string) {
    super(message);
    this.name = "DocApiError";
    this.code = code;
    this.status = status;
    this.required = required;
  }
}

async function request<T>(
  path: string,
  init?: RequestInit & { actor?: string },
): Promise<T> {
  const { actor = HUMAN_ACTOR, ...rest } = init ?? {};
  const headers: Record<string, string> = {
    "X-Actor": actor,
    ...(rest.headers as Record<string, string> | undefined),
  };
  if (rest.body && !(rest.body instanceof FormData)) {
    headers["Content-Type"] = "application/json";
  }

  const res = await fetch(`${getGatewayUrl()}/api/doc${path}`, {
    ...rest,
    headers,
  });

  if (!res.ok) {
    let code = "http_error";
    let message = `HTTP ${res.status}`;
    let required: string | undefined;
    try {
      const body = (await res.json()) as {
        error?: { code?: string; message?: string; required?: string } | string;
        code?: string;
        message?: string;
        required?: string;
      };
      if (typeof body.error === "object" && body.error !== null) {
        // acowork-doc's nested envelope {"error":{"code","message","required"}}.
        code = body.error.code ?? code;
        message = body.error.message ?? message;
        required =
          typeof body.error.required === "string" ? body.error.required : undefined;
      } else if (typeof body.code === "string") {
        // Gateway ACL middleware's flat envelope (403 fires here before
        // the request ever reaches the doc service).
        code = body.code;
        message = body.message ?? message;
        required = typeof body.required === "string" ? body.required : undefined;
      }
    } catch {
      // 非 JSON 响应体，保留默认
    }
    // 权限拒绝统一换成可操作的本地化文案（ADR-087 D5.3），
    // 服务端原文保留在 DocApiError.code / required 里供分类。
    if (res.status === 403 || code === NOT_AUTHORIZED) {
      message = permissionMessage(required);
    }
    throw new DocApiError(res.status, code, message, required);
  }

  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

// ── 健康探测 ──────────────────────────────────────────────────────────

/** doc 进程 /health（doc_proxy 透明转发）。200 = 在线；503 = 未就绪/离线。 */
export function checkHealth(): Promise<boolean> {
  return fetch(`${getGatewayUrl()}/api/doc/health`, {
    signal: AbortSignal.timeout(3000),
  })
    .then((res) => res.ok)
    .catch(() => false);
}

// ── 目录树 ────────────────────────────────────────────────────────────

/** `GET /api/tree?dir_id=` — 取某目录直接子项（dir_id 缺省 = root） */
export function getTree(dirId?: string): Promise<DocTreeNode> {
  const q = dirId ? `?dir_id=${encodeURIComponent(dirId)}` : "";
  return request<DocTreeNode>(`/tree${q}`);
}

/** `POST /api/dirs` — 新建子目录 */
export function createDir(input: CreateDirInput): Promise<DirMeta> {
  return request<DirMeta>("/dirs", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `GET /api/dirs/:dir_id` — 目录元数据 */
export function getDir(dirId: string): Promise<DirMeta> {
  return request<DirMeta>(`/dirs/${encodeURIComponent(dirId)}`);
}

/** `PATCH /api/dirs/:dir_id/name` — 重命名目录 */
export function renameDir(dirId: string, input: RenameDirInput): Promise<DirMeta> {
  return request<DirMeta>(`/dirs/${encodeURIComponent(dirId)}/name`, {
    method: "PATCH",
    body: JSON.stringify(input),
  });
}

/** `DELETE /api/dirs/:dir_id` — 删除目录（级联入回收站） */
export function deleteDir(dirId: string): Promise<void> {
  return request<void>(`/dirs/${encodeURIComponent(dirId)}`, { method: "DELETE" });
}

// ── 文档 CRUD ─────────────────────────────────────────────────────────

/** `POST /api/docs` — 新建文档（201） */
export function createDoc(input: CreateDocInput): Promise<DocMeta> {
  return request<DocMeta>("/docs", {
    method: "POST",
    body: JSON.stringify({ ...input, content: input.content ?? "" }),
  });
}

/** `GET /api/docs/:doc_id` — 读全文 */
export function getDoc(docId: string): Promise<DocRead> {
  return request<DocRead>(`/docs/${encodeURIComponent(docId)}`);
}

/** `PUT /api/docs/:doc_id` — 直接更新（人类路径，带 base_version 乐观并发） */
export function updateDoc(docId: string, input: UpdateDocInput): Promise<DocMeta> {
  return request<DocMeta>(`/docs/${encodeURIComponent(docId)}`, {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

/** `PATCH /api/docs/:doc_id/title` — 重命名（含版本，改名会动磁盘文件名） */
export function renameDoc(docId: string, baseVersion: number, newTitle: string): Promise<DocMeta> {
  return request<DocMeta>(`/docs/${encodeURIComponent(docId)}/title`, {
    method: "PATCH",
    body: JSON.stringify({ base_version: baseVersion, new_title: newTitle }),
  });
}

/** `POST /api/docs/:doc_id/move` — 移动文档到目标目录 */
export function moveDoc(docId: string, input: MoveDocInput): Promise<DocMeta> {
  return request<DocMeta>(`/docs/${encodeURIComponent(docId)}/move`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `DELETE /api/docs/:doc_id` — 删除（入回收站，204） */
export function deleteDoc(docId: string): Promise<void> {
  return request<void>(`/docs/${encodeURIComponent(docId)}`, { method: "DELETE" });
}

/** `GET /api/docs/:doc_id/path` — 相对路径 */
export async function getDocPath(docId: string): Promise<string> {
  const v = await request<{ path: string }>(`/docs/${encodeURIComponent(docId)}/path`);
  return v.path;
}

// ── 更新请求（审核队列）──────────────────────────────────────────────

/** `GET /api/requests?status=` — 请求列表（缺省全部；审核队列用 pending） */
export function listRequests(status?: string): Promise<UpdateRequest[]> {
  const q = status ? `?status=${encodeURIComponent(status)}` : "";
  return request<UpdateRequest[]>(`/requests${q}`);
}

/** `GET /api/requests/:request_id` */
export function getRequest(requestId: string): Promise<UpdateRequest> {
  return request<UpdateRequest>(`/requests/${encodeURIComponent(requestId)}`);
}

/** `POST /api/requests/:request_id/approve` — 审核通过（合并入库） */
export function approveRequest(requestId: string, input: ReviewInput): Promise<ApproveResult> {
  return request<ApproveResult>(`/requests/${encodeURIComponent(requestId)}/approve`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `POST /api/requests/:request_id/reject` — 审核拒绝 */
export function rejectRequest(requestId: string, input: ReviewInput): Promise<UpdateRequest> {
  return request<UpdateRequest>(`/requests/${encodeURIComponent(requestId)}/reject`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `GET /api/docs/:doc_id/requests` — 某文档的请求历史 */
export function listDocRequests(docId: string): Promise<UpdateRequest[]> {
  return request<UpdateRequest[]>(`/docs/${encodeURIComponent(docId)}/requests`);
}

// ── 回收站 / 检索 ────────────────────────────────────────────────────

/** `GET /api/trash` */
export function listTrash(): Promise<TrashEntry[]> {
  return request<TrashEntry[]>("/trash");
}

/** `POST /api/trash/:trash_id/restore` — 恢复（重新生成 doc_id） */
export function restoreTrash(trashId: string): Promise<DocMeta> {
  return request<DocMeta>(`/trash/${encodeURIComponent(trashId)}/restore`, { method: "POST" });
}

/** `DELETE /api/trash/:trash_id` — 永久删除 */
export function purgeTrash(trashId: string): Promise<void> {
  return request<void>(`/trash/${encodeURIComponent(trashId)}`, { method: "DELETE" });
}

/** `GET /api/search?keyword=&limit=` */
export function searchDocs(keyword: string, limit = 20, signal?: AbortSignal): Promise<SearchHit[]> {
  const q = `?keyword=${encodeURIComponent(keyword)}&limit=${limit}`;
  return request<SearchHit[]>(`/search${q}`, { signal });
}

// ── 权限（ACL，设计 §9）───────────────────────────────────────────

/** 权限端点的节点类型段（`/permissions/{kind}/{id}`） */
export type PermissionKind = "doc" | "dir";

/** `GET /api/permissions/:kind/:id` — 显式/生效 ACL + 是否可管理 */
export function getPermissions(kind: PermissionKind, id: string): Promise<PermissionsView> {
  return request<PermissionsView>(`/permissions/${kind}/${encodeURIComponent(id)}`);
}

/**
 * `PUT /api/permissions/:kind/:id` — 整体替换显式 ACL（204）。
 *
 * `acl = null` 清除（恢复继承）；`acl = []` 私有（仅创建者/admin）。
 * 服务端门：仅节点创建者或 admin 可调用（其他用户 403 required=manage）。
 */
export function setPermissions(
  kind: PermissionKind,
  id: string,
  acl: Ace[] | null,
): Promise<void> {
  return request<void>(`/permissions/${kind}/${encodeURIComponent(id)}`, {
    method: "PUT",
    body: JSON.stringify({ acl }),
  });
}
