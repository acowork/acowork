import { useState, useCallback, useMemo } from "react";
import { ChevronRight, ChevronDown, Copy, Check } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import hljs from "highlight.js/lib/core";
import javascript from "highlight.js/lib/languages/javascript";
import typescript from "highlight.js/lib/languages/typescript";
import python from "highlight.js/lib/languages/python";
import rust from "highlight.js/lib/languages/rust";
import c from "highlight.js/lib/languages/c";
import cpp from "highlight.js/lib/languages/cpp";
import java from "highlight.js/lib/languages/java";
import csharp from "highlight.js/lib/languages/csharp";
import go from "highlight.js/lib/languages/go";
import php from "highlight.js/lib/languages/php";
import ruby from "highlight.js/lib/languages/ruby";
import swift from "highlight.js/lib/languages/swift";
import kotlin from "highlight.js/lib/languages/kotlin";
import bash from "highlight.js/lib/languages/bash";
import json from "highlight.js/lib/languages/json";
import yaml from "highlight.js/lib/languages/yaml";
import css from "highlight.js/lib/languages/css";
import xml from "highlight.js/lib/languages/xml";
import sql from "highlight.js/lib/languages/sql";
import markdown from "highlight.js/lib/languages/markdown";
import shell from "highlight.js/lib/languages/shell";
import ini from "highlight.js/lib/languages/ini";
import diff from "highlight.js/lib/languages/diff";
import plaintext from "highlight.js/lib/languages/plaintext";
import { MermaidBlock } from "./MermaidBlock";

hljs.registerLanguage("javascript", javascript);
hljs.registerLanguage("js", javascript);
hljs.registerLanguage("typescript", typescript);
hljs.registerLanguage("ts", typescript);
hljs.registerLanguage("python", python);
hljs.registerLanguage("py", python);
hljs.registerLanguage("rust", rust);
hljs.registerLanguage("rs", rust);
hljs.registerLanguage("c", c);
hljs.registerLanguage("h", c);
hljs.registerLanguage("cpp", cpp);
hljs.registerLanguage("c++", cpp);
hljs.registerLanguage("cc", cpp);
hljs.registerLanguage("cxx", cpp);
hljs.registerLanguage("java", java);
hljs.registerLanguage("csharp", csharp);
hljs.registerLanguage("cs", csharp);
hljs.registerLanguage("go", go);
hljs.registerLanguage("golang", go);
hljs.registerLanguage("php", php);
hljs.registerLanguage("ruby", ruby);
hljs.registerLanguage("rb", ruby);
hljs.registerLanguage("swift", swift);
hljs.registerLanguage("kotlin", kotlin);
hljs.registerLanguage("kt", kotlin);
hljs.registerLanguage("bash", bash);
hljs.registerLanguage("sh", bash);
hljs.registerLanguage("json", json);
hljs.registerLanguage("yaml", yaml);
hljs.registerLanguage("yml", yaml);
hljs.registerLanguage("css", css);
hljs.registerLanguage("xml", xml);
hljs.registerLanguage("html", xml);
hljs.registerLanguage("sql", sql);
hljs.registerLanguage("markdown", markdown);
hljs.registerLanguage("md", markdown);
hljs.registerLanguage("shell", shell);
hljs.registerLanguage("ini", ini);
hljs.registerLanguage("toml", ini);
hljs.registerLanguage("diff", diff);
hljs.registerLanguage("plaintext", plaintext);
hljs.registerLanguage("text", plaintext);

interface CodeBlockProps {
    language: string;
    code: string;
}

export function CodeBlock({ language, code }: CodeBlockProps) {
    const { t } = useTranslation();
    const [collapsed, setCollapsed] = useState(false);
    const [copied, setCopied] = useState(false);

    const highlighted = useMemo(() => {
        const lang = language || "";
        try {
            if (lang && hljs.getLanguage(lang)) {
                const result = hljs.highlight(code, { language: lang });
                return result.value;
            }
        } catch {
            // fall through to auto-detect
        }
        const result = hljs.highlightAuto(code);
        return result.value;
    }, [code, language]);

    const handleCopy = useCallback(async () => {
        try {
            await navigator.clipboard.writeText(code);
            setCopied(true);
            setTimeout(() => setCopied(false), 2000);
        } catch {
            const ta = document.createElement("textarea");
            ta.value = code;
            document.body.appendChild(ta);
            ta.select();
            document.execCommand("copy");
            document.body.removeChild(ta);
            setCopied(true);
            setTimeout(() => setCopied(false), 2000);
        }
    }, [code]);

    // Route mermaid diagrams to dedicated renderer (must be after ALL hooks)
    if (language === "mermaid") {
        return <MermaidBlock chart={code} />;
    }

    const langLabel = language || "code";

    return (
        <div className="overflow-hidden rounded-md border border-chat-border">
            {/* Title bar — darker shade grounds the block as a 'labeled section' */}
            <div className="flex items-center justify-between border-b border-chat-border bg-chat-title px-3 py-1.5">
                <div className="flex items-center gap-1.5">
                    <button
                        onClick={() => setCollapsed(!collapsed)}
                        className="flex items-center justify-center rounded p-0.5 text-text-tertiary hover:text-text-secondary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
                        aria-label={collapsed ? "Expand code" : "Collapse code"}
                    >
                        {collapsed ? (
                            <ChevronRight className="h-3.5 w-3.5" />
                        ) : (
                            <ChevronDown className="h-3.5 w-3.5" />
                        )}
                    </button>
                    <span className="text-xs font-medium text-text-tertiary ">
                        {langLabel}
                    </span>
                </div>
                <button
                    onClick={handleCopy}
                    className="flex items-center gap-1 rounded px-1.5 py-0.5 text-xs text-text-tertiary hover:text-text-secondary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
                    aria-label={t("codeBlock.ariaLabelCopy")}
                >
                    {copied ? (
                        <>
                            <Check className="h-3 w-3" />
                            Copied
                        </>
                    ) : (
                        <>
                            <Copy className="h-3 w-3" />
                            Copy
                        </>
                    )}
                </button>
            </div>

            {/* Code content — lighter shade so the title bar above grounds the block */}
            {!collapsed && (
                <div
                    className="overflow-x-auto whitespace-pre-wrap bg-chat-body p-3 font-mono leading-relaxed"
                    style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.9)" }}
                    dangerouslySetInnerHTML={{ __html: highlighted }}
                />
            )}
        </div>
    );
}