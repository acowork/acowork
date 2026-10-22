/**
 * ProjectAvatar — 项目图标渲染（custom / builtin / 默认兜底）。
 *
 * 与 `MemberAvatar`/`AgentAvatar` 同一圆角/描边语法，渲染优先级：
 * 1. custom（`metadata.icon.path`）—— 走 PM `icon-file` 端点，需 bearer → `AuthedImage`
 * 2. builtin（`metadata.icon.id`）—— `BUILTIN_ICONS[id]` 本地渲染
 * 3. 默认项目图标资源（`DEFAULT_PROJECT_ICON`）—— 未设置 / 非法时兜底
 *
 * custom 字节加载失败（离线 / 401）时同样落默认图标（`AuthedImage` fallback）。
 */

import { BUILTIN_ICONS } from "../../lib/builtinIcons";
import {
  DEFAULT_PROJECT_ICON,
  parseProjectIcon,
  resolveProjectIconUrl,
} from "../../lib/projectIcon";
import { AuthedImage } from "./AuthedImage";
import type { PmProject } from "../../lib/pm-types";

export interface ProjectAvatarProps {
  project: PmProject;
  /** 边长（px）。默认 20（列表行 / 头部小尺寸）。 */
  size?: number;
  /** 追加样式类（合并进根元素的 ring/圆角基础类之外）。 */
  className?: string;
}

const BASE_RING =
  "rounded-md object-cover ring-1 ring-zinc-300/60 dark:ring-zinc-600/60";

export function ProjectAvatar({ project, size = 20, className }: ProjectAvatarProps) {
  const icon = parseProjectIcon(project.metadata);
  const cls = className ? `${BASE_RING} ${className}` : BASE_RING;
  const style = { width: size, height: size };

  if (icon?.kind === "custom") {
    return (
      <AuthedImage
        src={resolveProjectIconUrl(project.id, icon.path)}
        alt={project.title}
        className={cls}
        style={style}
        fallback={<DefaultIcon size={size} className={className} />}
      />
    );
  }

  if (icon?.kind === "builtin" && BUILTIN_ICONS[icon.id]) {
    return (
      <img
        src={BUILTIN_ICONS[icon.id]}
        alt={project.title}
        draggable={false}
        className={cls}
        style={style}
      />
    );
  }

  return <DefaultIcon size={size} className={className} />;
}

function DefaultIcon({ size, className }: { size: number; className?: string }) {
  return (
    <img
      src={DEFAULT_PROJECT_ICON}
      alt=""
      draggable={false}
      className={className ? `${BASE_RING} ${className}` : BASE_RING}
      style={{ width: size, height: size }}
    />
  );
}
