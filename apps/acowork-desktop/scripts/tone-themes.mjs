import { readFileSync, writeFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

/** One-off: set the saturation (chroma) of every tint token in the
 *  app-style theme packs to the default zinc concentration (~5%),
 *  keeping hue (theme identity) and the lightness ladder (contrast)
 *  intact. Semantic hues that must stay loud are excluded.
 *
 *  MODE:
 *    "match-default" — S becomes TARGET_S (zinc-level, like default)
 *    "ratio"         — S multiplied by RATIO
 */
const DIR = "src/styles/themes";
const MODE = "match-default";
const TARGET_S = 5; // default zinc family sits at ~4-6% saturation
const RATIO = 0.5;
const KEEP = [
  /^--color-danger-/,        // error family keeps its strength
  /^--color-menu-danger/,    // menu error label/hover
  /^--color-menu-warning/,   // menu warning label/hover
  /^--color-highlight-locate/, // locate highlight
  /^--shadow/,               // shadows keep their depth tint
];

for (const file of readdirSync(DIR).filter((f) => f.endsWith(".css"))) {
  const path = join(DIR, file);
  const lines = readFileSync(path, "utf8").split("\n");
  const out = lines.map((line) => {
    const decl = line.match(/^\s*(--[a-zA-Z0-9-]+):/);
    if (!decl || KEEP.some((re) => re.test(decl[1]))) return line;
    return line.replace(
      /hsl\((\d+)\s+(\d+(?:\.\d+)?)%/g,
      (_, h, s) => `hsl(${h} ${MODE === "match-default" ? TARGET_S : Math.round(parseFloat(s) * RATIO * 10) / 10}%`,
    );
  });
  writeFileSync(path, out.join("\n"), "utf8");
  console.log(`${file}: tint tokens set to zinc-level saturation`);
}
