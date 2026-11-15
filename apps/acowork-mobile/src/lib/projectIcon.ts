/**
 * 项目图标（project icon）引用解析与 URL 构造 — mobile 版。
 *
 * 与 desktop 的 `lib/projectIcon.ts` 同源：图标引用存进 `PmProject.metadata.icon`，
 * shape 见 [`ProjectIconRef`]：
 * - `{ kind: "builtin", id: "icon-07" }` —— 本地用 `BUILTIN_ICONS[id]` 渲染
 * - `{ kind: "custom", path: "projects/{pid}/icon/icon-01.png" }` —— 经
 *   `GET /api/pm/projects/{pid}/icon-file` 服务端渲染（[`resolveProjectIconUrl`]）
 * - 缺失 / 非法 → `null`，UI 兜底为默认项目图标资源
 *
 * 与头像（`components/AuthImage.tsx`）同一范式，只是作用域是项目、且 custom
 * 走 PM 端点。物理落盘与命名（`icon-NN.{ext}`）在服务端
 * （`core/acowork-pm/src/api/icon.rs`）。
 */

import { BUILTIN_ICONS } from './builtinIcons'
import projectDefaultIconUrl from '../assets/project-default-icon.png'

/** 默认项目图标（Vite `?url` 解析为哈希化静态资源 URL），未设置图标时兜底。 */
export const DEFAULT_PROJECT_ICON = projectDefaultIconUrl

/** 项目图标引用（存进 `metadata.icon`）。 */
export type ProjectIconRef =
  | { kind: 'builtin'; id: string }
  | { kind: 'custom'; path: string }

/**
 * 构造服务端渲染 custom 图标的字节 URL。
 *
 * 走 Gateway 透明反代 `/api/pm/*`（`pm_proxy.rs`），图片字节需 bearer，
 * 故由 `AuthImage` 经 `fetch` 加载（拦截器附带 token）。
 */
export function resolveProjectIconUrl(baseUrl: string, pid: string, relPath: string): string {
  return `${baseUrl}/api/pm/projects/${encodeURIComponent(pid)}/icon-file?path=${encodeURIComponent(relPath)}`
}

/**
 * 从 `metadata.icon` 解析出图标引用，含向后兼容：
 * 非法 / 缺失 / builtin id 不在内置集 → `null`（调用方兜底默认图标）。
 */
export function parseProjectIcon(
  metadata: Record<string, unknown> | null | undefined,
): ProjectIconRef | null {
  const raw = metadata?.icon
  if (!raw || typeof raw !== 'object') return null
  const icon = raw as Record<string, unknown>
  if (icon.kind === 'builtin' && typeof icon.id === 'string' && BUILTIN_ICONS[icon.id]) {
    return { kind: 'builtin', id: icon.id }
  }
  if (icon.kind === 'custom' && typeof icon.path === 'string' && icon.path) {
    return { kind: 'custom', path: icon.path }
  }
  return null
}
