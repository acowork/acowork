#!/usr/bin/env node
/**
 * One-shot codemod: replaces Tailwind palette classes (zinc/red/green/amber…)
 * with semantic theme tokens so a theme pack can recolour the whole app by
 * overriding CSS variables alone.
 *
 * Pass 1 (per line, token-level): a `dark:Y` token whose variants minus
 * `dark:` match a light token `X` of the same utility collapses into ONE
 * semantic class — the token auto-flips in `.dark`, so the dark variant is
 * deleted. This works even when other classes sit between X and Y (the
 * usual `bg-zinc-100 px-3 dark:bg-zinc-800` shape) and across separate
 * string literals of one cn() call.
 *
 * Pass 2: remaining single-writes map through the same ladder.
 *
 * Deliberately NOT touched:
 *   - `bg-zinc-800/900/950`, `bg-white`, `bg-black` written WITHOUT a dark:
 *     pair — cross-mode solid fills (dark primary buttons, splash art).
 *   - Standalone `dark:Y` with no light twin on the line ("darken only in
 *     dark mode").
 *   - `text-zinc-100/300` single-writes — dark-only foregrounds.
 *   - Decorative data-viz hues (indigo/violet/rose…) — content colours.
 *
 * Run with --write to apply; default is dry-run stats. Idempotent.
 */
import { readFileSync, writeFileSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, relative, resolve } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const SRC = resolve(__dirname, "..", "src");
const WRITE = process.argv.includes("--write");

const UTILITY = "bg|text|border|ring|divide|from|via|to|placeholder|decoration|outline|caret|accent|fill|stroke";
const PALETTE = "zinc|red|green|emerald|amber|yellow|blue|sky|orange";
const TOKEN_RE = new RegExp(
  `(?<![\\w:.-])((?:[a-z-]+(?::[a-z-]+)*:)?)((?:${UTILITY})-(?:white|black|(?:${PALETTE})(?:-\\d+)?)(?:/\\d+)?)(?![\\w-])`,
  "g",
);

/** key: `${utility}-${palette}-${lightShade}|${darkShade}` -> semantic suffix */
const PAIR_RULES = {
  // neutral surface ladder (zinc)
  "bg-zinc-50|900": "surface-subtle",
  "bg-zinc-100|800": "surface",
  "bg-zinc-100|900": "surface-subtle",
  "bg-zinc-200|700": "surface-hover",
  "bg-zinc-200|600": "surface-strong",
  "bg-zinc-300|600": "surface-strong",
  "bg-zinc-100|700": "surface-hover",
  "bg-zinc-400|500": "loading",
  "bg-white|800": "surface",
  "bg-white|900": "surface-subtle",
  "text-zinc-900|100": "text",
  "text-zinc-800|200": "text",
  "text-zinc-800|100": "text",
  "text-zinc-700|300": "text-secondary",
  "text-zinc-700|200": "text-secondary",
  "text-zinc-600|400": "text-tertiary",
  "text-zinc-500|400": "text-tertiary",
  "text-zinc-400|500": "text-disabled",
  "text-zinc-400|600": "text-disabled",
  "border-zinc-200|700": "border-outer",
  "border-zinc-300|600": "border-strong",
  "border-zinc-300|700": "border-strong",
  "border-zinc-400|600": "border-strong",
  "border-zinc-800|700": "border-outer",
  "ring-zinc-300|600": "border-strong",
  "ring-zinc-300|700": "border-strong",
  "ring-zinc-400|600": "border-strong",
  "divide-zinc-200|700": "border-outer",
  // danger (red)
  "bg-red-50|900": "danger-surface",
  "bg-red-100|900": "danger-surface-hover",
  "bg-red-200|900": "danger-surface-hover",
  "bg-red-500|600": "danger-solid",
  "bg-red-600|700": "danger-solid",
  "text-red-600|400": "danger-text",
  "text-red-700|400": "danger-text",
  "text-red-500|400": "danger-icon",
  "text-red-800|300": "danger-text-strong",
  "border-red-200|800": "danger-border",
  "border-red-300|700": "danger-border",
  "border-red-400|700": "danger-border",
  // success (green/emerald)
  "bg-green-50|900": "ok-surface",
  "bg-green-100|900": "ok-surface",
  "bg-emerald-50|900": "ok-surface",
  "bg-emerald-100|900": "ok-surface",
  "bg-emerald-500|600": "ok-solid",
  "bg-green-500|600": "ok-solid",
  "text-green-600|400": "ok",
  "text-green-700|400": "ok",
  "text-emerald-600|400": "ok",
  "text-emerald-700|300": "ok",
  // warning (amber/yellow/orange)
  "bg-amber-50|900": "warning-surface",
  "bg-amber-100|900": "warning-surface-hover",
  "bg-amber-200|900": "warning-surface-hover",
  "bg-amber-500|600": "warning-solid",
  "bg-yellow-50|900": "warning-surface",
  "bg-yellow-100|900": "warning-surface-hover",
  "bg-yellow-500|600": "warning-solid",
  "bg-orange-50|900": "warning-surface",
  "text-amber-500|400": "warning",
  "text-amber-600|400": "warning",
  "text-amber-700|300": "warning-quiet",
  "text-amber-800|300": "warning-quiet",
  "text-yellow-600|400": "warning",
  "text-yellow-700|400": "warning-quiet",
  "border-amber-200|700": "warning-border",
  "border-amber-200|800": "warning-border",
  "border-amber-300|600": "warning-border",
  "border-amber-300|700": "warning-border",
  "border-amber-400|700": "warning-border",
  // info (blue/sky)
  "bg-blue-50|900": "info-surface",
  "bg-sky-50|900": "info-surface",
  "bg-blue-100|900": "info-surface",
  "bg-sky-100|900": "info-surface",
  "bg-blue-500|600": "info-solid",
  "bg-sky-500|600": "info-solid",
  "text-blue-700|400": "info",
  "text-blue-600|400": "info",
  "text-sky-700|400": "info",
  "text-sky-600|400": "info",
};

/** key: `${utility}-${palette}-${shade}` -> semantic suffix */
const SINGLE_RULES = {
  "bg-zinc-50": "surface-subtle",
  "bg-zinc-100": "surface",
  "bg-zinc-200": "surface-hover",
  "bg-zinc-300": "surface-strong",
  "bg-zinc-400": "loading",
  "bg-zinc-500": "loading",
  "text-zinc-900": "text",
  "text-zinc-800": "text",
  "text-zinc-700": "text-secondary",
  "text-zinc-600": "text-tertiary",
  "text-zinc-500": "text-tertiary",
  "text-zinc-400": "text-disabled",
  "border-zinc-100": "border-outer",
  "border-zinc-200": "border-outer",
  "border-zinc-300": "border-strong",
  "border-zinc-400": "border-strong",
  "ring-zinc-300": "border-strong",
  "ring-zinc-400": "border-strong",
  "placeholder-zinc-400": "text-disabled",
  "accent-zinc-600": "text-disabled",
  "text-red-600": "danger-text",
  "text-red-700": "danger-text",
  "text-red-800": "danger-text-strong",
  "text-red-500": "danger-icon",
  "text-red-400": "danger-icon",
  "bg-red-50": "danger-surface",
  "bg-red-100": "danger-surface-hover",
  "bg-red-200": "danger-surface-hover",
  "bg-red-500": "danger-solid",
  "bg-red-600": "danger-solid",
  "bg-red-700": "danger-solid",
  "border-red-200": "danger-border",
  "border-red-300": "danger-border",
  "border-red-400": "danger-border",
  "text-green-600": "ok",
  "text-green-700": "ok",
  "text-green-500": "ok",
  "text-green-400": "ok",
  "text-emerald-600": "ok",
  "text-emerald-500": "ok",
  "text-emerald-400": "ok",
  "text-emerald-700": "ok",
  "bg-green-50": "ok-surface",
  "bg-green-100": "ok-surface",
  "bg-emerald-50": "ok-surface",
  "bg-emerald-100": "ok-surface",
  "bg-emerald-500": "ok-solid",
  "bg-green-500": "ok-solid",
  "text-amber-500": "warning",
  "text-amber-600": "warning",
  "text-amber-400": "warning",
  "text-amber-700": "warning-quiet",
  "text-amber-800": "warning-quiet",
  "text-amber-900": "warning-quiet",
  "bg-amber-50": "warning-surface",
  "bg-amber-100": "warning-surface-hover",
  "bg-amber-200": "warning-surface-hover",
  "bg-amber-500": "warning-solid",
  "bg-amber-400": "warning-solid",
  "bg-amber-600": "warning-solid",
  "border-amber-200": "warning-border",
  "border-amber-300": "warning-border",
  "border-amber-400": "warning-border",
  "border-amber-500": "warning-border",
  "text-yellow-600": "warning",
  "text-yellow-500": "warning",
  "text-yellow-700": "warning-quiet",
  "bg-yellow-50": "warning-surface",
  "bg-yellow-100": "warning-surface-hover",
  "bg-yellow-500": "warning-solid",
  "bg-yellow-400": "warning-solid",
  "text-blue-700": "info",
  "text-blue-600": "info",
  "text-blue-400": "info",
  "text-blue-800": "info",
  "text-sky-700": "info",
  "text-sky-600": "info",
  "text-sky-500": "info",
  "text-sky-900": "info",
  "bg-blue-50": "info-surface",
  "bg-sky-50": "info-surface",
  "bg-blue-100": "info-surface",
  "bg-sky-100": "info-surface",
  "bg-blue-500": "info-solid",
  "bg-blue-400": "info-solid",
  "bg-sky-500": "info-solid",
  "text-orange-400": "warning",
  "text-orange-600": "warning-quiet",
  "text-orange-700": "warning-quiet",
  "bg-orange-50": "warning-surface",
  "bg-orange-500": "warning-solid",
};

function sourceFiles(dir) {
  const out = [];
  for (const entry of readdirSync(dir)) {
    const p = join(dir, entry);
    if (statSync(p).isDirectory()) out.push(...sourceFiles(p));
    else if (/\.(tsx?|jsx?)$/.test(entry)) out.push(p);
  }
  return out;
}

function parseToken(match) {
  const variants = match[1] ?? "";
  const cls = match[2];
  const m = cls.match(/^([a-z]+)-(white|black|[a-z]+)(?:-(\d+))?(\/\d+)?$/);
  const utility = m[1];
  const palette = m[2];
  const shade = m[3] ?? "";
  const alpha = m[4] ?? "";
  const variantList = variants ? variants.replace(/:$/, "").split(":") : [];
  return {
    start: match.index,
    end: match.index + match[0].length,
    variants,
    variantList,
    utility,
    palette,
    shade,
    alpha,
    cls: `${utility}-${palette}${shade ? "-" + shade : ""}`,
    isDark: variantList.includes("dark"),
    pureVariants: variantList.filter((v) => v !== "dark").join(":"),
  };
}

const keyOf = (t) => `${t.utility}-${t.palette}${t.shade ? "-" + t.shade : ""}`;

function processLine(line) {
  const tokens = [...line.matchAll(TOKEN_RE)].map(parseToken);
  if (!tokens.length) return { line, pairs: 0, singles: 0 };
  const edits = [];
  let pairs = 0;
  let singles = 0;

  // ── Pass 1: pair each dark token with a same-utility light twin ──
  const used = new Set();
  for (const dark of tokens.filter((t) => t.isDark)) {
    const light = tokens.find(
      (t) =>
        !t.isDark &&
        !used.has(t) &&
        t.utility === dark.utility &&
        t.pureVariants === dark.pureVariants,
    );
    if (!light) continue;
    // Exact pair rules first; otherwise fall back to the LIGHT side's own
    // single-write mapping — the semantic token auto-flips in .dark, so the
    // dark twin is redundant whatever shade it picked.
    const semantic =
      PAIR_RULES[`${keyOf(light)}|${dark.shade}`] ?? SINGLE_RULES[keyOf(light)];
    if (!semantic) continue;
    used.add(light);
    pairs += 1;
    edits.push({
      start: light.start,
      end: light.end,
      text: `${light.variants}${light.utility}-${semantic}${light.alpha}`,
    });
    // delete the dark twin, swallowing one leading blank when present
    const swallow = light !== dark && line[dark.start - 1] === " " ? 1 : 0;
    edits.push({ start: dark.start - swallow, end: dark.end, text: "" });
  }

  // ── Pass 2: remaining single writes (no dark prefix, not already used) ──
  for (const tok of tokens) {
    if (tok.isDark || used.has(tok)) continue;
    const semantic = SINGLE_RULES[tok.cls];
    if (!semantic) continue;
    singles += 1;
    edits.push({
      start: tok.start,
      end: tok.end,
      text: `${tok.variants}${tok.utility}-${semantic}${tok.alpha}`,
    });
  }

  if (!edits.length) return { line, pairs, singles };
  edits.sort((a, b) => b.start - a.start);
  let out = line;
  let lastStart = Infinity;
  for (const e of edits) {
    if (e.end > lastStart) continue; // overlap guard — keep the first edit
    lastStart = e.start;
    out = out.slice(0, e.start) + e.text + out.slice(e.end);
  }
  return { line: out, pairs, singles };
}

let pairCount = 0;
let singleCount = 0;
const touched = new Map();
const leftovers = new Map();

const files = sourceFiles(SRC);
for (const file of files) {
  const before = readFileSync(file, "utf8");
  const outLines = before.split("\n").map((line) => {
    const r = processLine(line);
    pairCount += r.pairs;
    singleCount += r.singles;
    if (r.line !== line) bump(touched, relative(SRC, file));
    return r.line;
  });
  const out = outLines.join("\n");

  // leftover census (informational only)
  for (const m of out.matchAll(new RegExp(`(?<![\\w:.-])((?:[a-z-]+(?::[a-z-]+)*:)?)((?:${UTILITY})-(?:${PALETTE})-\\d+)(?![\\w-])`, "g"))) {
    bump(leftovers, `${m[1] ?? ""}${m[2]}`);
  }

  if (WRITE && out !== before) writeFileSync(file, out, "utf8");
}

function bump(map, key, n = 1) {
  map.set(key, (map.get(key) ?? 0) + n);
}

console.log(`files scanned: ${files.length}`);
console.log(`pair replacements: ${pairCount}`);
console.log(`single replacements: ${singleCount}`);
console.log(`files touched: ${touched.size}`);
if (!WRITE) console.log("(dry-run — pass --write to apply)");
console.log("\nleftover palette classes:");
for (const [cls, n] of [...leftovers].sort((a, b) => b[1] - a[1]).slice(0, 30)) {
  console.log(`  ${String(n).padStart(4)}  ${cls}`);
}
