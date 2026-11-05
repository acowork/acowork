/**
 * DocRichEditor — Doc 视图的 Tiptap 富文本编辑引擎（ADR-079 P0）。
 *
 * 职责边界：
 * - 持有 Tiptap `useEditor` 实例（ExtensionKit，见 `extension-kit.ts`）。
 * - 输入/输出均为 **markdown 字符串**（与 editorStore 的 `content` 同一事实源）：
 *   - 挂载 / 外部更新（reload / applyMergedUpdate）时 `md → JSON` 载入；
 *   - 每次编辑 `onUpdate` 立即 `JSON → md` 回写 store（P0 会话层语义：
 *     .md 仍是权威，Tiptap 只是编辑层）。
 * - 外部内容同步：以 `lastEmittedMd` ref 区分「自己回写」与「外部推送」，
 *   避免 setContent 死循环；外部推送仅在内容确实不同时 setContent。
 * - Ctrl/Cmd+S → onSave（走现有 PUT + base_version 链路）。
 * - `readOnly` 热切换 `setEditable()`（对齐 DocFlow 的只读优先级链）。
 *
 * ⚠️ 懒加载边界：本模块（含 extension-kit + markdown 转换器）只允许被
 * DocEditor 通过动态 `import()` 引入，避免 micromark/Tiptap 拖慢首屏。
 */

import { useEffect, useRef } from "react";
import { EditorContent, useEditor } from "@tiptap/react";
import { useTranslation } from "../../../i18n/useTranslation";
import { markdownToTiptapJSON, tiptapToMarkdown } from "../../../lib/markdown";
import { cn } from "../../../lib/utils";
import { RICH_EDITOR_CHAR_LIMIT, buildExtensionKit } from "./extension-kit";
import { RichToolbar } from "./RichToolbar";
import { TableBubbleMenu } from "./TableBubbleMenu";

export interface DocRichEditorProps {
  /** markdown 事实源（editorStore.content）。 */
  contentMd: string;
  /** doc 服务离线时只读（健康度 gate）。 */
  readOnly: boolean;
  /** 空文档占位文案（i18n）。 */
  placeholder?: string;
  /** 编辑产生的新 markdown（→ editorStore.setContent）。 */
  onContentChange: (md: string) => void;
  /** Ctrl/Cmd+S（→ editorStore.save）。 */
  onSave: () => void;
  /** 编辑器实例就绪回调（测试用；P1 Collaboration 扩展注入 provider 也需要）。 */
  onEditorReady?: (editor: ReturnType<typeof useEditor>) => void;
}

/** canonical 形式：空 → ""；否则去除尾部空白 + 单个 \n（与 tiptapToMarkdown 输出对齐）。 */
export function canonicalMd(md: string): string {
  return md.trim() === "" ? "" : md.replace(/\s*$/, "") + "\n";
}

export function DocRichEditor({
  contentMd,
  readOnly,
  placeholder,
  onContentChange,
  onSave,
  onEditorReady,
}: DocRichEditorProps) {
  const { t } = useTranslation();

  // 最新回调引用（避免 useEditor 闭包捕获过期 prop）。
  const onContentChangeRef = useRef(onContentChange);
  const onSaveRef = useRef(onSave);
  const onEditorReadyRef = useRef(onEditorReady);
  useEffect(() => {
    onContentChangeRef.current = onContentChange;
    onSaveRef.current = onSave;
    onEditorReadyRef.current = onEditorReady;
  }, [onContentChange, onSave, onEditorReady]);

  // 最近一次「回写给 store」的 md —— 外部内容同步的判据。
  // 统一 canonical 形式（尾随单个 \n），使初始回显与 round-trip 输出可比。
  const lastEmittedMd = useRef(canonicalMd(contentMd));

  const editor = useEditor(
    {
      immediatelyRender: true,
      extensions: buildExtensionKit({ placeholder }),
      content: markdownToTiptapJSON(contentMd),
      editable: !readOnly,
      editorProps: {
        attributes: {
          class: "doc-rich-content prose prose-sm prose-zinc max-w-none focus:outline-none",
        },
        handleKeyDown: (_, event) => {
          if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "s") {
            event.preventDefault();
            onSaveRef.current();
            return true;
          }
          return false;
        },
      },
      onUpdate: ({ editor: ed }) => {
        const md = canonicalMd(tiptapToMarkdown(ed.getJSON()));
        // 守卫：初始回显 / 无实际内容变化的 transaction 不污染 dirty 标志。
        if (md === lastEmittedMd.current) return;
        lastEmittedMd.current = md;
        onContentChangeRef.current(md);
      },
      onCreate: ({ editor: ed }) => {
        onEditorReadyRef.current?.(ed);
      },
    },
    [],
  );

  // 外部内容同步（reload / applyMergedUpdate / 409 刷新）。
  useEffect(() => {
    if (!editor) return;
    const incoming = canonicalMd(contentMd);
    if (incoming === lastEmittedMd.current) return; // 自己回写的 echo
    lastEmittedMd.current = incoming;
    editor.commands.setContent(markdownToTiptapJSON(incoming), { emitUpdate: false });
  }, [editor, contentMd]);

  // 只读热切换（对齐 DocFlow setEditable 不重建实例）。
  //
  // `emitUpdate=false` 是必须的：Tiptap v3 的 `setEditable(editable,
  // emitUpdate = true)` 会发出一个 **update 事件**（哪怕 doc 没变），而
  // 本组件的 `onUpdate` 把"编辑器序列化结果 ≠ 载入时的原始 markdown"当
  // 成用户编辑。编辑器在载入时会把 markdown 规范化（`* a` → `- a`、
  // 两空格硬换行 → `\`、脚注节点丢失…），于是"打开即未保存"——
  // 任何正文含这些构造的文档一点开就是 dirty，切换文档就弹丢弃确认。
  useEffect(() => {
    if (editor) editor.setEditable(!readOnly, false);
  }, [editor, readOnly]);

  const charCount =
    (editor?.storage.characterCount?.characters?.() as number | undefined) ?? 0;
  const overLimit = charCount > RICH_EDITOR_CHAR_LIMIT;

  return (
    <div className="flex h-full min-h-0 flex-col bg-editor-canvas">
      <RichToolbar editor={editor} disabled={readOnly} />
      <div className="doc-rich-editor-scroll min-h-0 flex-1 overflow-y-auto">
        <EditorContent editor={editor} className="h-full" />
        <TableBubbleMenu editor={editor} readOnly={readOnly} />
      </div>
      {/* 字符计数（DocFlow 同款上限，ADR-079 §7 大文档性能） */}
      <div
        className={cn(
          "flex shrink-0 items-center justify-end border-t border-border-divider px-3 py-0.5 text-10 tabular-nums",
          overLimit ? "text-red-500" : "text-text-tertiary",
        )}
      >
        {t("doc.charCount", { count: charCount, limit: RICH_EDITOR_CHAR_LIMIT })}
      </div>
    </div>
  );
}
