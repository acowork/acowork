#!/usr/bin/env node
// scripts/check-i18n.mjs
//
// Compile-time lint for i18next translation files.
//
// Why this exists:
//   i18next default interpolation prefix/suffix is `{{` / `}}` (double braces).
//   AI-generated translations have repeatedly introduced SINGLE-brace
//   placeholders like `{count}` which i18next silently fails to interpolate
//   — they render as literal `{count}` text in the UI. To make the failure
//   loud and fast, this script enforces the double-brace convention at build
//   time and exits non-zero on any violation.
//
// Rules enforced per translation value:
//   1. `{{name}}` (double braces)  — ALLOWED
//   2. `{name}`   (single braces)  — ERROR  (most common AI mistake)
//   3. `{{{name}}}` (triple braces) — ERROR  (over-escape / typo)
//   4. Unbalanced braces like `{name` or `name}` — ERROR
//   5. Empty braces `{}` or `{{}}` — ERROR
//
// Why the key-resolution pass exists too:
//   When `t("a.b.c")` finds no key and the call passes no fallback, i18next
//   returns the KEY PATH ITSELF, which then paints the UI (e.g. a block header
//   reading `chatView.block.roundCount` instead of `3 rounds`). Placeholder
//   linting alone cannot see that, and the unit tests mock i18n and assert on
//   key paths, so neither could they. This pass resolves every statically
//   analysable call site against the locale files.
//
// Rules enforced per call site (`scripts/check-i18n.mjs` pass 2):
//   6. `t("a.b")` in src/ must resolve in en.json — en is `fallbackLng`, so a
//      miss there is what actually paints the key path. ERROR
//   7. `t("a.b")` must also resolve in zh-CN.json, else the Chinese UI silently
//      degrades to English. ERROR
//   Exemptions: call sites passing `defaultValue` or a positional fallback
//   string (they render fallback text, never the key path), and `key_one` /
//   `key_other` plural forms, which satisfy the plain key.
//   ja / ko / zh-TW are not enforced — `fallbackLng` covers them.
//
// Known limit: keys handed to `t()` through a variable (the `STATE_KEYS` map in
// components/chat/ChatExecutionBlock.tsx is one) are invisible to a static
// scan. A broader rule — "every dotted string literal in src is a key" — was
// tried and rejected: most of those literals are API field paths and filenames
// (`attachment.size`, `contextUsage.usage_percent`, `messages.jsonl`).
//
// Usage:
//   node scripts/check-i18n.mjs
//   npm run check:i18n
//
// Exit code: 0 on success, 1 on any violation.

import { readFileSync, readdirSync } from "node:fs";
import { join, relative, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

// i18n locale directory is at <projectRoot>/src/i18n/locales
const LOCALES_DIR = resolve(__dirname, "..", "src", "i18n", "locales");
const SRC_DIR = resolve(__dirname, "..", "src");

// Locales every used key must resolve in, with the reason for each.
const ENFORCED_LOCALES = [
    { file: "en.json", why: "it is fallbackLng — a miss renders the key path in the UI" },
    { file: "zh-CN.json", why: "a miss makes the Chinese UI fall back to English" },
];

// Literal call sites: `t("ns.key")` / `t('ns.key')` / `i18n.t("ns.key")`.
// `\b` keeps `it("…")`, `split("…")` and friends out of the match.
const KEY_CALL_RE = /\bt\(\s*["']([A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)+)["']/g;

/**
 * Walk a plain object recursively, calling `visit(value, path)` for every
 * leaf string value. `path` is a dot-separated key path (e.g. "time.minutesAgo").
 */
function walk(obj, path, visit) {
    if (obj === null || obj === undefined) return;
    if (typeof obj === "string") {
        visit(obj, path);
        return;
    }
    if (Array.isArray(obj)) {
        obj.forEach((item, i) => walk(item, `${path}[${i}]`, visit));
        return;
    }
    if (typeof obj === "object") {
        for (const [k, v] of Object.entries(obj)) {
            const childPath = path ? `${path}.${k}` : k;
            walk(v, childPath, visit);
        }
    }
}

/**
 * Inspect a translation value and return an array of violation messages.
 * Each message describes a single bad placeholder occurrence.
 */
function findViolations(value) {
    const errors = [];
    // Match runs of `{` and `}` so we can detect unbalanced or wrong-arity braces.
    // We scan for any sequence containing `{` and `}` and analyze the run.
    const re = /\{+[^{}]*\}+/g;
    let m;
    while ((m = re.exec(value)) !== null) {
        const token = m[0];
        const openCount = (token.match(/^\{+/) || [""])[0].length;
        const closeCount = (token.match(/\}+$/) || [""])[0].length;
        const inner = token.slice(openCount, token.length - closeCount);
        const ctx = value.slice(Math.max(0, m.index - 20), m.index + token.length + 20);

        if (openCount !== closeCount) {
            errors.push(
                `Unbalanced braces \`${token}\` (open=${openCount}, close=${closeCount}) in "${truncate(ctx)}"`,
            );
        } else if (openCount === 1) {
            errors.push(
                `Single-brace placeholder \`${token}\` is not valid i18next syntax. Use double braces: \`{{${inner.trim()}}}\`. Context: "${truncate(ctx)}"`,
            );
        } else if (openCount === 3) {
            // i18next triple-brace is for raw HTML and almost never wanted.
            errors.push(
                `Triple-brace placeholder \`${token}\` is almost certainly a typo. Use \`{{${inner.trim()}}}\`. Context: "${truncate(ctx)}"`,
            );
        } else if (openCount >= 4) {
            errors.push(
                `Quad+ brace placeholder \`${token}\` is invalid i18next syntax. Context: "${truncate(ctx)}"`,
            );
        } else if (openCount === 2) {
            // Double braces — check that the inner is a valid identifier-ish token.
            if (inner.trim().length === 0) {
                errors.push(`Empty double-brace placeholder \`{{}}\` in "${truncate(ctx)}"`);
            } else if (!/^[a-zA-Z_][a-zA-Z0-9_]*$/.test(inner.trim())) {
                errors.push(
                    `Invalid placeholder name \`{{${inner.trim()}}}\` in "${truncate(ctx)}" (expected identifier like {{count}})`,
                );
            }
        }
    }
    return errors;
}

function truncate(s, max = 60) {
    if (s.length <= max) return s;
    return s.slice(0, max - 1) + "…";
}

/**
 * Source files under `dir` that can hold call sites. Test files are skipped:
 * they mock i18n and assert on key paths on purpose, so their keys are not
 * evidence about the shipped UI.
 */
function listSourceFiles(dir, out = []) {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
        if (entry.name === "node_modules" || entry.name === "dist" || entry.name === ".vite") continue;
        const p = join(dir, entry.name);
        if (entry.isDirectory()) {
            listSourceFiles(p, out);
        } else if (/\.(ts|tsx)$/.test(entry.name) && !/\.(test|spec)\.(ts|tsx)$/.test(entry.name)) {
            out.push(p);
        }
    }
    return out;
}

/** Look up a dot-separated key path in a parsed locale object. */
function resolvePath(obj, key) {
    let cur = obj;
    for (const seg of key.split(".")) {
        if (cur === null || typeof cur !== "object") return undefined;
        cur = cur[seg];
    }
    return cur;
}

/** A key resolves if the plain form or either plural form is present. */
function isPresent(obj, key) {
    return (
        resolvePath(obj, key) !== undefined ||
        resolvePath(obj, `${key}_one`) !== undefined ||
        resolvePath(obj, `${key}_other`) !== undefined
    );
}

/**
 * Argument text of the call whose `(` is at `openIdx` — paren-balanced, with
 * string literals skipped so a `)` inside a fallback string cannot end the
 * window early. Falls back to a bounded window if the scan never balances.
 */
function argText(text, openIdx) {
    let depth = 0;
    for (let i = openIdx; i < text.length; i++) {
        const ch = text[i];
        if (ch === '"' || ch === "'" || ch === "`") {
            i = closeQuoteIdx(text, i, ch);
            continue;
        }
        if (ch === "(") depth++;
        else if (ch === ")") {
            depth--;
            if (depth === 0) return text.slice(openIdx + 1, i);
        }
    }
    return text.slice(openIdx + 1, openIdx + 400);
}

/** Index of the closing quote of the literal opening at `i`. */
function closeQuoteIdx(text, i, quote) {
    for (let j = i + 1; j < text.length; j++) {
        if (text[j] === "\\") j++;
        else if (text[j] === quote) return j;
    }
    return text.length;
}

/**
 * Whether the call passes its own fallback text — `defaultValue:` in the
 * options object, or a positional string default (`t(key, "Fallback")`).
 * Such call sites render the fallback, never the key path, so they are exempt.
 */
function hasFallback(args) {
    return /(^|[{,;\s])defaultValue\s*:/.test(args) || /^\s*,\s*["'`]/.test(args);
}

/**
 * Collect every literal `t("ns.key")` in src/: key -> first file seen plus a
 * `hard` flag. `hard` is AND-semantics across call sites — one unguarded use
 * is enough to paint the key path, even if other uses pass a default.
 */
function collectUsedKeys() {
    const used = new Map();
    for (const file of listSourceFiles(SRC_DIR)) {
        const text = readFileSync(file, "utf8");
        KEY_CALL_RE.lastIndex = 0;
        let m;
        while ((m = KEY_CALL_RE.exec(text)) !== null) {
            const key = m[1];
            const parenIdx = text.indexOf("(", m.index);
            const entry = used.get(key) || { file: relative(process.cwd(), file), hard: false };
            if (!hasFallback(argText(text, parenIdx))) entry.hard = true;
            used.set(key, entry);
        }
    }
    return used;
}

/**
 * Pass 2: every hard-used key must resolve in the enforced locales.
 * Returns report lines, one per violation.
 */
function checkKeyResolution(parsedLocales) {
    const used = collectUsedKeys();
    const errors = [];
    let missingTotal = 0;
    for (const [key, entry] of [...used.entries()].sort(([a], [b]) => a.localeCompare(b))) {
        if (!entry.hard) continue;
        for (const { file, why } of ENFORCED_LOCALES) {
            const locale = parsedLocales.get(file);
            if (!locale || isPresent(locale, key)) continue;
            missingTotal++;
            errors.push(`  - ${key}: not found in ${file} (${why}) — used at ${entry.file}`);
        }
    }
    return { checked: used.size, missing: missingTotal, errors };
}

function main() {
    const files = readdirSync(LOCALES_DIR).filter((f) => f.endsWith(".json"));
    if (files.length === 0) {
        console.error(`[check-i18n] No JSON files found in ${LOCALES_DIR}`);
        process.exit(1);
    }

    let totalErrors = 0;
    const reportLines = [];
    const parsedLocales = new Map();

    for (const file of files) {
        const filePath = join(LOCALES_DIR, file);
        let data;
        try {
            data = JSON.parse(readFileSync(filePath, "utf8"));
        } catch (e) {
            console.error(`[check-i18n] Failed to parse ${relative(process.cwd(), filePath)}: ${e.message}`);
            process.exit(1);
        }
        parsedLocales.set(file, data);

        const fileErrors = [];
        walk(data, "", (value, path) => {
            const violations = findViolations(value);
            for (const v of violations) {
                fileErrors.push(`  - ${path}: ${v}`);
            }
        });

        if (fileErrors.length > 0) {
            totalErrors += fileErrors.length;
            reportLines.push(`\n[${relative(process.cwd(), filePath)}] ${fileErrors.length} placeholder violation(s):`);
            reportLines.push(...fileErrors);
        }
    }

    // Pass 2 — keys used by the UI must actually exist.
    const resolution = checkKeyResolution(parsedLocales);
    if (resolution.missing > 0) {
        totalErrors += resolution.missing;
        reportLines.push(`\n[${resolution.checked} t() keys scanned] ${resolution.missing} unresolved key(s):`);
        reportLines.push(...resolution.errors);
    }

    if (totalErrors === 0) {
        console.log(
            `[check-i18n] OK — ${resolution.checked} scanned call sites resolve in ${ENFORCED_LOCALES
                .map((l) => l.file)
                .join(" + ")}, and all ${files.length} locale files use valid i18next double-brace placeholders.`,
        );
        process.exit(0);
    }

    console.error("\n[check-i18n] FAILED — i18n errors detected:");
    console.error(reportLines.join("\n"));
    console.error(`\nTotal: ${totalErrors} violation(s) across ${files.length} locale file(s).`);
    console.error("\nFix: i18next requires DOUBLE braces `{{name}}`, not single `{name}`.");
    console.error("Single braces render as literal text and (if followed by uppercase CSS) may look like uppercase placeholders.");
    console.error("An unresolved key renders the key path itself — add the translation, or pass `defaultValue`.");
    process.exit(1);
}

main();
