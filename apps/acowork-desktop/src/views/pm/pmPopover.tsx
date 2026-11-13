/**
 * pmPopover — PM 视图内部的锚定弹层薄封装。
 *
 * 为什么再来一个：列表视图有 6 个锚定弹层（列头 `⋮` 菜单、状态菜单、筛选
 * 多选、分组、字段配置、批量操作），每处都照 `WorkspaceSelector` /
 * `ContextUsageIcon` 那样手写一遍「portal 到 body + 测量 + 翻转 + 外点关闭」
 * 就是第六份拷贝。`ContextMenu` 子系统面向右键指针坐标，`Dropdown` 是单选
 * 值控件，都不适合承载「多选筛选 + 搜索 + 自定义行」的弹层内容。弹层里的
 * 行主题统一复用 `components/common/dropdownTheme.ts`（accent 洗色 + 勾）。
 *
 * 刻意**留在 `views/pm/` 里，不上提到 `components/common`**：它是 PM 的
 * 局部薄封装，公共弹层原语该由谁统一是另一件事，不该由这次改动顺带决定。
 *
 * 定位复用 `lib/dropdownPosition.computeDropdownPlacement`（与 chat 工具栏、
 * workspace 选择器同一套测量规则），面板外观复用 globals.css 的
 * `.context-menu` 语法（modal-surface + border-outer + 阴影），所以它和
 * 应用里其它菜单长得一样。
 *
 * 关闭语义：外点（`mousedown`，捕获前）、`Escape`。面板 portal 到 body，
 * 所以它**不是**触发器的后代 —— 外点判定必须同时看 anchor 和 panel 两个
 * ref，否则面板内第一次 mousedown 就会把自己关掉（`WorkspaceSelector` 注释
 * 里踩过的那个坑）。
 */

import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { Check } from "lucide-react";
import {
  computeDropdownPlacement,
  DROPDOWN_EDGE_GAP,
} from "../../lib/dropdownPosition";
import { cn } from "../../lib/utils";
import {
  DROPDOWN_CHECK_CLASS,
  dropdownItemClass,
} from "../../components/common/dropdownTheme";

interface PmPopoverProps {
  /** 触发元素（面板贴在它下沿，放不下就贴上沿）。 */
  anchor: HTMLElement;
  onClose: () => void;
  children: ReactNode;
  /** 面板宽度（px）。内容自适应时可以不传。 */
  width?: number;
  /** 水平贴边：`left` = 从 anchor 左沿向右长，`right` = 贴 anchor 右沿向左长。 */
  align?: "left" | "right";
  className?: string;
}

export function PmPopover({
  anchor,
  onClose,
  children,
  width,
  align = "left",
  className,
}: PmPopoverProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);

  const measure = useCallback(() => {
    const panelEl = panelRef.current;
    if (!panelEl) return;
    const r = anchor.getBoundingClientRect();
    const direction =
      window.innerHeight - r.bottom >= panelEl.offsetHeight + DROPDOWN_EDGE_GAP ||
      r.top < panelEl.offsetHeight + DROPDOWN_EDGE_GAP
        ? "down"
        : "up";
    const placement = computeDropdownPlacement({
      anchor: { left: r.left, right: r.right, top: r.top, bottom: r.bottom },
      menuWidth: panelEl.offsetWidth,
      menuHeight: panelEl.offsetHeight,
      viewportWidth: window.innerWidth,
      viewportHeight: window.innerHeight,
      direction,
      align,
    });
    setPos({ top: placement.top, left: placement.left });
  }, [anchor, align]);

  // 首帧前测量：面板不能先在 (0,0) 露一眼再跳位。
  useLayoutEffect(() => {
    measure();
  }, [measure]);

  // 面板自身尺寸变化（筛选列表异步填充）→ 重测。
  useLayoutEffect(() => {
    if (typeof ResizeObserver === "undefined") return;
    const el = panelRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => measure());
    ro.observe(el);
    return () => ro.disconnect();
  }, [measure]);

  useEffect(() => {
    const onResize = () => measure();
    window.addEventListener("resize", onResize);
    // 捕获阶段：容器自己的滚动也要跟住触发器。
    window.addEventListener("scroll", onResize, true);
    return () => {
      window.removeEventListener("resize", onResize);
      window.removeEventListener("scroll", onResize, true);
    };
  }, [measure]);

  useEffect(() => {
    const onPointerDown = (e: MouseEvent) => {
      const target = e.target as Node;
      if (panelRef.current?.contains(target)) return;
      if (anchor.contains(target)) return;
      onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("mousedown", onPointerDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onPointerDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [anchor, onClose]);

  if (typeof document === "undefined") return null;

  return createPortal(
    <div
      ref={panelRef}
      className={`context-menu${className ? ` ${className}` : ""}`}
      style={{
        top: pos?.top ?? -9999,
        left: pos?.left ?? -9999,
        width,
        // 测量完成前不可见：避免 (0,0) 闪一帧。
        visibility: pos ? "visible" : "hidden",
      }}
    >
      {children}
    </div>,
    document.body,
  );
}

/** 弹层里的分组小标题（「已显示 / 可添加」、列名等）。 */
export function PmPopoverSection({
  children,
  count,
}: {
  children: ReactNode;
  count?: number;
}) {
  return (
    <div className="flex items-center gap-2 px-3 pb-1 pt-2 text-10 uppercase tracking-wide text-text-tertiary">
      <span className="min-w-0 truncate">{children}</span>
      <span className="h-px flex-1 bg-border-divider" />
      {typeof count === "number" && <span className="tabular-nums">{count}</span>}
    </div>
  );
}

/** 弹层里的一个可点行。`checked` 画对勾，`disabled` 只禁点击不隐藏。 */
export function PmPopoverItem({
  children,
  onClick,
  checked,
  disabled,
  title,
  prefix,
}: {
  children: ReactNode;
  onClick?: () => void;
  checked?: boolean;
  disabled?: boolean;
  title?: string;
  /** 对勾与文字之间的视觉标记（头像 / 彩色点）。文字始终可截断。 */
  prefix?: ReactNode;
}) {
  return (
    <button
      type="button"
      // `disabled` 会吞掉 hover 反馈；这里用 aria-disabled + 短路 onClick，
      // 让「任务列不可隐藏」这类禁用项仍然看得见原因（配 title）。
      aria-disabled={disabled || undefined}
      title={title}
      onClick={disabled ? undefined : onClick}
      // The row theme, not `.context-menu-item`: a checked row now wears the
      // same accent wash + accent check as every other dropdown in the app.
      className={cn(
        dropdownItemClass(!!checked),
        "justify-start px-3 text-text",
        // Keep the old `.context-menu-item[aria-disabled]:hover` behaviour:
        // a disabled row must not light up under the cursor.
        disabled && "hover:bg-transparent dark:hover:bg-transparent",
      )}
      style={disabled ? { cursor: "not-allowed", opacity: 0.5 } : undefined}
    >
      <span className="flex w-3.5 shrink-0 justify-center" aria-hidden>
        {checked && <Check className={DROPDOWN_CHECK_CLASS} />}
      </span>
      {prefix}
      <span className="min-w-0 flex-1 truncate">{children}</span>
    </button>
  );
}
