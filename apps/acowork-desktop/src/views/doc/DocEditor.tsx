/**
 * DocEditor — doc 视图右侧编辑器（设计 §7 / plan D2-3）。
 *
 * - 双编辑引擎（ADR-079 D1 / P0）：rich = Tiptap 富文本（默认，懒加载）；
 *   source = Monaco markdown 源码（保留，含 MarkdownToolbar + 表格导航）。
 * - 编辑/分栏/预览三模式：预览 = DocMarkdownView（同渲染栈）；分栏 =
 *   左引擎右预览（可拖拽，宽度持久化）。
 * - 保存：PUT 携带 `base_version`（乐观并发）；409 `version_conflict` →
 *   amber banner「文档已被他人更新」+ 刷新按钮（不静默覆盖）。
 * - 来源标记：Agent add-to-doc 导入的文档展示 instance_id 的 display_name
 *   + workspace_path badge（ADR-073：通过 agentStore 把 instance_id
 *   解析为人类可读名，而不是直接显示原始 UUID）。
 * - 快捷键 Ctrl/Cmd+S 保存（Tiptap / Monaco 聚焦时均可）。
 * - 切换文档且本地有未保存修改 → ConfirmDialog 确认丢弃。
 * - rich 引擎动态 import()（Tiptap + micromark 不进首屏 chunk）；
 *   加载失败 → 自动降级 Monaco + amber 提示（显式失败，不静默）。
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { Check, Columns2, Eye, FileText, Loader2, Pencil, RefreshCw, Save, Sparkles, Type, Braces } from "lucide-react";
import Editor, { type OnMount } from "@monaco-editor/react";
import type { editor } from "monaco-editor";
import { useTranslation } from "../../i18n/useTranslation";
import { useDocEditorStore } from "../../stores/doc/editorStore";
import { useDocHealthStore } from "../../stores/doc/healthStore";
import { useAgentStore } from "../../stores/agentStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { initMonaco } from "../../lib/monacoBootstrap";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { SplitHandle } from "../../components/common/SplitHandle";
import { useDragResize } from "../../hooks/useDragResize";
import { MarkdownToolbar } from "../../components/markdown/MarkdownToolbar";
import { registerMarkdownTableNavigation } from "../../components/markdown/editorAid";
import { cn } from "../../lib/utils";
import { DocMarkdownView } from "./DocMarkdownView";
// 仅类型引用（懒加载边界：DocRichEditor 运行时经动态 import() 进入）。
import type { DocRichEditor } from "../../components/doc/editor/DocRichEditor";

/** 解析 agent instance_id → 显示名（meta.display_name ?? meta.name ?? id）。
 *  agentStore 是按 instance_id 索引的（ADR-073），与 `import.instance_id`
 *  的语义一致 —— 这里「键即查表 key」。 */
function resolveAgentName(
  agents: Record<string, { meta?: { display_name?: string; name?: string } }>,
  id: string | null,
): string | null {
  if (!id) return null;
  const a = agents[id];
  if (!a?.meta) return id;
  return a.meta.display_name || a.meta.name || id;
}

/** Monaco markdown 源码面板（source 引擎 + rich 加载失败的降级共用）。 */
function MonacoPane({
  doc,
  content,
  monacoReady,
  monacoFailed,
  monacoTheme,
  editorFontSize,
  healthy,
  editorAria,
  loadFailedLabel,
  onMount,
  onChange,
}: {
  doc: { meta: { doc_id: string } };
  content: string;
  monacoReady: boolean;
  monacoFailed: boolean;
  monacoTheme: string;
  editorFontSize: number;
  /** boolean | null（healthStore 未加载时为 null；null 视为不可用 → 只读） */
  healthy: boolean | null;
  editorAria: string;
  loadFailedLabel: string;
  onMount: OnMount;
  onChange: (value: string) => void;
}) {
  if (!monacoReady) {
    return monacoFailed ? (
      <div className="flex h-full items-center justify-center gap-2 text-xs text-text-tertiary">
        <RefreshCw className="h-4 w-4" aria-hidden />
        {loadFailedLabel}
      </div>
    ) : (
      <div className="flex h-full items-center justify-center gap-2 text-xs text-text-tertiary">
        <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
      </div>
    );
  }
  return (
    <Editor
      path={`doc:${doc.meta.doc_id}`}
      value={content}
      language="markdown"
      theme={monacoTheme}
      onChange={(value) => onChange(value ?? "")}
      onMount={onMount}
      keepCurrentModel={false}
      options={{
        minimap: { enabled: false },
        fontSize: editorFontSize,
        lineNumbers: "on",
        scrollBeyondLastLine: false,
        wordWrap: "on",
        tabSize: 2,
        renderWhitespace: "selection",
        padding: { top: 8 },
        automaticLayout: true,
        readOnly: !healthy,
        ariaLabel: editorAria,
      }}
    />
  );
}

export function DocEditor() {
  const { t } = useTranslation();
  const healthy = useDocHealthStore((s) => s.healthy);
  const doc = useDocEditorStore((s) => s.doc);
  const content = useDocEditorStore((s) => s.content);
  const dirty = useDocEditorStore((s) => s.dirty);
  const saving = useDocEditorStore((s) => s.saving);
  const loading = useDocEditorStore((s) => s.loading);
  const mode = useDocEditorStore((s) => s.mode);
  const engine = useDocEditorStore((s) => s.engine);
  const conflict = useDocEditorStore((s) => s.conflict);
  const saveError = useDocEditorStore((s) => s.saveError);
  const pendingOpenDocId = useDocEditorStore((s) => s.pendingOpenDocId);
  const setMode = useDocEditorStore((s) => s.setMode);
  const setEngine = useDocEditorStore((s) => s.setEngine);
  const setContent = useDocEditorStore((s) => s.setContent);
  const save = useDocEditorStore((s) => s.save);
  const reload = useDocEditorStore((s) => s.reload);
  const confirmPendingOpen = useDocEditorStore((s) => s.confirmPendingOpen);
  const cancelPendingOpen = useDocEditorStore((s) => s.cancelPendingOpen);
  // ADR-073: agentStore.agents 是按 **instance_id** 索引的（UUID）；
  // `doc.meta.import.instance_id` 正好是同一 key，可直接查表解析为
  // 人类可读的 display name（> name > 原始 UUID）。Store 未加载时
  // fallback 到 instance_id 本身（badge 不至于显示空字符串）。
  const agents = useAgentStore((s) => s.agents);

  // ── Monaco 生命周期（与 FileEditorPanel 一致：后台加载 + gate）────
  const [monacoReady, setMonacoReady] = useState(false);
  const [monacoFailed, setMonacoFailed] = useState(false);
  const editorRef = useRef<editor.IStandaloneCodeEditor | null>(null);
  useEffect(() => {
    let cancelled = false;
    initMonaco().then(
      () => {
        if (!cancelled) setMonacoReady(true);
      },
      () => {
        if (!cancelled) setMonacoFailed(true);
      },
    );
    return () => {
      cancelled = true;
    };
  }, []);

  // ── Tiptap 富文本引擎（ADR-079 P0）：动态 import() 懒加载 ──────
  // micromark + Tiptap 不进首屏 chunk；加载失败 → 显式降级 source。
  const [RichEditorComp, setRichEditorComp] = useState<typeof DocRichEditor | null>(null);
  const [richFailed, setRichFailed] = useState(false);
  useEffect(() => {
    let cancelled = false;
    import("../../components/doc/editor/DocRichEditor").then(
      (mod) => {
        if (!cancelled) setRichEditorComp(() => mod.DocRichEditor);
      },
      () => {
        if (!cancelled) setRichFailed(true);
      },
    );
    return () => {
      cancelled = true;
    };
  }, []);

  // 主题 + 字号（与 FileEditorPanel 同一来源：settingsStore）
  const theme = useSettingsStore((s) => s.theme);
  const osTheme = useSettingsStore((s) => s.osTheme);
  const fontSize = useSettingsStore((s) => s.fontSize);
  const monacoTheme = useMemo(() => {
    if (theme === "dark") return "vs-dark";
    if (theme === "light") return "vs";
    return osTheme === "dark" ? "vs-dark" : "vs";
  }, [theme, osTheme]);
  const editorFontSize = useMemo(() => Math.round(fontSize * 16), [fontSize]);

  // split 模式预览列宽度（可拖拽 + localStorage 持久化）
  const preview = useDragResize({
    storageKey: "acowork-doc-split-width",
    defaultWidth: 380,
    minWidth: 200,
    maxWidth: 800,
  });

  const [savedTick, setSavedTick] = useState(0);

  // 空状态
  if (!doc && !loading) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 text-text-tertiary">
        <FileText className="h-8 w-8 opacity-40" aria-hidden />
        <p className="text-xs">{t("doc.editorEmpty")}</p>
      </div>
    );
  }

  if (loading && !doc) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-xs text-text-tertiary">
        <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
      </div>
    );
  }

  if (!doc) return null;

  const handleSave = async () => {
    const ok = await save();
    if (ok) {
      setSavedTick((n) => n + 1);
      setTimeout(() => setSavedTick((n) => n + 1), 2500);
    }
  };

  /** Tab / Shift+Tab inside a GFM table moves across cells — shared with the
   *  workspace file editor (editorAid + tableAid). DocEditor always edits
   *  markdown, so no languageId filter is needed. */
  const handleEditorMount: OnMount = (ed, monaco) => {
    editorRef.current = ed;
    // Ctrl/Cmd+S → save
    ed.addCommand(
      // eslint-disable-next-line no-bitwise
      monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS,
      () => void handleSave(),
    );
    registerMarkdownTableNavigation(ed, monaco);
  };

  const modeTab = (
    key: "edit" | "split" | "preview",
    label: string,
    icon: typeof Pencil,
  ) => (
    <button
      key={key}
      type="button"
      role="tab"
      aria-selected={mode === key}
      disabled={!healthy}
      onClick={() => setMode(key)}
      className={cn(
        "flex items-center gap-1 rounded px-2 py-0.5 transition-colors",
        mode === key
          ? "bg-[var(--color-accent)]/10 text-[var(--color-accent)]"
          : "text-text-tertiary hover:text-zinc-700 dark:hover:text-zinc-200",
      )}
    >
      {(() => {
        const Icon = icon;
        return <Icon className="h-3 w-3" aria-hidden />;
      })()}
      {label}
    </button>
  );

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col bg-editor-canvas">
      {/* ── 顶栏：标题 + 元信息 + 模式/保存 ─────────────────── */}
      <div className="flex shrink-0 items-center gap-2 border-b border-border-divider px-3 py-1.5">
        <FileText className="h-4 w-4 shrink-0 text-text-tertiary" aria-hidden />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate text-xs font-medium text-text-secondary ">
              {doc.meta.name}
            </span>
            {doc.meta.import && (() => {
              // ADR-073: resolve the runtime instance_id to its human
              // display name. Tooltip keeps the raw UUID for forensic
              // debuggability (so the actual instance can be grepped
              // against the Gateway installed_agents log).
              const importer = resolveAgentName(agents, doc.meta.import.instance_id);
              const instanceId = doc.meta.import.instance_id;
              return (
                <span
                  className="inline-flex max-w-[45%] shrink items-center gap-1 truncate rounded-full bg-violet-50 px-1.5 py-0.5 text-10 text-violet-600 dark:bg-violet-900/30 dark:text-violet-300"
                  title={`${instanceId} · ${doc.meta.import.workspace_path}`}
                >
                  <Sparkles className="h-2.5 w-2.5 shrink-0" aria-hidden />
                  <span className="truncate">{t("doc.importedBy", { agent: importer ?? instanceId })}</span>
                </span>
              );
            })()}
          </div>
          <div className="flex items-center gap-2 text-10 text-text-tertiary">
            <span className="truncate">{doc.path}</span>
            <span aria-label={t("doc.versionLabel")}>v{doc.meta.version}</span>
            {dirty && <span className="text-amber-500">{t("doc.unsaved")}</span>}
            {savedTick % 2 === 1 && !dirty && (
              <span className="inline-flex items-center gap-0.5 text-emerald-600">
                <Check className="h-2.5 w-2.5" aria-hidden />
                {t("doc.saved")}
              </span>
            )}
          </div>
        </div>

        {/* 编辑引擎切换（rich=Tiptap 富文本 / source=Monaco 源码；预览模式隐藏） */}
        {mode !== "preview" && (
          <div
            className="flex shrink-0 items-center rounded-md border border-border-outer p-0.5 text-11"
            role="group"
            aria-label={t("doc.engineLabel")}
          >
            <button
              type="button"
              role="tab"
              aria-selected={engine === "rich"}
              disabled={!healthy}
              onClick={() => setEngine("rich")}
              className={cn(
                "flex items-center gap-1 rounded px-2 py-0.5 transition-colors",
                engine === "rich"
                  ? "bg-[var(--color-accent)]/10 text-[var(--color-accent)]"
                  : "text-text-tertiary hover:text-zinc-700 dark:hover:text-zinc-200",
              )}
            >
              <Type className="h-3 w-3" aria-hidden />
              {t("doc.richText")}
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={engine === "source"}
              disabled={!healthy}
              onClick={() => setEngine("source")}
              className={cn(
                "flex items-center gap-1 rounded px-2 py-0.5 transition-colors",
                engine === "source"
                  ? "bg-[var(--color-accent)]/10 text-[var(--color-accent)]"
                  : "text-text-tertiary hover:text-zinc-700 dark:hover:text-zinc-200",
              )}
            >
              <Braces className="h-3 w-3" aria-hidden />
              {t("doc.source")}
            </button>
          </div>
        )}

        {/* 模式切换（编辑/分栏/预览） */}
        <div
          className="flex shrink-0 items-center rounded-md border border-border-outer p-0.5 text-11"
          role="tablist"
          aria-label={t("doc.modeLabel")}
        >
          {modeTab("edit", t("doc.edit"), Pencil)}
          {modeTab("split", t("doc.split"), Columns2)}
          {modeTab("preview", t("doc.preview"), Eye)}
        </div>

        <button
          type="button"
          disabled={!dirty || saving || !healthy}
          onClick={() => void handleSave()}
          className={cn(
            "flex shrink-0 items-center gap-1 rounded-md px-2 py-1 text-11 font-medium transition-colors disabled:opacity-40",
            dirty
              ? "bg-accent-strong text-white hover:opacity-90"
              : "border border-zinc-200 text-text-tertiary dark:border-zinc-700",
          )}
        >
          {saving ? <Loader2 className="h-3 w-3 animate-spin" aria-hidden /> : <Save className="h-3 w-3" aria-hidden />}
          {t("doc.save")}
        </button>
      </div>

      {/* ── 409 版本冲突 / 错误 banner ─────────────────────── */}
      {conflict && (
        <div className="flex shrink-0 items-center gap-2 border-b border-amber-200 bg-amber-50 px-3 py-1.5 text-11 text-amber-800 dark:border-amber-800/40 dark:bg-amber-900/25 dark:text-amber-200">
          <span className="flex-1">{t("doc.versionConflict")}</span>
          <button
            type="button"
            onClick={() => void reload()}
            className="inline-flex items-center gap-1 rounded bg-amber-600 px-2 py-0.5 font-medium text-white hover:bg-amber-700"
          >
            <RefreshCw className="h-3 w-3" aria-hidden />
            {t("doc.refreshNow")}
          </button>
        </div>
      )}
      {saveError && !conflict && (
        <div className="flex shrink-0 items-center gap-2 border-b border-red-200 bg-red-50 px-3 py-1.5 text-11 text-red-700 dark:border-red-800/40 dark:bg-red-900/25 dark:text-red-200">
          <span className="flex-1">{saveError}</span>
        </div>
      )}

      {/* ── 工具栏（编辑/分栏模式下显示） ─────────────────── */}
      {/* source 引擎：MarkdownToolbar（Monaco executeEdits）；rich 引擎自带 RichToolbar */}
      {mode !== "preview" && engine === "source" && (
        <MarkdownToolbar editor={editorRef.current} disabled={!healthy} />
      )}

      {/* rich 引擎加载失败 → 显式降级提示（ADR-079 §7：不静默回退） */}
      {mode !== "preview" && engine === "rich" && richFailed && (
        <div className="flex shrink-0 items-center gap-2 border-b border-amber-200 bg-amber-50 px-3 py-1.5 text-11 text-amber-800 dark:border-amber-800/40 dark:bg-amber-900/25 dark:text-amber-200">
          <span className="flex-1">{t("doc.richLoadFailed")}</span>
        </div>
      )}

      {/* ── 编辑 / 分栏 / 预览 ─────────────────────────────── */}
      {mode === "preview" ? (
        <DocMarkdownView content={content} />
      ) : (
        <div className="flex h-full min-h-0 flex-1">
          {/* 左：编辑引擎（rich=Tiptap 懒加载 / source=Monaco） */}
          <div className="min-h-0 min-w-0 flex-1">
            {engine === "rich" ? (
              richFailed ? (
                // 富文本加载失败 → 降级 Monaco 源码编辑
                <MonacoPane
                  doc={doc}
                  content={content}
                  monacoReady={monacoReady}
                  monacoFailed={monacoFailed}
                  monacoTheme={monacoTheme}
                  editorFontSize={editorFontSize}
                  healthy={healthy}
                  editorAria={t("doc.editorAria")}
                  loadFailedLabel={t("doc.editorLoadFailed")}
                  onMount={handleEditorMount}
                  onChange={setContent}
                />
              ) : RichEditorComp ? (
                <RichEditorComp
                  key={doc.meta.doc_id}
                  contentMd={content}
                  readOnly={!healthy}
                  placeholder={t("doc.richPlaceholder")}
                  onContentChange={setContent}
                  onSave={() => void handleSave()}
                />
              ) : (
                <div className="flex h-full items-center justify-center gap-2 text-xs text-text-tertiary">
                  <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
                  {t("doc.editorLoadingRich")}
                </div>
              )
            ) : (
              <MonacoPane
                doc={doc}
                content={content}
                monacoReady={monacoReady}
                monacoFailed={monacoFailed}
                monacoTheme={monacoTheme}
                editorFontSize={editorFontSize}
                healthy={healthy}
                editorAria={t("doc.editorAria")}
                loadFailedLabel={t("doc.editorLoadFailed")}
                onMount={handleEditorMount}
                onChange={setContent}
              />
            )}
          </div>

          {/* 右：预览（split 模式） */}
          {mode === "split" && (
            <>
              <SplitHandle onMouseDown={preview.onHandleMouseDown} ariaLabel={t("doc.split")} />
              <div className="min-h-0 min-w-0 shrink-0" style={{ width: preview.width }}>
                <DocMarkdownView content={content} />
              </div>
            </>
          )}
        </div>
      )}

      {/* ── 切文档确认（丢弃未保存修改；由 editorStore.pendingOpenDocId 驱动） */}
      <ConfirmDialog
        open={pendingOpenDocId !== null}
        title={t("doc.discardTitle")}
        message={t("doc.discardSwitchMsg")}
        confirmLabel={t("doc.discard")}
        destructive
        onCancel={() => cancelPendingOpen()}
        onConfirm={() => void confirmPendingOpen()}
      />
    </div>
  );
}
