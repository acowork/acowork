import { readFileSync, writeFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

/** One-off: rewrite the theme packs so every tint token's SATURATION is
 *  driven by the global `--theme-chroma` knob (declared in globals.css,
 *  light 12% / dark 18%). Internal proportions are preserved as ratios:
 *  surfaces 1x, borders 0.85x, icon/controls 0.7x, text 0.45x. Semantic
 *  colours (danger/warning/locate) and muted stay literal so the knob
 *  cannot wash out errors or the muted slate. */
const DIR = "src/styles/themes";

const KEEP = [
  /^--color-danger-/,
  /^--color-menu-danger/,
  /^--color-menu-warning/,
  /^--color-highlight-locate/,
  /^--shadow/,
  /^--color-muted/, // default's own slate values
];

const RATIO = [
  [/^(--color-(border|menu-divider|nav-divider|input-border))/, 0.85],
  [/^(--color-(menu-icon|control-track|control-outline))/, 0.7],
  [/^(--color-(text|menu-text))/, 0.45],
];

const ratioFor = (name) => {
  for (const [re, r] of RATIO) if (re.test(name)) return r;
  return 1; // surfaces
};

for (const file of readdirSync(DIR).filter((f) => f.endsWith(".css"))) {
  const path = join(DIR, file);
  const out = readFileSync(path, "utf8").split("\n").map((line) => {
    const decl = line.match(/^\s*(--[a-zA-Z0-9-]+):/);
    if (!decl || KEEP.some((re) => re.test(decl[1]))) return line;
    const r = ratioFor(decl[1]);
    return line.replace(
      /hsl\((\d+)\s+\d+(?:\.\d+)?%\s+(\d+(?:\.\d+)?)%\)/,
      (_, h, l) =>
        `hsl(${h} ${r === 1 ? "var(--theme-chroma)" : `calc(var(--theme-chroma) * ${r})`} ${l}%)`,
    );
  });
  writeFileSync(path, out.join("\n"), "utf8");
  console.log(`${file}: saturation now driven by --theme-chroma`);
}
