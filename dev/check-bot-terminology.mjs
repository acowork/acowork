#!/usr/bin/env node
// dev/check-bot-terminology.mjs — T7 terminology guardrails (agent -> bot).
//
// Authority: checklist `agent-to-bot-术语统一修改清单.md` v6 §6 "建议的 CI 护栏"
// plus decisions D6 (.bot extension), D7 (latin "Bot" in every locale) and
// D8 (budget mode). Invoked from dev/ci.sh redlines, one subcommand per
// assertion:
//
//   node dev/check-bot-terminology.mjs locales   # assertion 1
//   node dev/check-bot-terminology.mjs msgids    # assertion 2
//   node dev/check-bot-terminology.mjs site      # assertion 3
//   node dev/check-bot-terminology.mjs desktop   # assertion 5 (classes A+B)
//
// D8 budget mode: every counted check asserts `actual <= BASELINE`. Baselines
// are hard-coded constants below, measured at landing (2026-11-26). A baseline
// may only be LOWERED as rename work lands; raising one is a regression and
// fails the gate. T8 zeroes them all.
//
// Whitelist rule (D8, "白名单不得成为后门"): every entry carries a fixed
// category and a reason comment:
//   ① code identifier (agent_id, {{agent}} placeholders, API paths, file names)
//   ② third-party / ecosystem name (incl. the SEO transitional search terms)
//   ③ historical record (docs/adr per D5, docs/research)
//   ④ real log output (text the shipped binaries themselves print)
//   ⑤ internal part (docs/_internal, docs/review, docs/prototypes, docs/prompt-audit)
// ANY new whitelist entry must be justified in the PM activity log of the card
// that adds it — who added it, why, which D decision it implements. An entry
// without an activity trail is a backdoor, not a whitelist.

import { readFileSync, existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const read = (p) => readFileSync(p.startsWith('/') || /^[A-Za-z]:/.test(p) ? p : join(ROOT, p), 'utf8');

function fail(lines) {
    for (const l of lines) console.error(l);
    process.exit(1);
}

function gitFiles(...pathspecs) {
    return execFileSync('git', ['-C', ROOT, 'ls-files', '-z', '--', ...pathspecs], { maxBuffer: 64 * 1024 * 1024 })
        .toString('utf8').split('\0').filter(Boolean);
}

// ─────────────────────────────────────────────────────────────────────────
// Assertion 1 — desktop locale VALUES must not carry the retired term.
// Caliber: occurrences of /agent/gi inside the values of the five locale
// JSONs (keys are code and NOT counted), after removing whitelist patterns.
// Measured at landing (2026-11-26, after T1): 0 in every file.
// ─────────────────────────────────────────────────────────────────────────
const LOCALE_AGENT_BASELINE = 0;

const LOCALE_WHITELIST = [
    // ① code identifier — reverse-domain example id; the checklist §2.1 row
    //    `createWizard.fieldAgentIdHint / cloneDialog.newAgentIdPlaceholder`
    //    says explicitly "不变" (example-ID form, not terminology).
    [/com\.example\.myagent/g, 'example package id (checklist §2.1)'],
    // ① code identifier — interpolation placeholder NAMES ({{agent}},
    //    {{agentId}}): template variables resolved by t(key, vars). Renaming
    //    them is a call-site change, outside the copy layer (checklist §1.2).
    [/\{\{\s*agent(?:Id)?\s*\}\}/g, 'interpolation placeholder name'],
];

function checkLocales() {
    const files = ['en', 'zh-CN', 'zh-TW', 'ja', 'ko'];
    const offenders = [];
    let total = 0;
    for (const f of files) {
        const path = `apps/acowork-desktop/src/i18n/locales/${f}.json`;
        if (!existsSync(join(ROOT, path))) fail([`ERROR: locale file missing: ${path} — a guard over a missing file passes vacuously.`]);
        const j = JSON.parse(read(path));
        (function walk(o, p) {
            for (const [k, v] of Object.entries(o)) {
                const kp = p ? `${p}.${k}` : k;
                if (typeof v === 'string') {
                    let s = v;
                    for (const [re] of LOCALE_WHITELIST) s = s.replace(re, '');
                    const n = (s.match(/agent/gi) || []).length;
                    if (n > 0) { offenders.push(`  ${f}.json ${kp} = ${JSON.stringify(v)}`); total += n; }
                } else if (v && typeof v === 'object') walk(v, kp);
            }
        })(j, '');
    }
    if (total > LOCALE_AGENT_BASELINE) {
        fail([
            `ERROR: desktop locale values carry the retired "agent" term: ${total} hits`,
            `       (budget: LOCALE_AGENT_BASELINE = ${LOCALE_AGENT_BASELINE}).`,
            ...offenders,
            '       User-visible copy says "Bot" (D4/D7). Fix the copy, or add a',
            '       whitelist entry WITH a PM activity trail (see file header).',
        ]);
    }
    console.log(`desktop locale bot-term red line: OK (${total} <= ${LOCALE_AGENT_BASELINE})`);
}

// ─────────────────────────────────────────────────────────────────────────
// Assertion 2 — mobile msgids key sets must match exactly.
// Mobile i18n uses the zh-CN source string AS the key (gettext style), so a
// key present in one language section but missing in another silently falls
// back to Simplified Chinese in the shipped UI (checklist §5.4 — the highest-
// structural risk of the rename). zh-CN itself is the key set, not a section.
// This is a structural check: any diff is red, no budget.
// ─────────────────────────────────────────────────────────────────────────
function checkMsgids() {
    const path = 'apps/acowork-mobile/src/i18n/msgids.ts';
    const src = read(path);
    const decl = src.indexOf('export const MSGIDS');
    if (decl < 0) fail([`ERROR: ${path} no longer declares MSGIDS — the key-set guard lost its anchor; update this check deliberately, never silently.`]);
    const ob = src.indexOf('{', decl);
    let depth = 0, end = -1;
    for (let i = ob; i < src.length; i++) {
        if (src[i] === '{') depth++;
        else if (src[i] === '}') { depth--; if (depth === 0) { end = i; break; } }
    }
    if (end < 0) fail([`ERROR: cannot brace-match the MSGIDS literal in ${path}`]);
    const obj = Function(`return ${src.slice(ob, end + 1)}`)();
    const langs = Object.keys(obj);
    if (langs.length < 4) fail([`ERROR: expected >= 4 language sections in MSGIDS, found ${langs.length} (${langs.join(',')})`]);
    const base = Object.keys(obj[langs[0]]);
    if (base.length === 0) fail([`ERROR: MSGIDS section "${langs[0]}" is empty — vacuous guard.`]);
    const diffs = [];
    for (const l of langs) {
        const keys = new Set(Object.keys(obj[l]));
        const missing = base.filter((k) => !keys.has(k));
        const extra = [...keys].filter((k) => !base.includes(k));
        if (missing.length || extra.length) {
            diffs.push(`  ${l}: missing ${missing.length} / extra ${extra.length}`);
            missing.slice(0, 10).forEach((k) => diffs.push(`    MISSING: ${JSON.stringify(k)}`));
            extra.slice(0, 10).forEach((k) => diffs.push(`    EXTRA:   ${JSON.stringify(k)}`));
        }
    }
    if (diffs.length) {
        fail([
            'ERROR: mobile msgids.ts language sections have divergent key sets.',
            '       A key missing from one language renders the zh-CN source string',
            '       in that locale (silent language mixing, checklist §5.4).',
            '       Fix msgids.ts so every section carries the identical key set.',
            ...diffs,
        ]);
    }
    console.log(`mobile msgids key-set red line: OK (${langs.length} sections x ${base.length} keys, identical)`);
}

// ─────────────────────────────────────────────────────────────────────────
// Assertion 3 — website / hub site/ user-visible copy whitelist check.
// Caliber: occurrences of /agent/gi in the user-visible files after stripping
// comments (comments are not user-visible, whitelist category per assertion-5
// convention) and removing whitelist patterns carrying a fixed category and
// reason. Measured at landing (2026-11-26, after T3 + T6 site commits): 0.
//
// The two repos are siblings of the main checkout and are NOT present on a
// GitHub runner (CI checks out ACowork only). The leg SKIPs loudly when a
// repo is absent — it guards the multi-repo dev machines, where the platform
// is actually assembled; the repos' own CIs own their enforcement.
// ─────────────────────────────────────────────────────────────────────────
const SITE_AGENT_BASELINE = 0;

const SITE_WHITELIST = [
    // ① code identifier — repository name; D1 renames the DISPLAY name only.
    [/ACoworkAgentHub/g, 'repository name (D1: repo name preserved)'],
    // ① code identifier — upstream file name (contributor contract doc).
    [/AGENTS\.md/g, 'upstream file name'],
    // ① code identifier — directory layout / API paths / real commands users
    //    copy-paste (checklist §1.2 + §5.3: commands must stay truthful).
    [/agents\/[A-Za-z0-9_<>\/-]*/g, 'agents/ layout + /agents/ API paths'],
    // ① code identifier — JSON/JS field and variable names.
    [/agent_id|agentId|agent_instance_id/g, 'field/variable names'],
    [/\.agents\b|doc\.agent\b/g, 'JS property access'],
    // ① code identifier — command placeholder inside `cp -r agents/_template
    //    agents/<owner>/<your-agent>` (checklist §4.1: 命令保留).
    [/your-agent/g, 'command placeholder in a real command'],
    // ④ real log output — the literal the shipped acowork-node binary prints
    //    (acowork-node/src/control/mod.rs); rewriting it in the docs would
    //    make the site disagree with the user's own terminal.
    [/Node Agent starting/g, 'real log output (acowork-node)'],
    // ④ real log output — printed by the Gateway (gateway/node_manager.rs);
    //    named verbatim in the T7 card's whitelist category ④.
    [/Local node agent spawned/g, 'real log output (gateway)'],
    // ② ecosystem/SEO search term — checklist §5.2 decided to keep "AI
    //    agents" as a transitional search term during the rename. Matched
    //    term-by-term so a reordered keyword list cannot smuggle new copy.
    [/AI bots \(AI agents\)/g, 'SEO transitional (checklist §5.2)'],
    [/AI agents/g, 'SEO transitional keyword (checklist §5.2)'],
    [/multi-agent/g, 'SEO transitional keyword (checklist §5.2)'],
];

function stripComments(text, kind) {
    let t = text;
    if (kind === 'html' || kind === 'svg') t = t.replace(/<!--[\s\S]*?-->/g, '');
    if (kind === 'js') t = t.replace(/\/\*[\s\S]*?\*\//g, '');
    // full-line comments only: `#` in header files, `//` in js, `*`/`/*` leftovers.
    // (Markdown has no comment syntax; its lines are matched by the whitelist.)
    t = t.split('\n').filter((l) => !/^\s*(\/\/|\/\/\/|#|\*|\/\*)/.test(l)).join('\n');
    return t;
}

function checkSite() {
    const website = process.env.ACOWORK_WEBSITE_DIR || join(ROOT, '..', 'ACoworkWebSite');
    const hub = process.env.ACOWORK_HUB_DIR || join(ROOT, '..', 'ACoworkAgentHub');
    let total = 0;
    const offenders = [];
    const plan = [
        [website, 'index.html', 'html'],
        [website, 'README.md', 'md'],
        [website, 'assets/architecture.svg', 'svg'],
        [hub, 'site/index.html', 'html'],
        [hub, 'site/hub.js', 'js'],
        [hub, 'site/_headers', 'headers'],
    ];
    for (const [dir, rel, kind] of plan) {
        if (!existsSync(dir)) {
            console.log(`SKIP site leg: ${dir} is not checked out here (GitHub CI checks out ACowork only; the site repos' own CI owns enforcement there).`);
            continue;
        }
        const p = join(dir, rel);
        if (!existsSync(p)) { offenders.push(`  missing ${p} — the site layout moved; update this check deliberately.`); continue; }
        let t = stripComments(read(p), kind);
        for (const [re] of SITE_WHITELIST) t = t.replace(re, '');
        const hits = t.match(/agent/gi) || [];
        if (hits.length) {
            total += hits.length;
            for (const line of t.split('\n')) {
                const m = line.match(/agent/gi);
                if (m) offenders.push(`  ${p}: ${line.trim().slice(0, 120)}`);
            }
        }
    }
    if (total > SITE_AGENT_BASELINE) {
        fail([
            `ERROR: website/hub user-visible copy carries the retired "agent" term: ${total} hits`,
            `       (budget: SITE_AGENT_BASELINE = ${SITE_AGENT_BASELINE}).`,
            ...offenders,
            '       D1/D4/D7: user-visible copy says "Bot". Fix the copy or add a',
            '       whitelist entry WITH a PM activity trail (see file header).',
        ]);
    }
    console.log(`website/hub site copy red line: OK (${total} <= ${SITE_AGENT_BASELINE})`);
}

// ─────────────────────────────────────────────────────────────────────────
// Assertion 5 — desktop `.agent` literals, class A + class B.
// Scan scope: tracked files under apps/acowork-desktop/src (ts/tsx) and
// apps/acowork-desktop/src-tauri/src (rs).
//
// Class A — extension PRODUCERS (picker filters, client-side checks, file
//   name assembly, gate constants). Any hit is a functional break waiting
//   to happen (the T5 post-mortem: picker filters that refuse .bot files).
//   Budget: 0, whitelist empty.
// Class B — the literal `.agent` inside user-visible strings (error
//   messages, JSX copy, generated package templates). Budget at landing:
//   1 — see the constant below.
//
// Whitelist (per the T7 card; each entry must carry its category):
//   - test fixtures (*.test.ts/tsx, #[cfg(test)] rs modules): test data,
//     not user-visible.
//   - pure comment lines (// /// /* * #): not user-visible.
//   - com.example.myagent (① example id).
//   - locale JSON values: NOT scanned here — assertion 1 owns them.
// ─────────────────────────────────────────────────────────────────────────
const DESKTOP_EXT_CLASS_A_BASELINE = 0;

// Class B baseline — measured 2026-11-26 at landing: 1.
// The single hit is a REGRESSION that arrived after T5 shipped:
//   apps/acowork-desktop/src-tauri/src/commands/agent_library.rs:709
//   `format!("Not an .agent package: {package_path}")` in the new
//   `upgrade_local_agent` command, introduced by 9a5d18b4 (2026-11-26).
// The T7 card forbids touching business code ("本卡只加检查"), so the
// budget records the measured value; the fix is a one-word string change
// and this baseline MUST go to 0 (T8 zeroes it). Do not raise it.
const DESKTOP_AGENT_LITERAL_CLASS_B_BASELINE = 1;

const CLASS_A_PATTERNS = [
    // file-picker filter arrays: extensions: ["agent"] / ['agent']
    [/extensions:\s*\[[^\]]*['"]agent['"]/, 'picker filter still offers the retired extension'],
    // client-side drop validation
    [/\.endsWith\(\s*['"]\.?agent['"]\s*\)/, 'client-side extension check on the retired literal'],
    // temp/zip name assembly in src-tauri
    [/\.with_extension\(\s*['"]agent['"]\s*\)/, 'file name built with the retired extension'],
    // gate constant definition: const PACKAGE_EXT: &str = "agent";
    [/PACKAGE_EXT[^=\n]*=\s*['"]agent['"]/, 'package-extension gate constant'],
    // format-string file assembly: format!("{}.agent", id) / join(format!("{agent_id}.agent")
    [/format!\(\s*["'][^"']*\.agent\\?["']/, 'file name assembled with the retired extension'],
];

const CLASS_B_RE = /(?:^|[^A-Za-z0-9_.-])\\?\.agent\b/;
const CLASS_B_WHITELIST = [
    // ① code identifier — example package id form.
    [/com\.example\.myagent/g, 'example package id'],
];

function checkDesktop() {
    const files = gitFiles('apps/acowork-desktop/src', 'apps/acowork-desktop/src-tauri/src')
        .filter((f) => /\.(ts|tsx|rs)$/.test(f));
    const hitsA = [];
    const hitsB = [];
    for (const f of files) {
        const isTestFile = /\.(test|spec)\.(ts|tsx)$/.test(f);
        let inRustTestModule = false;
        const lines = read(f).split('\n');
        for (let i = 0; i < lines.length; i++) {
            const line = lines[i];
            const n = i + 1;
            if (f.endsWith('.rs') && /^\s*#\[cfg\(test\)\]/.test(line)) inRustTestModule = true; // test fixtures: whitelist
            if (/^\s*(\/\/|\/\/\/|\/\*|\*|#)/.test(line)) continue; // pure comment: whitelist
            if (isTestFile || inRustTestModule) continue; // test fixture: whitelist
            for (const [re, why] of CLASS_A_PATTERNS) {
                if (re.test(line)) hitsA.push(`  ${f}:${n}: ${line.trim().slice(0, 120)}\n      -> ${why}`);
            }
            if (!isTestFile) {
                let s = line;
                for (const [re] of CLASS_B_WHITELIST) s = s.replace(re, '');
                if (CLASS_B_RE.test(s)) hitsB.push(`  ${f}:${n}: ${line.trim().slice(0, 120)}`);
            }
        }
    }
    if (hitsA.length > DESKTOP_EXT_CLASS_A_BASELINE) {
        fail([
            `ERROR: desktop code produces the retired .agent extension again: ${hitsA.length} hits`,
            `       (budget: DESKTOP_EXT_CLASS_A_BASELINE = ${DESKTOP_EXT_CLASS_A_BASELINE}).`,
            '       Packages ship as .bot (D6). A picker filter or gate still saying',
            '"agent" makes local import unusable — see the T5 post-mortem.',
            ...hitsA,
        ]);
    }
    if (hitsB.length > DESKTOP_AGENT_LITERAL_CLASS_B_BASELINE) {
        fail([
            `ERROR: user-visible desktop strings carry the .agent literal again: ${hitsB.length} hits`,
            `       (budget: DESKTOP_AGENT_LITERAL_CLASS_B_BASELINE = ${DESKTOP_AGENT_LITERAL_CLASS_B_BASELINE}).`,
            ...hitsB,
            '       The known budgeted hit is agent_library.rs:709 (regression from',
            '       9a5d18b4, tracked for T8). A NEW one is a regression: fix the',
            '       copy, never raise the baseline.',
        ]);
    }
    console.log(`desktop .agent literal red line: OK (A: ${hitsA.length} <= ${DESKTOP_EXT_CLASS_A_BASELINE}, B: ${hitsB.length} <= ${DESKTOP_AGENT_LITERAL_CLASS_B_BASELINE})`);
}

const CHECKS = { locales: checkLocales, msgids: checkMsgids, site: checkSite, desktop: checkDesktop };
const which = process.argv[2];
if (!CHECKS[which]) {
    console.error(`usage: node dev/check-bot-terminology.mjs <${Object.keys(CHECKS).join('|')}>`);
    process.exit(2);
}
CHECKS[which]();
