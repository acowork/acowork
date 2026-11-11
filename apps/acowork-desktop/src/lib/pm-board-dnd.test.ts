/**
 * 看板命中判定 — 指针落在卡片上才算「变子任务」。
 *
 * 钉住两件事（用假矩形，不依赖 DOM）：
 *   1. 指针在卡片矩形内 → 卡片优先（嵌套在列里也取卡片）；
 *   2. 指针在卡片之间 / 列底空白（仍在列内）→ 列，不是某张卡片；
 *   3. 键盘拖拽没有指针坐标 → 退回 closestCorners，仍能选出目标。
 */
import { describe, expect, it } from "vitest";
import type { ClientRect, CollisionDetection } from "@dnd-kit/core";
import { boardCollisionDetection } from "./pm-board-dnd";

const rect = (top: number, height: number): ClientRect =>
  ({ top, bottom: top + height, left: 0, right: 300, width: 300, height }) as ClientRect;

const COLUMN = rect(0, 600);
const CARD_1 = rect(10, 60);
const CARD_2 = rect(90, 60);

const droppableRects = new Map<string, ClientRect>([
  ["column-pending", COLUMN],
  ["drop-t1", CARD_1],
  ["drop-t2", CARD_2],
]);
const droppableContainers = [...droppableRects.keys()].map((id) => ({ id })) as never;

function firstHit(x: number, y: number): string | undefined {
  const args = {
    active: { id: "t1" },
    collisionRect: rect(y, 60),
    droppableRects,
    droppableContainers,
    pointerCoordinates: { x, y },
  } as unknown as Parameters<CollisionDetection>[0];
  return boardCollisionDetection(args)[0]?.id as string | undefined;
}

describe("boardCollisionDetection", () => {
  it("命中卡片矩形 → 卡片（嵌套的列不会抢走）", () => {
    expect(firstHit(100, 40)).toBe("drop-t1");
    expect(firstHit(100, 120)).toBe("drop-t2");
  });

  it("命中卡片间隙 / 列底空白 → 列（保持状态流转，不 reparent）", () => {
    expect(firstHit(100, 80)).toBe("column-pending");
    expect(firstHit(100, 400)).toBe("column-pending");
  });

  it("指针落在所有列之外 → 无命中", () => {
    expect(firstHit(500, 40)).toBeUndefined();
  });

  it("键盘拖拽（无指针坐标）退回 closestCorners", () => {
    const args = {
      active: { id: "t1" },
      collisionRect: rect(12, 60),
      droppableRects,
      droppableContainers,
      pointerCoordinates: null,
    } as unknown as Parameters<CollisionDetection>[0];
    expect(boardCollisionDetection(args)[0]?.id).toBe("drop-t1");
  });
});