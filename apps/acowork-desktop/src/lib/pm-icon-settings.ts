/**
 * `metadata.icon` 的**写入口**（与 `pm-board-settings.ts` /
 * `pm-view-settings.ts` 同族：纯函数、零运行时依赖）。
 *
 * 为什么单独一个模块，而不是放在 `lib/projectIcon.ts` 里：那边是**读侧**
 * （解析 ref、拼服务端 URL、默认图标 artwork），因此 import 了
 * `lib/config`（→ settingsStore）和一个 `.png`。图标回写的调用方是
 * `stores/pm/projectStore.ts`（数据层）与设置面板，数据层不该因为一个
 * 三行展开的 helper 把网关配置和静态资源拖进自己的模块图里 —— 那会让
 * store 的单元测试被迫 mock 一整条它并不关心的链。
 *
 * 这里只 `import type`：类型在编译期擦除，运行时没有边。
 */

import type { ProjectIconRef } from "./projectIcon";

/**
 * 生成回写用 metadata：先合并旧字段再覆盖 `icon`。
 *
 * 后端 PATCH 对 `metadata` 是**整体替换**，直接发 `{ icon }` 会把
 * `boardPageSize` 等兄弟字段清空。`ref === null` 表示清除（回落默认
 * 图标）。图标写入**必须**走这里，不要在调用方手抄展开。
 */
export function withProjectIcon(
  metadata: Record<string, unknown> | null | undefined,
  ref: ProjectIconRef | null,
): Record<string, unknown> {
  return { ...(metadata ?? {}), icon: ref };
}
