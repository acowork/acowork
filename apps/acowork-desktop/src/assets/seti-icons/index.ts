/**
 * Seti UI file icons — MIT licensed SVG icons from https://github.com/jesseweed/seti-ui
 * Exception: pptx.svg is from the file-icons set (ISC, John Gardner) — Seti has no presentation icon.
 * Imported as raw strings via Vite's ?raw suffix for inline SVG rendering.
 */

import rust from "./rust.svg?raw";
import typescript from "./typescript.svg?raw";
import react from "./react.svg?raw";
import javascript from "./javascript.svg?raw";
import html from "./html.svg?raw";
import css from "./css.svg?raw";
import sass from "./sass.svg?raw";
import less from "./less.svg?raw";
import json from "./json.svg?raw";
import xml from "./xml.svg?raw";
import markdown from "./markdown.svg?raw";
import defaultIcon from "./default.svg?raw";
import shell from "./shell.svg?raw";
import python from "./python.svg?raw";
import go from "./go.svg?raw";
import go2 from "./go2.svg?raw";
import java from "./java.svg?raw";
import kotlin from "./kotlin.svg?raw";
import swift from "./swift.svg?raw";
import ruby from "./ruby.svg?raw";
import php from "./php.svg?raw";
import c from "./c.svg?raw";
import cpp from "./cpp.svg?raw";
import csharp from "./c-sharp.svg?raw";
import fsharp from "./f-sharp.svg?raw";
import docker from "./docker.svg?raw";
import eslint from "./eslint.svg?raw";
import image from "./image.svg?raw";
import svg from "./svg.svg?raw";
import pdf from "./pdf.svg?raw";
import settings from "./settings.svg?raw";
import lock from "./lock.svg?raw";
import db from "./db.svg?raw";
import yml from "./yml.svg?raw";
import powershell from "./powershell.svg?raw";
import windows from "./windows.svg?raw";
import git from "./git.svg?raw";
import gitIgnore from "./git_ignore.svg?raw";
import csv from "./csv.svg?raw";
import config from "./config.svg?raw";
import video from "./video.svg?raw";
import audio from "./audio.svg?raw";
import zip from "./zip.svg?raw";
import folder from "./folder.svg?raw";
import license from "./license.svg?raw";
import makefile from "./makefile.svg?raw";
import npm from "./npm.svg?raw";
import webpack from "./webpack.svg?raw";
import vue from "./vue.svg?raw";
import vite from "./vite.svg?raw";
import tsconfig from "./tsconfig.svg?raw";
import favicon from "./favicon.svg?raw";
import xls from "./xls.svg?raw";
import word from "./word.svg?raw";
// Not from Seti: file-icons set (ISC, John Gardner) — the MS Office silhouette for PowerPoint.
import pptx from "./pptx.svg?raw";

/** Map of icon name → raw SVG string */
export const SETI_ICONS: Record<string, string> = {
    rust,
    typescript,
    react,
    javascript,
    html,
    css,
    sass,
    less,
    json,
    xml,
    markdown,
    default: defaultIcon,
    shell,
    python,
    go,
    go2,
    java,
    kotlin,
    swift,
    ruby,
    php,
    c,
    cpp,
    "c-sharp": csharp,
    "f-sharp": fsharp,
    docker,
    eslint,
    image,
    svg,
    pdf,
    settings,
    lock,
    db,
    yml,
    powershell,
    windows,
    git,
    git_ignore: gitIgnore,
    csv,
    config,
    video,
    audio,
    zip,
    folder,
    license,
    makefile,
    npm,
    webpack,
    vue,
    vite,
    tsconfig,
    favicon,
    xls,
    word,
    pptx,
};
