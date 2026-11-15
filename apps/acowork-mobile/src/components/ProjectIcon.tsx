/**
 * ProjectIcon — 项目图标渲染（custom / builtin / 默认兜底）。
 *
 * mobile 版，与 desktop 的 `components/common/ProjectAvatar.tsx` 同源，渲染优先级：
 * 1. custom（`metadata.icon.path`）—— 走 PM `icon-file` 端点，需 bearer → `AuthImage`
 * 2. builtin（`metadata.icon.id`）—— `BUILTIN_ICONS[id]` 本地渲染
 * 3. 默认项目图标资源（`DEFAULT_PROJECT_ICON`）—— 未设置 / 非法时兜底
 *
 * custom 字节加载失败（离线 / 401）时同样落默认图标（`AuthImage` fallback）。
 *
 * 样式是普通 CSS class（mobile 没有 Tailwind），故圆角 / 描边由 `.project-icon`
 * 承担，不接受 `className` 拼接。
 */

import { BUILTIN_ICONS } from '../lib/builtinIcons'
import {
  DEFAULT_PROJECT_ICON,
  parseProjectIcon,
  resolveProjectIconUrl,
} from '../lib/projectIcon'
import { AuthImage } from './AuthImage'
import type { PmProject } from '../lib/api'

export interface ProjectIconProps {
  project: PmProject
  /** Gateway 地址（custom 图标的字节 URL 需要它）。 */
  baseUrl: string
  /** 边长（px）。默认 24。 */
  size?: number
}

export function ProjectIcon({ project, baseUrl, size = 24 }: ProjectIconProps) {
  const icon = parseProjectIcon(project.metadata)
  const style = { width: size, height: size }

  if (icon?.kind === 'custom') {
    return (
      <AuthImage
        src={resolveProjectIconUrl(baseUrl, project.id, icon.path)}
        alt=""
        className="project-icon"
        style={style}
        fallback={<DefaultIcon size={size} />}
      />
    )
  }

  if (icon?.kind === 'builtin' && BUILTIN_ICONS[icon.id]) {
    return (
      <img
        src={BUILTIN_ICONS[icon.id]}
        alt=""
        draggable={false}
        className="project-icon"
        style={style}
      />
    )
  }

  return <DefaultIcon size={size} />
}

function DefaultIcon({ size }: { size: number }) {
  return (
    <img
      src={DEFAULT_PROJECT_ICON}
      alt=""
      draggable={false}
      className="project-icon"
      style={{ width: size, height: size }}
    />
  )
}
