/**
 * 看板命中判定 — 指针优先。
 *
 * 卡片 droppable 嵌在列 droppable 里面（「拖到卡片上 = 变子任务」）。原来用
 * `closestCorners`：拿被拖卡片的四角去和 droppable 的角比距离，而列是个
 * 巨大的矩形、卡片角永远更近 —— 结果只要指针在列内任何位置（卡片间的 6px
 * 间隙、列底空白）最近的都是某张卡片，于是「拖到列空白只改状态」永远走不到，
 * 每次都被 reparent 成子任务。
 *
 * `pointerWithin` 换成「指针是否真的落在 droppable 矩形内」：落在卡片上取
 * 卡片（嵌套时角距更小者胜），落在间隙取列。
 *
 * 键盘拖拽（Space 拾起 + ←/→）没有指针坐标，`pointerWithin` 返回空，退回
 * `closestCorners` 保留原有的按几何距离选列行为。
 */

import { closestCorners, pointerWithin, type CollisionDetection } from "@dnd-kit/core";

export const boardCollisionDetection: CollisionDetection = (args) =>
  args.pointerCoordinates ? pointerWithin(args) : closestCorners(args);