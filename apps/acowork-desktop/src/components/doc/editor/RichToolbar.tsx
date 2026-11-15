/**
 * RichToolbar — Tiptap 富文本工具栏（DocEditor rich engine）。
 *
 * 与 MarkdownToolbar（Monaco）对应；所有操作走 editor.chain()，进 Tiptap
 * undo/redo 栈。组件不持有文档状态 —— editor 实例即唯一事实源，通过订阅
 * `transaction` / `selectionUpdate` 触发重渲染以刷新 active 态。
 */

import { useEffect, useState } from "react";
import type { Editor } from "@tiptap/react";
import {
  Bold,
  Code,
  Heading1,
  Heading2,
  Heading3,
  Image,
  Italic,
  Link,
  List,
  ListOrdered,
  ListTodo,
  Minus,
  Quote,
  Redo2,
  SquareCode,
  Strikethrough,
  Table,
  Undo2,
  Workflow,
  type LucideIcon,
} from "lucide-react";
import { useTranslation } from "../../../i18n/useTranslation";
import { cn } from "../../../lib/utils";

interface RichToolbarProps {
  /** Tiptap editor 实例（null 直到挂载完成）。 */
  editor: Editor | null;
  /** 禁用所有操作（doc 服务离线 / 只读）。 */
  disabled?: boolean;
}

type ToolAction = () => void;

function ToolButton({
  icon: Icon,
  label,
  active,
  disabled,
  onClick,
}: {
  icon: LucideIcon;
  label: string;
  active?: boolean;
  disabled?: boolean;
  onClick: ToolAction;
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      aria-pressed={active}
      disabled={disabled}
      onClick={onClick}
      className={cn(
        "flex h-7 w-7 shrink-0 items-center justify-center rounded transition-colors",
        active
          ? "bg-[var(--color-accent)]/10 text-[var(--color-accent)]"
          : "text-text-tertiary hover:bg-[var(--color-accent)]/10 hover:text-zinc-800  dark:hover:bg-[var(--color-accent)]/15 dark:hover:text-zinc-100",
        disabled && "pointer-events-none opacity-40",
      )}
    >
      <Icon className="h-3.5 w-3.5" aria-hidden />
    </button>
  );
}

export function RichToolbar({ editor, disabled }: RichToolbarProps) {
  const { t } = useTranslation();
  // 订阅 editor 事务/选区变化以刷新 active 态。
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!editor) return;
    const bump = () => setTick((n) => n + 1);
    editor.on("transaction", bump);
    editor.on("selectionUpdate", bump);
    return () => {
      editor.off("transaction", bump);
      editor.off("selectionUpdate", bump);
    };
  }, [editor]);

  const inactive = disabled || !editor;
  const chain = () => editor!.chain().focus();
  const is = (name: string, attrs?: Record<string, unknown>) => editor?.isActive(name, attrs) ?? false;

  const promptLink = () => {
    if (!editor) return;
    const prev = editor.getAttributes("link").href as string | undefined;
    const href = window.prompt(t("doc.linkPrompt"), prev ?? "https://");
    if (href === null) return;
    if (href === "") {
      chain().unsetLink().run();
      return;
    }
    chain().extendMarkRange("link").setLink({ href }).run();
  };

  const promptImage = () => {
    if (!editor) return;
    const src = window.prompt(t("doc.imagePrompt"), "");
    if (src) chain().setImage({ src }).run();
  };

  const insertTable = () => {
    chain().insertTable({ rows: 3, cols: 3, withHeaderRow: true }).run();
  };

  const insertMermaid = () => {
    chain()
      .insertContent({
        type: "codeBlock",
        attrs: { language: "mermaid" },
        content: [{ type: "text", text: "graph TD\n  A[Start] --> B[End]" }],
      })
      .run();
  };

  return (
    <div
      role="toolbar"
      aria-label={t("doc.richToolbar")}
      className="flex shrink-0 flex-wrap items-center gap-0.5 border-b border-border-divider bg-editor-canvas px-2 py-1"
    >
      <ToolButton icon={Heading1} label={t("doc.tbH1")} active={is("heading", { level: 1 })} disabled={inactive} onClick={() => chain().toggleHeading({ level: 1 }).run()} />
      <ToolButton icon={Heading2} label={t("doc.tbH2")} active={is("heading", { level: 2 })} disabled={inactive} onClick={() => chain().toggleHeading({ level: 2 }).run()} />
      <ToolButton icon={Heading3} label={t("doc.tbH3")} active={is("heading", { level: 3 })} disabled={inactive} onClick={() => chain().toggleHeading({ level: 3 }).run()} />
      <span className="mx-0.5 h-4 w-px bg-zinc-200 dark:bg-zinc-700" aria-hidden />
      <ToolButton icon={Bold} label={t("doc.tbBold")} active={is("bold")} disabled={inactive} onClick={() => chain().toggleBold().run()} />
      <ToolButton icon={Italic} label={t("doc.tbItalic")} active={is("italic")} disabled={inactive} onClick={() => chain().toggleItalic().run()} />
      <ToolButton icon={Strikethrough} label={t("doc.tbStrike")} active={is("strike")} disabled={inactive} onClick={() => chain().toggleStrike().run()} />
      <ToolButton icon={Code} label={t("doc.tbInlineCode")} active={is("code")} disabled={inactive} onClick={() => chain().toggleCode().run()} />
      <ToolButton icon={SquareCode} label={t("doc.tbCodeBlock")} active={is("codeBlock")} disabled={inactive} onClick={() => chain().toggleCodeBlock().run()} />
      <span className="mx-0.5 h-4 w-px bg-zinc-200 dark:bg-zinc-700" aria-hidden />
      <ToolButton icon={Quote} label={t("doc.tbQuote")} active={is("blockquote")} disabled={inactive} onClick={() => chain().toggleBlockquote().run()} />
      <ToolButton icon={List} label={t("doc.tbBulletList")} active={is("bulletList")} disabled={inactive} onClick={() => chain().toggleBulletList().run()} />
      <ToolButton icon={ListOrdered} label={t("doc.tbOrderedList")} active={is("orderedList")} disabled={inactive} onClick={() => chain().toggleOrderedList().run()} />
      <ToolButton icon={ListTodo} label={t("doc.tbTaskList")} active={is("taskList")} disabled={inactive} onClick={() => chain().toggleTaskList().run()} />
      <ToolButton icon={Table} label={t("doc.tbTable")} disabled={inactive} onClick={insertTable} />
      <ToolButton icon={Workflow} label={t("doc.tbMermaid")} disabled={inactive} onClick={insertMermaid} />
      <span className="mx-0.5 h-4 w-px bg-zinc-200 dark:bg-zinc-700" aria-hidden />
      <ToolButton icon={Link} label={t("doc.tbLink")} active={is("link")} disabled={inactive} onClick={promptLink} />
      <ToolButton icon={Image} label={t("doc.tbImage")} disabled={inactive} onClick={promptImage} />
      <ToolButton icon={Minus} label={t("doc.tbRule")} disabled={inactive} onClick={() => chain().setHorizontalRule().run()} />
      <span className="mx-0.5 h-4 w-px bg-zinc-200 dark:bg-zinc-700" aria-hidden />
      <ToolButton icon={Undo2} label={t("doc.tbUndo")} disabled={inactive || !editor?.can().undo()} onClick={() => chain().undo().run()} />
      <ToolButton icon={Redo2} label={t("doc.tbRedo")} disabled={inactive || !editor?.can().redo()} onClick={() => chain().redo().run()} />
    </div>
  );
}
