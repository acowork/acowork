/**
 * MermaidNodeView — codeBlock 的 mermaid NodeView（差距分析 35 §4.2，P1）。
 *
 * 富文本内 mermaid 代码块 → 可视化渲染（复用 chat MermaidBlock 的渲染核心：
 * ensureInit + renderMermaid，抽离在 `src/lib/mermaid.ts`），点击/按钮切换
 * 回代码编辑态。
 *
 * 设计约束：
 * - **不改变数据模型**：仍是 codeBlock + language=mermaid，markdown round-trip
 *   与序列化完全不变（NodeView 只管 DOM 渲染）。
 * - 非 mermaid 代码块走默认渲染（NodeViewContent as pre/code），本 NodeView
 *   只是 codeBlock 的「渲染器」，按 language 分流。
 * - NodeViewContent 恒挂载（contentDOM 稳定性要求），render 态用 CSS 隐藏。
 * - readOnly 时不出「编辑代码」入口；点击守卫 editor.isEditable。
 */

import { useEffect, useMemo, useRef, useState } from "react";
import {
  NodeViewContent,
  NodeViewWrapper,
  type NodeViewProps,
} from "@tiptap/react";
import { Pencil, Eye } from "lucide-react";
import { useTranslation } from "../../../i18n/useTranslation";
import { isPlausibleMermaid, renderMermaid } from "../../../lib/mermaid";
import { tokensFor } from "../../../lib/mermaidTheme";
import { useSettingsStore } from "../../../stores/settingsStore";
import { log } from "../../../lib/logger";

const RENDER_DEBOUNCE_MS = 200;

export function MermaidNodeView({ node, editor }: NodeViewProps) {
  const { t } = useTranslation();
  const [svg, setSvg] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const [editing, setEditing] = useState(false);
  // 渲染代次守卫：内容变化/卸载时丢弃过期结果。
  const renderToken = useRef(0);

  const isMermaid = node.attrs.language === "mermaid";
  const code = node.textContent;
  const editable = editor.isEditable;
  // 只读热切换（setEditable）不派发事务，node view 不会立即重渲染；
  // 用派生值保证下次重渲染时只读态强制回渲染态，编辑态不会卡死。
  const isEditing = editable && editing;

  // 主题调色板：mermaid 在渲染期把颜色烘焙进 SVG，切换主题必须重渲染。
  const theme = useSettingsStore((s) => s.theme);
  const osTheme = useSettingsStore((s) => s.osTheme);
  const tokens = useMemo(() => tokensFor(theme, osTheme), [theme, osTheme]);

  // 渲染态（非编辑时）debounce 渲染 mermaid。
  useEffect(() => {
    if (isEditing || !isMermaid || !isPlausibleMermaid(code)) return;
    const token = ++renderToken.current;
    const timer = setTimeout(async () => {
      try {
        const out = await renderMermaid(code, tokens);
        if (token !== renderToken.current) return;
        setSvg(out);
        setFailed(false);
      } catch (err) {
        log.error("[MermaidNodeView] render failed:", err);
        if (token !== renderToken.current) return;
        setSvg(null);
        setFailed(true);
      }
    }, RENDER_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [code, isEditing, isMermaid, tokens]);

  // 非 mermaid 代码块：默认渲染（不涉及 mermaid）。
  if (!isMermaid) {
    return (
      <NodeViewWrapper as="pre" className="[&_code]:font-mono">
        <NodeViewContent<"code"> as="code" />
      </NodeViewWrapper>
    );
  }

  return (
    <NodeViewWrapper
      data-mermaid-nodeview
      className="mermaid-nodeview group relative my-2 overflow-x-auto rounded-md border border-border-outer bg-zinc-50  dark:bg-zinc-900/50"
    >
      {/* 渲染态：图 / 占位 / 失败回退（代码） */}
      {!isEditing && svg && (
        <div
          className="cursor-pointer [&_svg]:block [&_svg]:max-w-full [&_svg]:h-auto"
          onClick={() => editable && setEditing(true)}
          title={t("doc.mermaidEdit")}
        >
          <div dangerouslySetInnerHTML={{ __html: svg }} />
        </div>
      )}
      {!isEditing && !svg && !failed && (
        <div className="flex min-h-[80px] items-center justify-center text-xs text-text-tertiary">
          {t("doc.mermaidRendering")}
        </div>
      )}
      {!isEditing && !svg && failed && (
        <pre className="m-0 whitespace-pre-wrap p-3 font-mono text-xs leading-relaxed text-text-tertiary">
          {code}
        </pre>
      )}

      {/* 代码编辑态 —— NodeViewContent 恒挂载（contentDOM 稳定），CSS 切换显隐 */}
      <div className={isEditing ? "p-3" : "hidden"}>
        <NodeViewContent<"pre"> as="pre" className="m-0 whitespace-pre-wrap font-mono text-xs leading-relaxed" />
      </div>

      {/* 切换按钮 */}
      {editable && !isEditing && (
        <button
          type="button"
          onClick={() => setEditing(true)}
          aria-label={t("doc.mermaidEdit")}
          title={t("doc.mermaidEdit")}
          className="absolute right-2 top-2 flex h-6 w-6 items-center justify-center rounded bg-white/90 text-text-tertiary opacity-0 shadow transition-opacity hover:text-zinc-800 group-hover:opacity-100 dark:bg-zinc-800/90 dark:hover:text-zinc-100"
        >
          <Pencil className="h-3 w-3" aria-hidden />
        </button>
      )}
      {editable && isEditing && (
        <button
          type="button"
          onClick={() => setEditing(false)}
          aria-label={t("doc.mermaidPreview")}
          title={t("doc.mermaidPreview")}
          className="absolute right-2 top-2 flex h-6 w-6 items-center justify-center rounded bg-white/90 text-text-tertiary shadow hover:text-zinc-800 dark:bg-zinc-800/90 dark:hover:text-zinc-100"
        >
          <Eye className="h-3 w-3" aria-hidden />
        </button>
      )}
    </NodeViewWrapper>
  );
}
