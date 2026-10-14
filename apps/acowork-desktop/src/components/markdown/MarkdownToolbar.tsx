/**
 * MarkdownToolbar — insert Markdown snippets into the Doc editor's Monaco
 * instance.
 *
 * Every action goes through `editor.executeEdits`, so inserts participate in
 * Monaco's undo/redo stack (Ctrl+Z undoes a toolbar insert). The component
 * holds NO document state: the snippet generators in `./snippets` are pure
 * functions over the current selection, and the editor model stays the single
 * source of truth.
 *
 * The toolbar is deliberately dumb — it never talks to the doc store. It
 * receives the live `editor` instance from DocEditor's onMount.
 */

import { useEffect, useRef, useState } from "react";
import type { editor } from "monaco-editor";
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
  SquareCode,
  Strikethrough,
  Table,
  Workflow,
  type LucideIcon,
} from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import {
  bold,
  bulletList,
  codeBlock,
  heading,
  image,
  inlineCode,
  italic,
  link,
  mermaid,
  orderedList,
  quote,
  rule,
  strike,
  table,
  taskList,
  type SnippetResult,
} from "./snippets";

interface MarkdownToolbarProps {
  /** Live Monaco editor instance (null until Monaco has mounted). */
  editor: editor.IStandaloneCodeEditor | null;
  /** Disable all actions (e.g. doc service offline). */
  disabled?: boolean;
}

/**
 * Replace the current selection with a snippet and place the cursor inside it.
 * `make` receives the selected text so wraps (bold, lists, …) can adapt.
 */
function applySnippet(
  ed: editor.IStandaloneCodeEditor,
  make: (selected: string) => SnippetResult,
): void {
  const model = ed.getModel();
  const sel = ed.getSelection();
  if (!model || !sel) return;

  // `getSelection()` returns a plain ISelection object (not the Selection
  // class), so read the position fields directly instead of calling methods.
  const startOffset = model.getOffsetAt({
    lineNumber: sel.startLineNumber,
    column: sel.startColumn,
  });
  const selected = model.getValueInRange(sel);
  const snippet = make(selected);

  ed.executeEdits("acowork.markdown-toolbar", [
    { range: sel, text: snippet.text, forceMoveMarkers: true },
  ]);

  const cursorOffset = startOffset + snippet.selectionStart;
  const pos = model.getPositionAt(cursorOffset);
  if (snippet.selectionEnd !== undefined && snippet.selectionEnd > snippet.selectionStart) {
    const endPos = model.getPositionAt(startOffset + snippet.selectionEnd);
    ed.setSelection({
      startLineNumber: pos.lineNumber,
      startColumn: pos.column,
      endLineNumber: endPos.lineNumber,
      endColumn: endPos.column,
    });
  } else {
    ed.setPosition(pos);
  }
  ed.focus();
}

interface ToolbarItem {
  key: string;
  icon: LucideIcon;
  label: string;
  run: () => void;
}

export function MarkdownToolbar({ editor, disabled }: MarkdownToolbarProps) {
  const { t } = useTranslation();
  const [tableOpen, setTableOpen] = useState(false);
  const [rows, setRows] = useState(3);
  const [cols, setCols] = useState(3);
  const popRef = useRef<HTMLDivElement>(null);

  // Close the table picker on outside click.
  useEffect(() => {
    if (!tableOpen) return;
    const onDown = (e: MouseEvent) => {
      if (popRef.current && !popRef.current.contains(e.target as Node)) setTableOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [tableOpen]);

  const inactive = disabled || !editor;
  const run = (make: (selected: string) => SnippetResult) => () => {
    if (editor) applySnippet(editor, make);
  };

  const items: ToolbarItem[] = [
    { key: "h1", icon: Heading1, label: t("doc.tbH1"), run: run((s) => heading(1, s)) },
    { key: "h2", icon: Heading2, label: t("doc.tbH2"), run: run((s) => heading(2, s)) },
    { key: "h3", icon: Heading3, label: t("doc.tbH3"), run: run((s) => heading(3, s)) },
    { key: "bold", icon: Bold, label: t("doc.tbBold"), run: run(bold) },
    { key: "italic", icon: Italic, label: t("doc.tbItalic"), run: run(italic) },
    { key: "strike", icon: Strikethrough, label: t("doc.tbStrike"), run: run(strike) },
    { key: "inlineCode", icon: Code, label: t("doc.tbInlineCode"), run: run(inlineCode) },
    { key: "codeBlock", icon: SquareCode, label: t("doc.tbCodeBlock"), run: run((s) => codeBlock("", s)) },
    { key: "link", icon: Link, label: t("doc.tbLink"), run: run(link) },
    { key: "image", icon: Image, label: t("doc.tbImage"), run: run(image) },
    { key: "quote", icon: Quote, label: t("doc.tbQuote"), run: run(quote) },
    { key: "bulletList", icon: List, label: t("doc.tbBulletList"), run: run(bulletList) },
    { key: "orderedList", icon: ListOrdered, label: t("doc.tbOrderedList"), run: run(orderedList) },
    { key: "taskList", icon: ListTodo, label: t("doc.tbTaskList"), run: run(taskList) },
    { key: "mermaid", icon: Workflow, label: t("doc.tbMermaid"), run: run(mermaid) },
    { key: "rule", icon: Minus, label: t("doc.tbRule"), run: run(rule) },
  ];

  const insertTable = () => {
    if (!editor) return;
    applySnippet(editor, () => table(rows, cols, (i) => t("doc.tbColLabel", { n: i + 1 })));
    setTableOpen(false);
  };

  const btnClass = (active?: boolean) =>
    cn(
      "flex h-7 w-7 shrink-0 items-center justify-center rounded transition-colors",
      active
        ? "bg-[var(--color-accent)]/10 text-[var(--color-accent)]"
        : "text-text-tertiary hover:bg-zinc-100 hover:text-zinc-800  dark:hover:bg-zinc-800 dark:hover:text-zinc-100",
      inactive && "pointer-events-none opacity-40",
    );

  return (
    <div
      role="toolbar"
      aria-label={t("doc.toolbarLabel")}
      className="flex shrink-0 flex-wrap items-center gap-0.5 border-b border-border-divider bg-editor-canvas px-2 py-1"
    >
      {items.map((item) => (
        <button
          key={item.key}
          type="button"
          title={item.label}
          aria-label={item.label}
          disabled={inactive}
          onClick={item.run}
          className={btnClass()}
        >
          <item.icon className="h-3.5 w-3.5" aria-hidden />
        </button>
      ))}

      {/* ── Table insert (n×m picker) ─────────────────────── */}
      <div ref={popRef} className="relative">
        <button
          type="button"
          title={t("doc.tbTable")}
          aria-label={t("doc.tbTable")}
          aria-expanded={tableOpen}
          disabled={inactive}
          onClick={() => setTableOpen((v) => !v)}
          className={btnClass(tableOpen)}
        >
          <Table className="h-3.5 w-3.5" aria-hidden />
        </button>
        {tableOpen && (
          <div className="absolute right-0 top-full z-30 mt-1 flex items-center gap-2 rounded-md border border-border-outer bg-white p-2 shadow-lg dark:bg-zinc-900">
            <label className="flex items-center gap-1 text-11 text-text-tertiary ">
              {t("doc.tbTableRows")}
              <input
                type="number"
                min={1}
                max={20}
                value={rows}
                onChange={(e) => setRows(Math.max(1, Math.min(20, Number.parseInt(e.target.value, 10) || 1)))}
                className="w-12 rounded border border-border-outer px-1 py-0.5 text-11  dark:bg-zinc-800"
              />
            </label>
            <label className="flex items-center gap-1 text-11 text-text-tertiary ">
              {t("doc.tbTableCols")}
              <input
                type="number"
                min={1}
                max={12}
                value={cols}
                onChange={(e) => setCols(Math.max(1, Math.min(12, Number.parseInt(e.target.value, 10) || 1)))}
                className="w-12 rounded border border-border-outer px-1 py-0.5 text-11  dark:bg-zinc-800"
              />
            </label>
            <button
              type="button"
              onClick={insertTable}
              className="rounded bg-accent-strong px-2 py-0.5 text-11 font-medium text-white hover:opacity-90"
            >
              {t("doc.tbTableInsert")}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
