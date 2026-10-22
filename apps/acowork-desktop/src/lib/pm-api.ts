/**
 * PM REST API 客户端 — 对齐 acowork-pm 服务端（core/acowork-pm/src/api）。
 *
 * 路径前缀：`{gw}/api/pm/...`（Gateway 反代挂载，见 acowork-gateway/src/http/pm_api.rs）。
 * 错误格式：`{"error": {"code": "...", "message": "..."}}`（见 acowork-pm/src/error.rs）。
 * 人类操作需带 `X-Actor` 头（服务端 created_by / 审核者来源）。
 */

import { getGatewayUrl } from "./config";
import type {
  AddProjectMemberInput,
  CreateProjectInput,
  CreateTaskInput,
  PmAttachmentMeta,
  PmProject,
  PmTask,
  PmTaskResponse,
  ReparentInput,
  ReviewInput,
  UpdateProjectInput,
  UpdateTaskInput,
} from "./pm-types";

/** 当前人类操作者标识（服务端约定：human 或 agent:xxx） */
const HUMAN_ACTOR = "human";

export class PmApiError extends Error {
  readonly code: string;
  readonly status: number;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "PmApiError";
    this.code = code;
    this.status = status;
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

  const res = await fetch(`${getGatewayUrl()}/api/pm${path}`, {
    ...rest,
    headers,
  });

  if (!res.ok) {
    let code = "http_error";
    let message = `HTTP ${res.status}`;
    try {
      const body = (await res.json()) as {
        error?: { code?: string; message?: string };
      };
      code = body.error?.code ?? code;
      message = body.error?.message ?? message;
    } catch {
      // 非 JSON 响应体，保留默认
    }
    throw new PmApiError(res.status, code, message);
  }

  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

// ── Projects ──────────────────────────────────────────────────────────

/**
 * 规范化项目响应：旧服务端可能缺 `members` 字段（契约演进前），
 * 统一补 `[]`，避免 UI 运行时 undefined 崩溃。
 */
function normalizeProject(p: PmProject): PmProject {
  return { ...p, members: p.members ?? [] };
}

export function listProjects() {
  return request<PmProject[]>("/projects").then((list) => list.map(normalizeProject));
}

export function createProject(input: CreateProjectInput) {
  return request<PmProject>("/projects", {
    method: "POST",
    body: JSON.stringify(input),
  }).then(normalizeProject);
}

export function getProject(pid: string) {
  return request<PmProject>(`/projects/${encodeURIComponent(pid)}`).then(normalizeProject);
}

export function updateProject(pid: string, input: UpdateProjectInput) {
  return request<PmProject>(`/projects/${encodeURIComponent(pid)}`, {
    method: "PATCH",
    body: JSON.stringify(input),
  }).then(normalizeProject);
}

export function deleteProject(pid: string) {
  return request<void>(`/projects/${encodeURIComponent(pid)}`, {
    method: "DELETE",
  });
}

// ── Project Members ───────────────────────────────────────────────────

/** 添加项目成员（Agent 实例或人类账号，input.kind）。重复添加 → 409 member_already_exists */
export function addProjectMember(pid: string, input: AddProjectMemberInput) {
  return request<PmProject>(`/projects/${encodeURIComponent(pid)}/members`, {
    method: "POST",
    body: JSON.stringify(input),
  }).then(normalizeProject);
}

/** 移除项目成员。成员名下仍有未完成任务 → 409 member_has_open_tasks */
export function removeProjectMember(pid: string, instanceId: string) {
  return request<PmProject>(
    `/projects/${encodeURIComponent(pid)}/members/${encodeURIComponent(instanceId)}`,
    { method: "DELETE" },
  ).then(normalizeProject);
}

/**
 * 转让群主。`newOwner` 必须是当前成员里的人类账号（服务端 store 层校验）。
 * 只有当前群主或 admin 能发起（服务端读可信 `X-Actor` / `x-actor-admin`）：
 * 不符 → 403。成功后原群主降级为普通成员，群聊参与者随之收敛。
 */
export function transferProjectOwner(pid: string, newOwner: string) {
  return request<PmProject>(
    `/projects/${encodeURIComponent(pid)}/transfer-owner`,
    { method: "POST", body: JSON.stringify({ new_owner: newOwner }) },
  ).then(normalizeProject);
}

// ── Tasks ─────────────────────────────────────────────────────────────

/** 项目任务列表 — 返回 TaskResponse[]（含 parent_id/depth/is_blocked 派生字段） */
export function listProjectTasks(pid: string) {
  return request<PmTaskResponse[]>(`/projects/${encodeURIComponent(pid)}/tasks`);
}

export function createTask(pid: string, input: CreateTaskInput) {
  return request<PmTask>(`/projects/${encodeURIComponent(pid)}/tasks`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

export function getTask(tid: string) {
  return request<PmTaskResponse>(`/tasks/${encodeURIComponent(tid)}`);
}

export function updateTask(tid: string, input: UpdateTaskInput) {
  return request<PmTask>(`/tasks/${encodeURIComponent(tid)}`, {
    method: "PATCH",
    body: JSON.stringify(input),
  });
}

export function deleteTask(tid: string, opts?: { cascade?: boolean }) {
  const q = opts?.cascade ? "?cascade=true" : "";
  return request<void>(`/tasks/${encodeURIComponent(tid)}${q}`, {
    method: "DELETE",
  });
}

/** 移动任务到新父下（new_parent=null 提升为根任务） */
export function reparentTask(tid: string, newParent: string | null) {
  return request<void>(`/tasks/${encodeURIComponent(tid)}/parent`, {
    method: "PATCH",
    body: JSON.stringify({ new_parent: newParent } satisfies ReparentInput),
  });
}

/** 人类审核（submitted → done / rejected）；comment 为拒绝理由（可选） */
export function reviewTask(tid: string, approved: boolean, comment?: string) {
  const body: ReviewInput = { approved };
  if (comment) body._comment = comment;
  return request<PmTask>(`/tasks/${encodeURIComponent(tid)}/review`, {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** 追加备注（服务端 P3 预留，当前 404 则忽略） */
export function addNote(tid: string, text: string) {
  return request<void>(`/tasks/${encodeURIComponent(tid)}/notes`, {
    method: "POST",
    body: JSON.stringify({ text }),
  });
}

// ── Attachments ───────────────────────────────────────────────────────

export function listTaskAttachments(tid: string) {
  return request<PmAttachmentMeta[]>(`/tasks/${encodeURIComponent(tid)}/attachments`);
}

export function uploadAttachment(tid: string, file: File, actor?: string) {
  const form = new FormData();
  form.append("file", file);
  return request<PmAttachmentMeta>(
    `/tasks/${encodeURIComponent(tid)}/attachments`,
    { method: "POST", body: form, actor },
  );
}

export function deleteAttachment(aid: string) {
  return request<void>(`/attachments/${encodeURIComponent(aid)}`, {
    method: "DELETE",
  });
}

/** 附件下载/预览 URL（thumbnail 仅图片） */
export function attachmentUrl(aid: string, opts?: { download?: boolean; thumb?: boolean }) {
  const q = new URLSearchParams();
  if (opts?.download) q.set("download", "1");
  if (opts?.thumb) q.set("thumb", "1");
  const qs = q.toString();
  return `${getGatewayUrl()}/api/pm/attachments/${encodeURIComponent(aid)}${qs ? `?${qs}` : ""}`;
}

// ── Project Icons ───────────────────────────────────────────────────

/** 项目图标资产条目（上传返回、`icon-assets` 列表项）。 */
export interface PmIconAsset {
  relative_path: string;
}

/**
 * 上传项目图标（multipart，字段 `file`）→ 服务端存 `icon-NN.{ext}` 并返回
 * 相对路径。**不**自动选中——调用方拿到 `relative_path` 后合并进
 * `metadata.icon` 再回写（与头像上传一致）。
 */
export function uploadProjectIcon(pid: string, file: File) {
  const form = new FormData();
  form.append("file", file);
  return request<PmIconAsset>(`/projects/${encodeURIComponent(pid)}/icon`, {
    method: "POST",
    body: form,
  });
}

/** 列出某项目已上传的自定义图标（供选择器 Custom tab 网格）。 */
export function fetchProjectIconAssets(pid: string): Promise<PmIconAsset[]> {
  return request<{ assets: PmIconAsset[] }>(
    `/projects/${encodeURIComponent(pid)}/icon-assets`,
  ).then((r) => r.assets);
}

// ── Search（ADR-081 P0-2）────────────────────────────────────────────

/** `GET /api/pm/search?q=&limit=` — 项目/任务子串搜索，命中字段加权
 *  title > description > assignee（服务端 `api/search.rs`）。 */
export interface PmSearchHit {
  kind: "project" | "task";
  id: string;
  title: string;
  snippet: string;
  score: number;
  project_id: string;
  task_id?: string;
}

export function searchPm(q: string, limit = 20, signal?: AbortSignal): Promise<PmSearchHit[]> {
  const qs = new URLSearchParams({ q, limit: String(limit) });
  return request<PmSearchHit[]>(`/search?${qs.toString()}`, { signal });
}
