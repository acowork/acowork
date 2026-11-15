import { readFileSync, writeFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

/** One-off retune after user feedback: the dark ladder stays EXACTLY the
 *  default lightness values (brightness identical to default — that was
 *  the "switching a theme makes the UI brighter" complaint), but tint
 *  saturation rises from zinc-level (0-10%) to a perceivable level so
 *  switching themes is visible again. Light mode gets the same treatment
 *  at a gentler strength. */
const DIR = "src/styles/themes";

/** Dark scope: [S, L] — L is the default ladder (unchanged), S is
 *  perceivable-but-calm. Semantic families (danger / menu-danger /
 *  menu-warning / highlight / shadows) are not listed and keep values. */
const DARK = {
  "--color-bg": [18, 3.9],
  "--color-page-bg": [18, 8],
  "--color-panel-block": [18, 16.7],
  "--color-panel-inset": [18, 8],
  "--color-panel-inset-2": [18, 4],
  "--color-modal-surface": [18, 10],
  "--color-editor-canvas": [18, 11.8],
  "--color-nav-surface": [18, 11.7],
  "--color-nav-item-hover": [18, 22],
  "--color-nav-divider": [15, 35],
  "--color-right-panel": [18, 11.7],
  "--color-chat-body": [18, 8],
  "--color-chat-title": [18, 13],
  "--color-chat-input-bg": [18, 11.7],
  "--color-input-bg": [18, 16.7],
  "--color-input-border": [15, 18],
  "--color-border-outer": [15, 13],
  "--color-border-divider": [15, 17],
  "--color-menu-divider": [15, 20],
  "--color-text": [8, 90],
  "--color-text-secondary": [8, 80],
  "--color-text-tertiary": [8, 60],
  "--color-text-disabled": [8, 38],
  "--color-menu-text": [12, 85],
  "--color-menu-icon": [12, 46],
  "--color-control-track": [12, 36],
  "--color-control-outline": [12, 46],
  "--color-muted": [47.4, 15],
  "--color-muted-foreground": [16.3, 65],
};

/** Light scope: saturation only (keep current L) — 5% was invisible,
 *  12% reads as a gentle tint. Text tokens stay at their neutral level. */
const LIGHT_S = 12;
const LIGHT_TEXT = /^--color-(text|menu-text)/;

for (const file of readdirSync(DIR).filter((f) => f.endsWith(".css"))) {
  const path = join(DIR, file);
  const lines = readFileSync(path, "utf8").split("\n");
  let inDark = false;
  const out = lines.map((line) => {
    if (/^:root\.theme-[a-z-]+\.dark \{/.test(line)) inDark = true;
    if (inDark && line.trim() === "}") inDark = false;
    const decl = line.match(/^\s*(--[a-zA-Z0-9-]+):/);
    if (!decl) return line;
    if (inDark) {
      const ladder = DARK[decl[1]];
      if (!ladder) return line;
      const [s, l] = ladder;
      return line.replace(
        /hsl\((\d+)\s+\d+(?:\.\d+)?%\s+\d+(?:\.\d+)?%/,
        (_, h) => `hsl(${h} ${s}% ${l}%`,
      );
    }
    // light scope: recolour surfaces only
    if (LIGHT_TEXT.test(decl[1]) || /^--color-(danger|menu-danger|menu-warning|highlight)/.test(decl[1])) {
      return line;
    }
    return line.replace(/hsl\((\d+)\s+(\d+(?:\.\d+)?)%/, (_, h) => `hsl(${h} ${LIGHT_S}%`);
  });
  writeFileSync(path, out.join("\n"), "utf8");
  console.log(`${file}: dark L=default ladder + S retuned; light S=${LIGHT_S}%`);
}
