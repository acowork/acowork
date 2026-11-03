/**
 * File icon mappings — uses Seti UI icons (MIT license)
 * Source: https://github.com/jesseweed/seti-ui
 *
 * All colors are controlled by the SetiIcon component's theme-aware color maps
 * (DARK_COLORS / LIGHT_COLORS). The `color` prop is only used for rare overrides.
 */

interface FileIconInfo {
    /** Seti icon name — must match a key in SETI_ICONS */
    name: string;
}

/* ─── Extension → icon mapping ─────────────────────────────────────── */

const EXT_MAP: Record<string, FileIconInfo> = {
    // Rust
    rs: { name: "rust" },
    toml: { name: "settings" },
    lock: { name: "lock" },

    // TypeScript / JavaScript
    ts: { name: "typescript" },
    tsx: { name: "react" },
    js: { name: "javascript" },
    jsx: { name: "react" },
    mjs: { name: "javascript" },
    cjs: { name: "javascript" },

    // Web
    html: { name: "html" },
    css: { name: "css" },
    scss: { name: "sass" },
    less: { name: "less" },
    svg: { name: "svg" },

    // Data / Config
    json: { name: "json" },
    yaml: { name: "yml" },
    yml: { name: "yml" },
    xml: { name: "xml" },

    // Markdown / Docs
    md: { name: "markdown" },
    mdx: { name: "markdown" },
    txt: { name: "default" },
    pdf: { name: "pdf" },

    // Shell
    sh: { name: "shell" },
    bash: { name: "shell" },
    zsh: { name: "shell" },
    fish: { name: "shell" },
    ps1: { name: "powershell" },
    bat: { name: "windows" },
    cmd: { name: "windows" },

    // Python
    py: { name: "python" },
    pyi: { name: "python" },

    // Go
    go: { name: "go" },
    mod: { name: "go2" },
    sum: { name: "lock" },

    // Java / JVM
    java: { name: "java" },
    kt: { name: "kotlin" },
    scala: { name: "java" },

    // Swift
    swift: { name: "swift" },

    // Ruby
    rb: { name: "ruby" },

    // PHP
    php: { name: "php" },

    // Images
    png: { name: "image" },
    jpg: { name: "image" },
    jpeg: { name: "image" },
    gif: { name: "image" },
    webp: { name: "image" },
    ico: { name: "favicon" },
    bmp: { name: "image" },

    // Media
    mp4: { name: "video" },
    mov: { name: "video" },
    avi: { name: "video" },
    webm: { name: "video" },
    mp3: { name: "audio" },
    wav: { name: "audio" },
    flac: { name: "audio" },

    // Archives
    zip: { name: "zip" },
    tar: { name: "zip" },
    gz: { name: "zip" },
    rar: { name: "zip" },
    "7z": { name: "zip" },

    // Spreadsheets
    csv: { name: "csv" },
    xlsx: { name: "xls" },
    xls: { name: "xls" },
    xlsm: { name: "xls" },

    // Word processing / presentations
    doc: { name: "word" },
    docx: { name: "word" },
    docm: { name: "word" },
    ppt: { name: "pptx" },
    pptx: { name: "pptx" },
    pptm: { name: "pptx" },

    // Database
    db: { name: "db" },
    sqlite: { name: "db" },
    sql: { name: "db" },

    // Security
    pem: { name: "lock" },
    key: { name: "lock" },
    cert: { name: "lock" },
    crt: { name: "lock" },

    // C/C++
    c: { name: "c" },
    h: { name: "c" },
    cpp: { name: "cpp" },
    hpp: { name: "cpp" },

    // C#
    cs: { name: "c-sharp" },

    // F#
    fs: { name: "f-sharp" },

    // VCS
    gitignore: { name: "git_ignore" },
    gitattributes: { name: "git" },
};

/* ─── Filename-only matches (exact name, no extension lookup) ─────── */

const NAME_MAP: Record<string, FileIconInfo> = {
    Dockerfile: { name: "docker" },
    Makefile: { name: "makefile" },
    LICENSE: { name: "license" },
    CONTRIBUTING: { name: "markdown" },
    CHANGELOG: { name: "markdown" },
    "package.json": { name: "npm" },
    "tsconfig.json": { name: "tsconfig" },
    ".eslintrc": { name: "eslint" },
    ".eslintrc.js": { name: "eslint" },
    ".eslintrc.json": { name: "eslint" },
    ".prettierrc": { name: "config" },
    "tailwind.config.js": { name: "config" },
    "tailwind.config.ts": { name: "config" },
    "vite.config.ts": { name: "vite" },
    "vite.config.js": { name: "vite" },
    "webpack.config.js": { name: "webpack" },
    "vue.config.js": { name: "vue" },
};

const DEFAULT_ICON: FileIconInfo = { name: "default" };

/** Get icon info for a filename — returns Seti icon name */
export function getFileIcon(filename: string): FileIconInfo {
    // Check filename-only matches first (exact filename like "package.json")
    const nameMatch = NAME_MAP[filename];
    if (nameMatch) return nameMatch;

    // Check special dot-files
    if (filename === ".gitignore") return EXT_MAP["gitignore"] ?? DEFAULT_ICON;
    if (filename === ".gitattributes") return EXT_MAP["gitattributes"] ?? DEFAULT_ICON;
    if (filename.endsWith(".lock")) return EXT_MAP["lock"] ?? DEFAULT_ICON;

    // Check extension
    const ext = filename.includes(".") ? filename.split(".").pop()!.toLowerCase() : "";
    return EXT_MAP[ext] ?? DEFAULT_ICON;
}
