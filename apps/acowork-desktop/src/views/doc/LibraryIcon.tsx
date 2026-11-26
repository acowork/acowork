/**
 * LibraryIcon — 文档库图标渲染（列表行 / 卡片 / 对话框 / 选择器共用一处）。
 *
 * 值域来自 `LIBRARY_ICONS`（文档语义的线条字形），**不是** `BUILTIN_ICONS`
 * —— 后者是 bot/用户头像集的立绘，摆在文档库上会读成「这里住着一个机器人」。
 * 字形用 `currentColor`，行/卡片的选中态把文字刷白时图标跟着白，不需要第
 * 二套样式。
 *
 * 缺省 / 未知 key 回落为导航栏的 docs 产品图标（`LIBRARY_DEFAULT_ICON_SRC`）：
 * 没有自己图标的库，看上去就该是「文档」这件事本身，而不是一个泛用字形。
 */

import { LIBRARY_DEFAULT_ICON_SRC, LIBRARY_ICONS } from "../../lib/libraryIcons";
import { cn } from "../../lib/utils";

export function LibraryIcon({
  icon,
  size = 18,
  className,
}: {
  icon?: string | null;
  size?: number;
  className?: string;
}) {
  const Glyph = icon ? LIBRARY_ICONS[icon] : undefined;
  if (Glyph) {
    return (
      <Glyph
        aria-hidden
        style={{ width: size, height: size }}
        className={cn("shrink-0", className)}
      />
    );
  }
  return (
    <img
      src={LIBRARY_DEFAULT_ICON_SRC}
      alt=""
      draggable={false}
      width={size}
      height={size}
      style={{ width: size, height: size }}
      className={cn("shrink-0 object-contain", className)}
    />
  );
}