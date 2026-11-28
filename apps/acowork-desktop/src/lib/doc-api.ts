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
  CreateLibraryInput,
  DirMeta,
  DocMeta,
  DocRead,
  DocResolveResult,
  DocTreeNode,
  MoveDocInput,
  PermissionsView,
  RenameDirInput,
  ReviewInput,
  SearchHit,
  TrashEntry,
  UpdateDocInput,
  UpdateLibraryInput,
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
  if (rest.body && !(rest.body instanceof FormData) && !headers["Content-Type"]) {
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

// ── 文档库（根目录的直接子节点）──────────────────────────────────────

/** `GET /api/doc/libraries` — 当前用户可见的文档库列表 */
export function listLibraries(): Promise<DirMeta[]> {
  return request<{ libraries: DirMeta[] }>("/libraries").then((r) => r.libraries);
}

/** `POST /api/doc/libraries` — 新建文档库（图标/描述/成员/群主一次收齐） */
export function createLibrary(input: CreateLibraryInput): Promise<DirMeta> {
  return request<DirMeta>("/libraries", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `PATCH /api/doc/libraries/:dir_id` — 改文档库元数据（缺省字段不动，null 清空） */
export function updateLibrary(
  dirId: string,
  input: UpdateLibraryInput,
): Promise<DirMeta> {
  return request<DirMeta>(`/libraries/${encodeURIComponent(dirId)}`, {
    method: "PATCH",
    body: JSON.stringify(input),
  });
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

/**
 * `POST /refs/resolve` — 库内地址（`doc-…` 或 `研发/纪要.pdf`）→ 条目。
 *
 * 与 `/refs/validate` 的区别是**没有 Markdown 闸门**：链接可以指向库里任何条目
 * （pdf / docx / xlsx / pptx / 图片），校验只回答「PM 的交付物引用是否落地」。
 * 两条路由共用服务端的同一个 `resolve_entry`，所以地址语法不会漂移。
 *
 * 结果与输入一一对应、顺序不变 —— 调用方按位置对齐自己的输入。
 */
export function resolveDocEntries(refs: string[]): Promise<DocResolveResult[]> {
  return request<{ results: DocResolveResult[]; all_found: boolean }>("/refs/resolve", {
    method: "POST",
    body: JSON.stringify({ refs }),
  }).then((r) => r.results);
}

/**
 * 代表「这一份字节来自文档库」的来源标记。
 *
 * 字节缓存与 viewer 统一按工作区坐标 `(agentId, workspaceId, relPath)` 取字节
 * （同一批 viewer、同一份 blob LRU，见 `lib/docFileBlob.ts`），而文档条目没有
 * 工作区 —— 这个哨兵就是那个「没有工作区」的坐标值，和 `__agent_home__` 代表
 * 「没有 workspace 的 home」同构。它住在 doc 的地址层（这里），因为谁要知道
 * 「这份字节该走哪个路由」，答案就必须和路由常量在同一个模块里。
 */
export const DOC_FILE_AGENT_ID = "__doc__";

/**
 * 文档库条目的**字节**地址（`GET /api/doc/docs/{id}/file`）。
 *
 * 这是 `/api/doc` 字面量在桌面上的唯一构造处之一（R1）。它之所以必须住在这里
 * 而不是在字节缓存里：字节缓存（`lib/workspaceFileBlob.ts`）服务的是**工作区
 * 文件**，文档库只是它的一个外部来源；让缓存自己拼 doc 路由，等于把 doc 域的
 * URL 知识散到中间件之外，改路由时就得靠 grep 找齐。
 *
 * 它返回的是**可以 fetch 的字符串**（`fetch` 拦截器会补 bearer），不是可以塞进
 * `<img src>` / `<iframe src>` 的地址 —— 标签带不了 `Authorization` 头，那是
 * 「白屏 401」那个 bug 的形状。渲染面拿 blob（或 `blob:` URL），不拿这个字符串。
 */
export function getDocFileUrl(docId: string): string {
  // 与 `workspaceFileBlob` 的构造器同一条去尾斜杠规则：`getGatewayUrl()` 的
  // 返回值在测试 / 反向代理配置里可能带尾斜杠，拼出 `//api/doc` 会被
  // Axum 的路由当成不同的路径。
  const base = getGatewayUrl().replace(/\/+$/g, "");
  return `${base}/api/doc/docs/${encodeURIComponent(docId)}/file`;
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

/**
 * `POST /api/docs/import?dir_id=` — 上传一个文件到指定目录（multipart）。
 *
 * 文本类（md / markdown / txt）服务端落成普通 Markdown 文档；其余格式
 * （pdf / docx / xlsx / pptx / 图片…）落成只读文件条目，字节由
 * `GET /api/doc/docs/{id}/file` 提供——预览走工作区同一套预览组件。
 *
 * 不走 `request()` 的 JSON 分支：FormData 时不能设 Content-Type（boundary
 * 必须由浏览器生成），`request()` 已经跳过这一步。
 */
export function importDoc(dirId: string, file: File): Promise<DocMeta> {
  const form = new FormData();
  // 第三个参数显式给文件名：从工作区读回来的 Blob 常常没有 name。
  form.append("file", file, file.name);
  return request<DocMeta>(`/docs/import?dir_id=${encodeURIComponent(dirId)}`, {
    method: "POST",
    body: form,
  });
}

/**
 * `PUT /api/docs/:doc_id/file` — 用新字节覆盖一个导入文件条目。
 *
 * app 预览（wordcraft / deckcraft …）的保存回写给字节，不给 JSON：条目已经有
 * 名字和扩展名，没有要解析的东西，原始 body 就是文件本身。
 */
export function saveDocFile(docId: string, bytes: Uint8Array): Promise<DocMeta> {
  return request<DocMeta>(`/docs/${encodeURIComponent(docId)}/file`, {
    method: "PUT",
    headers: { "Content-Type": "application/octet-stream" },
    body: new Uint8Array(bytes),
  });
}

/**
 * 上传对话框 / `<input type=file>` 共用的 accept 串。
 *
 * 与服务端 `core/acowork-doc/src/types.rs::IMPORTABLE_EXTS` 同一张表：
 * 这里多列几个常见图片别名没关系（服务端仍会拒收），少列一个就是用户
 * 选不到支持的格式。
 */
export const DOC_IMPORT_ACCEPT =
  ".pdf,.doc,.docx,.xls,.xlsx,.ppt,.pptx,.csv,.txt,.md,.markdown," +
  ".png,.jpg,.jpeg,.gif,.webp,.svg,.bmp," +
  "application/pdf,image/*,text/*," +
  "application/vnd.openxmlformats-officedocument.wordprocessingml.document," +
  "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet," +
  "application/vnd.openxmlformats-officedocument.presentationml.presentation";

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
