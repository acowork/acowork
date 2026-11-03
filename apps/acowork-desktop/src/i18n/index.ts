import i18n from "i18next";
import { initReactI18next } from "react-i18next";

import en from "./locales/en.json";
import zhCN from "./locales/zh-CN.json";
import zhTW from "./locales/zh-TW.json";
import ja from "./locales/ja.json";
import ko from "./locales/ko.json";
import { log } from "../lib/logger";
import { readPref, writePref, onScopeChange } from "../lib/prefsScope";

/** Logical key — UI language is a user-level preference, so `prefsScope`
 *  namespaces it per account (the account record carries its own `language`
 *  field, but the UI choice is client-side and applied on next boot). */
const STORAGE_KEY = "i18nextLng";

const SUPPORTED_LANGS = ["en", "zh-CN", "zh-TW", "ja", "ko"];

const resources = {
    en: { translation: en },
    "zh-CN": { translation: zhCN },
    "zh-TW": { translation: zhTW },
    ja: { translation: ja },
    ko: { translation: ko },
};

/**
 * Walk a translation resource and return all single-brace placeholder violations.
 *
 * Why: i18next default interpolation prefix/suffix is `{{` / `}}` (double braces).
 * A single-brace placeholder like `{count}` is silently ignored by i18next and
 * rendered as literal text. AI-generated translations have repeatedly made this
 * mistake; this check surfaces the issue in the dev console immediately, so a
 * developer sees it the first time the app loads after the bad change.
 *
 * The same check is also enforced at build time by `scripts/check-i18n.mjs`,
 * which fails the build with a non-zero exit code. This runtime check is the
 * dev-loop safety net that catches the bug before the next build.
 */
function findSingleBraceViolations(obj: unknown, fileLabel: string, path = ""): Array<{ file: string; key: string; token: string; value: string }> {
    const out: Array<{ file: string; key: string; token: string; value: string }> = [];
    if (obj === null || obj === undefined) return out;
    if (typeof obj === "string") {
        // Match a single-brace identifier NOT nested inside `{{` or `}}`.
        // Negative-lookbehind/lookahead for `{` and `}` isolates the bare case.
        const re = /(?<!\{)\{[a-zA-Z_][a-zA-Z0-9_]*\}(?!\})/g;
        let m: RegExpExecArray | null;
        while ((m = re.exec(obj)) !== null) {
            out.push({ file: fileLabel, key: path, token: m[0], value: obj });
        }
        return out;
    }
    if (Array.isArray(obj)) {
        obj.forEach((item, i) => {
            out.push(...findSingleBraceViolations(item, fileLabel, `${path}[${i}]`));
        });
        return out;
    }
    if (typeof obj === "object") {
        for (const [k, v] of Object.entries(obj as Record<string, unknown>)) {
            const childPath = path ? `${path}.${k}` : k;
            out.push(...findSingleBraceViolations(v, fileLabel, childPath));
        }
    }
    return out;
}

function runRuntimeI18nLint(): void {
    // Vite exposes import.meta.env.DEV; also guard via NODE_ENV for safety.
    const isDev = (import.meta as ImportMeta & { env?: { DEV?: boolean } }).env?.DEV
        ?? (typeof process !== "undefined" && process.env?.NODE_ENV !== "production");
    if (!isDev) return;
    const bundles: Array<[string, unknown]> = [
        ["en", en],
        ["zh-CN", zhCN],
        ["zh-TW", zhTW],
        ["ja", ja],
        ["ko", ko],
    ];
    const violations = bundles.flatMap(([label, bundle]) =>
        findSingleBraceViolations(bundle, label),
    );
    if (violations.length === 0) {
        log.debug(`[i18n-lint] OK — all 5 locale files use valid i18next double-brace placeholders.`);
        return;
    }
    console.groupCollapsed(
        `%c[i18n-lint] FAILED — ${violations.length} single-brace placeholder violation(s) detected. ` +
        `Run \`npm run check:i18n\` to see details.`,
        "color: #ef4444; font-weight: bold;",
    );
    for (const v of violations) {
        log.warn(
            `[i18n-lint] ${v.file} > ${v.key}: single-brace placeholder \`${v.token}\` ` +
            `in "${v.value.slice(0, 80)}${v.value.length > 80 ? "…" : ""}". ` +
            `Use \`{{${v.token.slice(1, -1)}}}\` instead.`,
        );
    }
    console.groupEnd();
}

runRuntimeI18nLint();

// Detect initial language: localStorage > navigator > fallback
function detectLanguage(): string {
    const stored = readPref(STORAGE_KEY);
    if (stored && SUPPORTED_LANGS.includes(stored)) return stored;
    const navLang = navigator.language;
    // Match exact or prefix (e.g. "zh-CN", "zh-TW", "ja", "ko")
    if (SUPPORTED_LANGS.includes(navLang)) return navLang;
    if (navLang.startsWith("zh-T")) return "zh-TW";
    if (navLang.startsWith("zh")) return "zh-CN";
    if (navLang.startsWith("ja")) return "ja";
    if (navLang.startsWith("ko")) return "ko";
    return "en";
}

const initialLang = detectLanguage();
log.debug("[i18n] Detected initial language:", initialLang);

i18n
    .use(initReactI18next)
    .init({
        resources,
        lng: initialLang,
        fallbackLng: "en",
        supportedLngs: SUPPORTED_LANGS,
        interpolation: {
            escapeValue: false,
        },
    })
    .then(() => {
        log.debug("[i18n] Initialized, current language:", i18n.language);
    });

i18n.on("languageChanged", (lng) => {
    log.debug("[i18n] Language changed to:", lng);
    writePref(STORAGE_KEY, lng);
});

// A login that does not reload the page must also re-apply the new account's
// language, or the UI stays in the previous user's language for the rest of
// the session. Guarded against a no-op change so persisting the same value
// back under the new scope is the only write this causes.
onScopeChange(() => {
    const next = detectLanguage();
    if (next !== i18n.language) void i18n.changeLanguage(next);
});

export default i18n;
