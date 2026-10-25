/**
 * ChatMarkdown — the one markdown renderer every chat surface shares.
 *
 * Why this exists: the chat shell (`prose prose-sm prose-zinc … [&_th]:bg-chat-title
 * …`) and its element overrides were copied verbatim into three files —
 * `AgentChatView`, `GroupChatPanel`, `MessageBubble` — with a "keep in sync"
 * comment where the duplication met. The copies had already drifted: the group
 * surface's fenced-code handler dropped `dedent`, and its link handler
 * hard-coded `text-[var(--color-accent)] underline` instead of the house
 * `.prose a` recipe (globals.css:1225). That is exactly how the markdown-table
 * defect happened — one copy lost the `prose` scope every table rule in
 * globals.css is written under, so tables parsed but painted as plain text
 * while the same reply rendered correctly in the execution view.
 *
 * The shell is load-bearing, not cosmetic: `.prose table` (frame, borders),
 * `.prose th` / `.prose td` (padding, header shade) and the zebra override are
 * all scoped under `.prose`. Rendering markdown anywhere else in a chat surface
 * means going through this component so the scope cannot be forgotten again.
 */

import { Children, isValidElement, memo, type CSSProperties } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { Components } from "react-markdown";
import { CodeBlock } from "./CodeBlock";
import { handleChatMarkdownLinkClick, markdownUrlTransform } from "../editor/markdownLinkResolver";

/** Strip common leading whitespace from multi-line strings (same helper the
 *  execution bubbles use for indented code blocks). */
function dedent(code: string): string {
  const lines = code.split("\n");
  const nonEmpty = lines.filter((l) => l.trim().length > 0);
  if (nonEmpty.length === 0) return code.trim();
  const minIndent = Math.min(...nonEmpty.map((l) => l.match(/^ */)?.[0].length ?? 0));
  return lines.map((l) => l.slice(minIndent)).join("\n").trim();
}

/**
 * Behavioural overrides only — code fencing, safe link routing, table scroll.
 * Deliberately carries no font sizes: typography comes from the `prose` shell
 * (and `.prose a` for links), so a surface that wants different block spacing
 * passes `components` instead of forking this map.
 */
export const chatMarkdownComponents: Components = {
  pre: ({ children }: { children?: React.ReactNode }) => {
    const codeEl = Children.toArray(children).find(
      (child): child is React.ReactElement<{ className?: string; children?: React.ReactNode }> =>
        isValidElement(child) && child.type === "code",
    );
    if (codeEl) {
      const { className, children: codeContent } = codeEl.props;
      const language = className?.replace(/^language-/, "") || "";
      const code = dedent(Children.toArray(codeContent).join(""));
      return <CodeBlock language={language} code={code} />;
    }
    return <pre>{children}</pre>;
  },
  a: ({ href, children, node: _node, ...rest }: React.AnchorHTMLAttributes<HTMLAnchorElement> & { node?: unknown }) => {
    // In-chat links open through the workspace link resolver, never the webview
    // default (which would navigate the whole surface away from the chat).
    if (!href) return <span>{children}</span>;
    return (
      <a
        {...rest}
        href={href}
        onClick={(e) => {
          e.preventDefault();
          handleChatMarkdownLinkClick(href);
        }}
      >
        {children}
      </a>
    );
  },
  /** Same scroll wrapper every desktop markdown surface uses: a GFM table
   *  cannot shrink below its widest cell, so unwrapped it widens the bubble
   *  instead of scrolling inside it. */
  table: ({ children, ...rest }: React.TableHTMLAttributes<HTMLTableElement>) => (
    <div className="prose-table-scroll">
      <table {...rest}>{children}</table>
    </div>
  ),
};

/**
 * The markdown shell every chat surface uses — chat transcript, interaction
 * card, and execution bubble alike. All three local copies carried this exact
 * string, which is why it is the shared default rather than a per-surface prop.
 *
 * It is load-bearing, not cosmetic: every table rule in globals.css
 * (`.prose table` frame/borders, `.prose th` header shade, `.prose td` padding,
 * the zebra override) is scoped under `.prose`, and the `[&_th]:bg-chat-title`
 * / `[&_td]:bg-chat-body` pair is what keeps table cells opaque on a bubble.
 * Stripping any part of it repaints markdown in every chat surface at once —
 * which is exactly how the markdown-table defect happened when one copy lost
 * the `prose` scope.
 */
export const CHAT_MARKDOWN_SHELL_CLASS =
  "prose prose-sm prose-zinc max-w-none prose-h1:text-lg prose-h2:text-base prose-h3:text-sm prose-h4:text-sm prose-headings:font-semibold select-text break-words [&_th]:bg-chat-title [&_td]:bg-chat-body [&_tbody_tr]:!bg-transparent";

/** Module-level constant so react-markdown never sees a fresh array identity
 *  per render (an inline `[remarkGfm]` defeats memoization inside the library's
 *  own plugin-equality checks on every re-render). */
const REMARK_PLUGINS = [remarkGfm];

interface ChatMarkdownProps {
  content: string;
  /** Surface-specific element overrides, merged over the shared behavioural set.
   *  Use this for block spacing/typography; do not re-declare `pre` / `a` /
   *  `table` here — that is how the copies diverged in the first place. */
  components?: Components;
  /** Shell inline style. The execution surface scales chat text with the user's
   *  font-size setting; chat bubbles leave it unset. */
  style?: CSSProperties;
  /** Extra shell classes appended to the shell (surface-specific tweaks that
   *  are not element overrides, e.g. a tighter leading on a card). */
  className?: string;
}

export const ChatMarkdown = memo(function ChatMarkdown({
  content,
  components,
  style,
  className,
}: ChatMarkdownProps) {
  return (
    <div className={className ? `${CHAT_MARKDOWN_SHELL_CLASS} ${className}` : CHAT_MARKDOWN_SHELL_CLASS} style={style}>
      <ReactMarkdown
        remarkPlugins={REMARK_PLUGINS}
        urlTransform={markdownUrlTransform}
        components={components ? { ...chatMarkdownComponents, ...components } : chatMarkdownComponents}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
});
