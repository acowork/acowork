import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./i18n"; // i18n initialization (must run before any useTranslation call)
import "./styles/globals.css";
import { initThemePacks } from "./styles/themePacks";
import { initMonaco } from "./lib/monacoBootstrap";
import { installClipboardPatches } from "./lib/clipboard";
import { installMainThreadProbe, installLongTaskObserver } from "./lib/mainThreadProbe";
import { installAuthFetchInterceptor } from "./lib/authFetch";
import { installGatewayAuthBridge } from "./lib/gatewayAuthBridge";
import { installTokenTransport } from "./lib/session/wiring";
// ponytail: diagnostic — remove once the slow-start report is root-caused.
installMainThreadProbe();
installLongTaskObserver();

// ADR-076 §决策 3: give the session module its rotation transport BEFORE any
// consumer is installed. Both the interceptor and the bridge rotate through it;
// wiring it from a store's module side effect made them depend on import order.
installTokenTransport();

// ADR-076 §决策 3: attach the login token to every Gateway request and
// rotate it on 401. Installed before App renders so no child fetch escapes.
installAuthFetchInterceptor();

// ADR-076 §决策 3: the Rust command layer is a second HTTP client on the same
// session. Mirror the access token into it and answer its 401 renew requests
// from the store's single-flight rotation (see lib/gatewayAuthBridge.ts).
installGatewayAuthBridge();

// ═══ Bundled fonts (macOS uses -apple-system → SF Pro natively;
// Win/Linux fall through to Inter / Noto Sans SC below) ═══
//
// Inter (Latin) — close visual substitute for SF Pro, OFL-licensed.
// @fontsource ships woff2 + latin unicode-range subset per weight.
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/inter/700.css";

// Noto Sans SC Simplified Chinese subset (~7400 CJK chars).
// IMPORTANT: must use `chinese-simplified-XXX.css`, NOT the default
// `XXX.css` which only covers emoji/symbols and lacks CJK base glyphs.
// The chinese-simplified entry is a single @font-face per weight with
// no unicode-range cap, covering the full CJK Unified Ideographs base
// used by 99%+ of UI text. Source Han Sans CN / Noto Sans CJK SC is the
// upstream glyph source that PingFang SC was derived from — visually
// equivalent, legally redistributable (OFL).
import "@fontsource/noto-sans-sc/chinese-simplified-400.css";
import "@fontsource/noto-sans-sc/chinese-simplified-500.css";
import "@fontsource/noto-sans-sc/chinese-simplified-600.css";
import "@fontsource/noto-sans-sc/chinese-simplified-700.css";

// ═══ Monaco Editor bootstrap — MUST run before any component uses Monaco ═══
//
// 1. Tell @monaco-editor/react to use the locally-installed monaco-editor
//    instead of loading scripts from CDN (which may fail in Tauri's WebView).
// 2. Configure MonacoEnvironment.getWorker so that language-service workers
//    (TypeScript, JSON, CSS, HTML, editor) are resolved through Vite's
//    ?worker import pipeline rather than fetched as loose scripts.
//
// 3. navigator.clipboard NotAllowedError suppression. The patch and its
//    rationale live in lib/clipboard.ts (which also owns the copy helpers and
//    keeps the unpatched originals, so a helper can still tell a real refusal
//    from a swallowed one). Install it *before* Monaco creates its
//    BrowserClipboardService — initMonaco() below runs after this line.
// (Monaco itself is no longer imported here — see lib/monacoBootstrap.ts.
//  initMonaco() runs in the background below so the ~170 kB monaco-editor
//  bundle does not block first paint.)

installClipboardPatches();

// ═══ End Monaco bootstrap (moved to lib/monacoBootstrap.ts) ═══

// Import settingsStore early so theme is applied to DOM before first paint.
// The store initializer calls applyTheme() which toggles the .dark class
// based on the persisted preference from localStorage.
import "./stores/settingsStore";
import { useSettingsStore } from "./stores/settingsStore";

// Theme packs are discovered from styles/themes/*.css at build time. Apply the
// persisted one after settingsStore has set the .dark class, so an active pack
// always sees the correct light/dark mode on its first paint.
initThemePacks();

// Font size steps for Ctrl+/Ctrl- global shortcuts.
const FONT_SIZE_STEPS = [0.75, 0.875, 1.0, 1.125, 1.25];

// Disable native browser context menu to prevent accidental page refresh
// and other browser actions that would restart the entire app.
// Custom context menus (ChatPanel, AgentList) handle their own preventDefault.
document.addEventListener("contextmenu", (e) => e.preventDefault());

// Block native browser keyboard shortcuts so they can be redefined by the app.
//
// Architecture — two-layer design with clean module boundaries:
//
//   Layer 1 (this handler):
//     Bubble-phase listener on `window`. Only fires when the event was NOT
//     handled by any inner component. If Monaco Editor (or any other inner
//     handler) matched the keybinding, it calls stopPropagation() and the
//     event never reaches here — no explicit component detection needed.
//
//   Layer 2 (components, e.g. FileEditorPanel):
//     Register keybindings via Monaco's addCommand() API. When Ctrl+P is
//     pressed inside the editor, Monaco fires the registered handler which
//     opens the Command Palette and stops propagation.
//
// Clipboard (C/V/X/Z/A) and selection shortcuts are intentionally NOT blocked.
// F12 is NOT blocked — needed for DevTools in debug/development mode.
const BLOCKED_SHORTCUTS = new Set([
  "p", // Print
  "s", // Save page
  "f", // Browser find
  "h", // History
  "j", // Downloads
  "u", // View source
  "w", // Close tab
  "t", // New tab
  "n", // New window
  "k", // Browser search
]);

window.addEventListener("keydown", (e: KeyboardEvent) => {
  // Skip if already handled by an inner component (e.g. Monaco Editor).
  // Monaco calls preventDefault() + stopPropagation() on matched keybindings,
  // which prevents the event from bubbling here. This check covers the edge
  // case where an inner handler called preventDefault() without stopPropagation().
  if (e.defaultPrevented) return;

  // ── Global font size shortcuts: Ctrl+= / Ctrl+- ──────────────────────
  // Ctrl+= increases font size, Ctrl+- decreases. These are the same
  // shortcuts as browser zoom, repurposed for app-level font scaling.
  // Monaco Editor handles its own Ctrl+/- via internal actions, so when
  // the editor is focused, this handler won't fire (preventDefault).
  if (e.ctrlKey && !e.altKey && !e.metaKey && !e.shiftKey) {
    if (e.key === "=" || e.key === "+") {
      e.preventDefault();
      const state = useSettingsStore.getState();
      const idx = FONT_SIZE_STEPS.indexOf(state.fontSize);
      if (idx < FONT_SIZE_STEPS.length - 1) {
        state.setFontSize(FONT_SIZE_STEPS[idx + 1]);
      }
      return;
    }
    if (e.key === "-") {
      e.preventDefault();
      const state = useSettingsStore.getState();
      const idx = FONT_SIZE_STEPS.indexOf(state.fontSize);
      if (idx > 0) {
        state.setFontSize(FONT_SIZE_STEPS[idx - 1]);
      }
      return;
    }
  }

  // Block Ctrl+Shift+P — browser Print dialog (same key as VS Code Command Palette).
  if (e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey && e.key.toLowerCase() === "p") {
    e.preventDefault();
    return;
  }

  // Block Ctrl+<key> combinations (but not Ctrl+Alt, Ctrl+Meta, or Ctrl+Shift)
  if (e.ctrlKey && !e.altKey && !e.metaKey && !e.shiftKey) {
    if (BLOCKED_SHORTCUTS.has(e.key.toLowerCase())) {
      e.preventDefault();
      return;
    }
    if (e.key.toLowerCase() === "r") {
      e.preventDefault(); // Ctrl+R — page refresh
      return;
    }
  }

  // Block F5 (page refresh)
  if (e.key === "F5") {
    e.preventDefault();
  }
});

// KNOWN ISSUE (dev only, accepted for now): React.StrictMode double-invokes
// effects on first mount (mount → unmount → remount). @monaco-editor/react's
// Editor cleanup disposes the Monaco instance but keeps its internal ref
// pointing at the disposed editor, so the StrictMode remount re-runs its
// setModel() effect against a disposed instance → "InstantiationService has
// been disposed" (file editor crashes on open in dev). Production builds
// never double-mount, so release artifacts are unaffected. Proper fix would
// bypass @monaco-editor/react's Editor lifecycle (useMonaco + manual
// editor.create with ref cleanup) — deferred.
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);

// Kick off monaco-editor loading in the background — first paint no longer
// waits for the ~170 kB module graph. FileEditorPanel awaits initMonaco()
// before rendering <Editor>, so an early file-open still works. A failed load
// is logged in monacoBootstrap and retried when the editor panel next mounts,
// so swallow it here rather than raising an unhandled rejection at boot.
initMonaco().catch(() => {});

// TEMP DEBUG: capture every <a> click in capture phase (before React handlers
// run). Reports to Rust log via debug_log_link_click command. Delete once root
// cause of the <a> webview-crash is confirmed.
document.addEventListener(
  "click",
  (e) => {
    const a = (e.target as Element | null)?.closest?.("a") as HTMLAnchorElement | null;
    if (!a) return;
    const payload = JSON.stringify({
      href: a.getAttribute("href") ?? "",
      target: a.getAttribute("target") ?? "",
      defaultPrevented: e.defaultPrevented,
      eventPhase: e.eventPhase,
      outerStart: a.outerHTML.slice(0, 120),
    });
    void (window as unknown as { __TAURI__?: { core?: { invoke?: (n: string, args: unknown) => Promise<unknown> } } })
      ?.__TAURI__?.core?.invoke?.("debug_log_link_click", { payload })
      .catch(() => {});
  },
  true,
);
