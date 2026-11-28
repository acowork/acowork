/**
 * DocMarkdownView — doc 文档只读渲染（预览模式）。
 *
 * 与 `MarkdownPreviewView` 同一渲染栈（react-markdown + remark-gfm +
 * rehype-raw + CodeBlock + 表格滚动 override + prose 样式 token），保证
 * 视觉一致。区别：doc 是独立领域（无 workspace 文件上下文），内链/资源
 * 不做跨工作区解析 —— http(s)/mailto 正常链接，其余相对路径原样渲染。
 */

import { Children, isValidElement, type AnchorHTMLAttributes, type MouseEvent, type ReactElement, type ReactNode, type TableHTMLAttributes } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeRaw from "rehype-raw";
import { CodeBlock } from "../../components/chat/CodeBlock";
import { classifyMarkdownLink } from "../../lib/linkResolver";
import { openDocLinkOrNotify } from "../../components/editor/markdownLinkResolver";
import { cn } from "../../lib/utils";

/**
 * 文档正文里的链接该由谁处理。
 *
 * 这里过去把每个 `<a>` 都渲染成 `target="_blank"`，于是一条指向库内另一篇文档
 * 的相对链接（`../评审/y.md`）在 webview 里既打不开也报错 —— 和「点了没反应」
 * 是同一个症状。现在：外部链接与透传协议保持原样（新窗口），库内地址交给
 * `openDocLinkOrNotify`，用**当前文档所在目录**作为相对路径的基准，
 * 由 `openDocEntry` 决定文本视图还是字节 viewer。
 */
function useDocAnchorHandler(basePath: string | undefined) {
  return (e: MouseEvent<HTMLAnchorElement>, href: string | undefined) => {
    if (!href) return;
    const link = classifyMarkdownLink(href);
    if (link.kind !== "path") return; // external / passthrough / anchor：交给默认行为
    e.preventDefault();
    void openDocLinkOrNotify(link.path ?? href, { basePath });
  };
}

/** 与 MarkdownPreviewView 对齐的 ReactMarkdown components override */
function buildMarkdownComponents(basePath: string | undefined) {
  const handleAnchor = useDocAnchorHandler(basePath);
  return {
    pre: ({ children }: { children?: ReactNode }) => {
      const childArray = Children.toArray(children);
      const codeEl = childArray.find(
        (child): child is ReactElement<{ className?: string; children?: ReactNode }> =>
          isValidElement(child) && child.type === "code",
      );
      if (codeEl) {
        const { className, children: codeContent } = codeEl.props;
        const language = className?.replace(/^language-/, "") || "";
        const code = Children.toArray(codeContent).join("");
        return <CodeBlock language={language} code={code} />;
      }
      return <pre>{children}</pre>;
    },
    table: ({ children, ...rest }: TableHTMLAttributes<HTMLTableElement>) => (
      <div className="prose-table-scroll">
        <table {...rest}>{children}</table>
      </div>
    ),
    a: ({ onClick: _ignored, ...props }: AnchorHTMLAttributes<HTMLAnchorElement>) => (
      <a
        {...props}
        target="_blank"
        rel="noreferrer"
        onClick={(e) => handleAnchor(e, props.href)}
      />
    ),
  };
}

export function DocMarkdownView({
  content,
  basePath,
}: {
  content: string;
  /** 当前文档的库内路径 —— 正文里的相对链接以它所在目录为基准。 */
  basePath?: string;
}) {
  const markdownComponents = buildMarkdownComponents(basePath);
  return (
    <div
      className={cn(
        "markdown-preview prose prose-sm prose-zinc max-w-none h-full overflow-y-auto bg-editor-canvas px-5 py-4",
      )}
    >
      <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeRaw]} components={markdownComponents}>
        {content}
      </ReactMarkdown>
    </div>
  );
}
