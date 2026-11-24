// CI red line: a deps-less useEffect/useLayoutEffect that setStates is a
// feedback-loop risk — it re-runs on EVERY commit, and if the value it writes
// feeds back into what it measures, a discrete event turns it into a synchronous
// render cascade (the 23s workspace-open freeze, root-caused to such an effect
// in overlayScrollThumb). Legitimate deps-less effects (identity-stable late-ref
// capture, guarded self-heal) must carry an explicit `// deps-less-ok: <reason>`
// marker on the effect line or the line above. Anything else fails CI.
//
//   node dev/check-effect-deps.mjs [root]
//
// String/comment/template-aware lexer extracts each hook call's arguments so a
// `useEffect(...)` mentioned in a comment or string is never counted.
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";

const ROOT = process.argv[2] || "apps/acowork-desktop/src";
const SETTER = /\b(set[A-Z]\w*|remeasure|forceUpdate|updateMetrics|dispatch)\s*\(/;

function callArgs(src, open) {
  let i = open, depth = 0, mode = "code", argStart = open + 1;
  const args = [], tmpl = [], n = src.length;
  while (i < n) {
    const c = src[i], nx = src[i + 1] || "";
    if (mode === "code") {
      if (c === "/" && nx === "/") { mode = "line"; i += 2; continue; }
      if (c === "/" && nx === "*") { mode = "block"; i += 2; continue; }
      if (c === "'") { mode = "sq"; i += 1; continue; }
      if (c === '"') { mode = "dq"; i += 1; continue; }
      if (c === "`") { mode = "bt"; i += 1; continue; }
      if (c === "(" || c === "[" || c === "{") depth++;
      else if (c === ")" || c === "]" || c === "}") {
        if (c === "}" && tmpl.length && depth === tmpl[tmpl.length - 1]) {
          depth--; tmpl.pop(); mode = "bt"; i += 1; continue;
        }
        depth--;
        if (depth === 0) { args.push(src.slice(argStart, i)); return args; }
      } else if (c === "," && depth === 1) { args.push(src.slice(argStart, i)); argStart = i + 1; }
      i += 1; continue;
    }
    if (mode === "line") { if (c === "\n") mode = "code"; i += 1; continue; }
    if (mode === "block") { if (c === "*" && nx === "/") { mode = "code"; i += 2; continue; } i += 1; continue; }
    if (mode === "sq") { if (c === "\\") { i += 2; continue; } if (c === "'") mode = "code"; i += 1; continue; }
    if (mode === "dq") { if (c === "\\") { i += 2; continue; } if (c === '"') mode = "code"; i += 1; continue; }
    if (mode === "bt") {
      if (c === "\\") { i += 2; continue; }
      if (c === "`") { mode = "code"; i += 1; continue; }
      if (c === "$" && nx === "{") { depth++; tmpl.push(depth); mode = "code"; i += 2; continue; }
      i += 1; continue;
    }
  }
  return args;
}

function codeMask(src) {
  const n = src.length, mask = new Uint8Array(n);
  let i = 0, mode = "code";
  while (i < n) {
    const c = src[i], nx = src[i + 1] || "";
    if (mode === "code") {
      mask[i] = 1;
      if (c === "/" && nx === "/") { mode = "line"; mask[i] = 0; i += 2; continue; }
      if (c === "/" && nx === "*") { mode = "block"; mask[i] = 0; i += 2; continue; }
      if (c === "'") { mode = "sq"; mask[i] = 0; i += 1; continue; }
      if (c === '"') { mode = "dq"; mask[i] = 0; i += 1; continue; }
      if (c === "`") { mode = "bt"; mask[i] = 0; i += 1; continue; }
      i += 1; continue;
    }
    if (mode === "line") { if (c === "\n") { mode = "code"; mask[i] = 1; } i += 1; continue; }
    if (mode === "block") { if (c === "*" && nx === "/") { mode = "code"; i += 2; continue; } i += 1; continue; }
    if (mode === "sq") { if (c === "\\") { i += 2; continue; } if (c === "'") mode = "code"; i += 1; continue; }
    if (mode === "dq") { if (c === "\\") { i += 2; continue; } if (c === '"') mode = "code"; i += 1; continue; }
    if (mode === "bt") { if (c === "\\") { i += 2; continue; } if (c === "`") mode = "code"; i += 1; continue; }
    i += 1;
  }
  return mask;
}

function* walk(dir) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) yield* walk(p);
    else if (/\.tsx?$/.test(name) && !/\.(test|d)\.tsx?$/.test(name)) yield p;
  }
}

const violations = [];
for (const p of walk(ROOT)) {
  const src = readFileSync(p, "utf8");
  const mask = codeMask(src);
  const lines = src.split("\n");
  const re = /\b(useEffect|useLayoutEffect)\s*\(/g;
  let m;
  while ((m = re.exec(src))) {
    if (!mask[m.index]) continue;
    const args = callArgs(src, m.index + m[0].length - 1);
    if (!args.length || args.length >= 2) continue; // has deps array -> not deps-less
    if (!SETTER.test(args[0])) continue;
    const lineNo = src.slice(0, m.index).split("\n").length;
    let marked = /deps-less-ok/.test(lines[lineNo - 1] || "");
    for (let j = lineNo - 2; j >= 0 && j >= lineNo - 8; j--) {
      const t = (lines[j] || "").trim();
      if (t.startsWith("//") || t === "") { if (/deps-less-ok/.test(t)) { marked = true; } continue; }
      break;
    }
    if (marked) continue;
    violations.push(`${relative(ROOT, p).replace(/\\/g, "/")}:${lineNo}  deps-less ${m[1]} setStates without a 'deps-less-ok' marker`);
  }
}

if (violations.length) {
  console.error("ERROR: deps-less effect that setStates (feedback-loop risk).");
  console.error("Give it a dependency array, or if it is a guarded identity-stable capture,");
  console.error("mark it `// deps-less-ok: <reason>` on the effect line or the line above.\n");
  for (const v of violations.sort()) console.error("  " + v);
  process.exit(1);
}
console.log("effect-deps red line: no unmarked deps-less setState effects.");