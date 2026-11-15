/**
 * TableBubbleMenu — 表格浮动工具条（差距分析 35 §4.1，P0）。
 *
 * @tiptap/extension-table v3 的行列增删命令内核已齐（RichToolbar 只暴露了
 * insertTable），此处只补 UI：光标进入表格 / 单元格选区（CellSelection）时
 * 浮出，提供：上行加行 / 下行加行 / 左列加列 / 右列加列 / 删行 / 删列 /
 * 表头行/列切换 / 删除表格。
 *
 * 约束：
 * - 所有操作走 editor.chain()（进 undo/redo 栈），组件不持有文档状态
 *   （对齐 RichToolbar 现状）。
 * - 不暴露 mergeCells / splitCell（GFM 无法表达，round-trip 会漂移，YAGNI）。
 * - readOnly（editor 不可编辑）时不显示。
 */

import { CellSelection } from "@tiptap/pm/tables";
import type { Editor } from "@tiptap/react";
import { BubbleMenu } from "@tiptap/react/menus";
import {
  ArrowDown,
  ArrowLeft,
  ArrowRight,
  ArrowUp,
  PanelLeft,
  PanelTop,
  TableColumnsSplit,
  TableRowsSplit,
  Trash2,
  type LucideIcon,
} from "lucide-react";
import { useTranslation } from "../../../i18n/useTranslation";
import { cn } from "../../../lib/utils";

interface TableBubbleMenuProps {
  editor: Editor | null;
  /** 只读时整个菜单不渲染（readOnly prop 与渲染同拍，绕开 setEditable effect 时序）。 */
  readOnly: boolean;
}

function MenuButton({
  icon: Icon,
  label,
  onClick,
}: {
  icon: LucideIcon;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      onClick={onClick}
      className={cn(
        "flex h-6 w-6 shrink-0 items-center justify-center rounded text-text-tertiary transition-colors",
        "hover:bg-[var(--color-accent)]/10 hover:text-zinc-800 dark:hover:bg-[var(--color-accent)]/15 dark:hover:text-zinc-100",
      )}
    >
      <Icon className="h-3 w-3" aria-hidden />
    </button>
  );
}

export function TableBubbleMenu({ editor, readOnly }: TableBubbleMenuProps) {
  const { t } = useTranslation();
  // readOnly 时不渲染：React 卸载 BubbleMenu（unregisterPlugin + 移除元素），
  // 避免只读态残留浮动条。shouldShow 里再查 isEditable 兜底外部 setEditable。
  if (!editor || readOnly) return null;

  const chain = () => editor.chain().focus();

  return (
    <BubbleMenu
      editor={editor}
      updateDelay={100}
      data-testid="table-bubble-menu"
      shouldShow={({ editor: ed, state }) => {
        if (!ed.isEditable) return false;
        return ed.isActive("table") || state.selection instanceof CellSelection;
      }}
      className={cn(
        "flex items-center gap-0.5 rounded-md border border-border-outer bg-white/95 p-1 shadow-md backdrop-blur",
        "dark:bg-zinc-900/95",
      )}
    >
      <MenuButton icon={ArrowUp} label={t("doc.tbAddRowBefore")} onClick={() => chain().addRowBefore().run()} />
      <MenuButton icon={ArrowDown} label={t("doc.tbAddRowAfter")} onClick={() => chain().addRowAfter().run()} />
      <MenuButton icon={ArrowLeft} label={t("doc.tbAddColumnBefore")} onClick={() => chain().addColumnBefore().run()} />
      <MenuButton icon={ArrowRight} label={t("doc.tbAddColumnAfter")} onClick={() => chain().addColumnAfter().run()} />
      <span className="mx-0.5 h-4 w-px bg-zinc-200 dark:bg-zinc-700" aria-hidden />
      <MenuButton icon={TableRowsSplit} label={t("doc.tbDeleteRow")} onClick={() => chain().deleteRow().run()} />
      <MenuButton icon={TableColumnsSplit} label={t("doc.tbDeleteColumn")} onClick={() => chain().deleteColumn().run()} />
      <span className="mx-0.5 h-4 w-px bg-zinc-200 dark:bg-zinc-700" aria-hidden />
      <MenuButton icon={PanelTop} label={t("doc.tbToggleHeaderRow")} onClick={() => chain().toggleHeaderRow().run()} />
      <MenuButton icon={PanelLeft} label={t("doc.tbToggleHeaderColumn")} onClick={() => chain().toggleHeaderColumn().run()} />
      <span className="mx-0.5 h-4 w-px bg-zinc-200 dark:bg-zinc-700" aria-hidden />
      <MenuButton icon={Trash2} label={t("doc.tbDeleteTable")} onClick={() => chain().deleteTable().run()} />
    </BubbleMenu>
  );
}
