/**
 * 列宽拖拽手柄（表头右沿）。
 *
 * 起始宽度取 `getBoundingClientRect().width`，**不是** `col.style.width`：
 * `title` 列在 fixed 布局下刻意不写宽度（由它吸收剩余空间），读 style 会
 * 拿到空串，第一次拖动就跳到 0。原型里也是这么取的。
 *
 * 宽度只写内存态（决策 4）。持久化落点是 `metadata.pmListWidths`，届时
 * 写入必须走 `pm-view-settings` 的合并语义，别把兄弟字段清空。
 *
 * 指针生命周期与 `useDragResize` 同形（startX/startWidth ref + document 级
 * mousemove/mouseup + 拖动期禁选），但那个 hook 绑死了 localStorage 持久化
 * 与「左栏宽度」语义，这里要的是内存态 + 任意列，所以不复用它。
 */

import { useCallback, useEffect, useRef, useState } from "react";

const MIN_COL_WIDTH = 60;
const MAX_COL_WIDTH = 640;

interface PmColumnResizerProps {
  /** 落在 DOM 上便于测试与调试；手柄自身不需要知道自己属于哪一列。 */
  columnKey: string;
  onWidth: (px: number) => void;
}

export function PmColumnResizer({ columnKey, onWidth }: PmColumnResizerProps) {
  const originRef = useRef<{ startX: number; startWidth: number } | null>(null);
  const [dragging, setDragging] = useState(false);

  const onMouseDown = useCallback((e: React.MouseEvent<HTMLElement>) => {
    // 表头单元格的点击 = 排序，拖动必须先掐掉冒泡。
    e.stopPropagation();
    e.preventDefault();
    const th = e.currentTarget.closest("th");
    originRef.current = {
      startX: e.clientX,
      startWidth: th?.getBoundingClientRect().width ?? MIN_COL_WIDTH,
    };
    setDragging(true);
  }, []);

  useEffect(() => {
    if (!dragging) return;
    const onMove = (e: MouseEvent) => {
      const origin = originRef.current;
      if (!origin) return;
      const next = Math.min(
        Math.max(origin.startWidth + (e.clientX - origin.startX), MIN_COL_WIDTH),
        MAX_COL_WIDTH,
      );
      onWidth(Math.round(next));
    };
    const stop = () => setDragging(false);
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", stop);
    const prevSelect = document.body.style.userSelect;
    document.body.style.userSelect = "none";
    return () => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", stop);
      document.body.style.userSelect = prevSelect;
    };
  }, [dragging, onWidth]);

  return (
    <span
      data-resize-column={columnKey}
      role="separator"
      aria-orientation="vertical"
      onMouseDown={onMouseDown}
      className="absolute right-0 top-0 z-[3] h-full w-1.5 cursor-col-resize hover:bg-[var(--color-accent)]/40"
    />
  );
}
