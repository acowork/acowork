import { useMemo } from "react";
import { SETI_ICONS } from "../../assets/seti-icons";
import { useSettingsStore } from "../../stores/settingsStore";

/* ─── Theme-aware color maps ───────────────────────────────────────────
 * Each icon type gets a carefully chosen color for light & dark backgrounds.
 * Luminance targets: ~120-180 for dark mode (visible on dark bg),
 *                    ~80-160 for light mode (visible on light bg).
 * ────────────────────────────────────────────────────────────────────── */

const DARK_COLORS: Record<string, string> = {
    rust: "#A0A0A0",
    typescript: "#519ABA",
    react: "#61DAFB",
    javascript: "#CBBA61",
    html: "#E37933",
    css: "#519ABA",
    sass: "#CF6499",
    less: "#519ABA",
    json: "#CBBA61",
    xml: "#E37933",
    markdown: "#519ABA",
    default: "#A0A0A0",
    shell: "#8DC268",
    python: "#519ABA",
    go: "#519ABA",
    go2: "#519ABA",
    java: "#CC785C",
    kotlin: "#B178DF",
    swift: "#F08080",
    ruby: "#C9574F",
    php: "#A074B4",
    c: "#599DCE",
    cpp: "#6A9CD0",
    "c-sharp": "#68B723",
    "f-sharp": "#6A9CD0",
    docker: "#384D54",
    eslint: "#9B6DD7",
    image: "#A074B4",
    svg: "#C9A046",
    favicon: "#CBBA61",
    pdf: "#CC4B4B",
    settings: "#A0A0A0",
    lock: "#D4A76A",
    db: "#D4A76A",
    yml: "#A074B4",
    powershell: "#519ABA",
    windows: "#46AAEB",
    git: "#D0633C",
    git_ignore: "#D0633C",
    csv: "#8DC268",
    config: "#A0A0A0",
    video: "#C9574F",
    audio: "#C9574F",
    zip: "#D4A76A",
    folder: "#D4AA6A",
    license: "#CBBA61",
    makefile: "#B0B0B0",
    npm: "#CC4B4B",
    webpack: "#8ED6F8",
    vue: "#55B889",
    vite: "#C9A046",
    tsconfig: "#519ABA",
    xls: "#5FA05F",
    word: "#519ABA",
    pptx: "#D0633C",
};

const LIGHT_COLORS: Record<string, string> = {
    rust: "#A0A0A0",
    typescript: "#3B7BA5",
    react: "#1A8FAD",
    javascript: "#A89A20",
    html: "#E37933",
    css: "#3B7BA5",
    sass: "#B84479",
    less: "#3B7BA5",
    json: "#A89A20",
    xml: "#D06020",
    markdown: "#3B7BA5",
    default: "#6B6B6B",
    shell: "#5A8A30",
    python: "#356EA1",
    go: "#3B7BA5",
    go2: "#3B7BA5",
    java: "#A0522D",
    kotlin: "#8B5CC0",
    swift: "#E05858",
    ruby: "#B03030",
    php: "#7B4D94",
    c: "#3B7BA5",
    cpp: "#4A7CB5",
    "c-sharp": "#5A9A20",
    "f-sharp": "#4A7CB5",
    docker: "#2B6A8A",
    eslint: "#7B4DC0",
    image: "#7B4D94",
    svg: "#A07A20",
    favicon: "#A89A20",
    pdf: "#CC3333",
    settings: "#6B6B6B",
    lock: "#A07A20",
    db: "#A07A20",
    yml: "#7B4D94",
    powershell: "#3B7BA5",
    windows: "#0078D7",
    git: "#C0502A",
    git_ignore: "#C0502A",
    csv: "#5A8A30",
    config: "#6B6B6B",
    video: "#B03030",
    audio: "#B03030",
    zip: "#A07A20",
    folder: "#B08A40",
    license: "#A89A20",
    makefile: "#707070",
    npm: "#CC3333",
    webpack: "#3B7BA5",
    vue: "#3D8B5F",
    vite: "#A07A20",
    tsconfig: "#3B7BA5",
    xls: "#3D8B3D",
    word: "#3B7BA5",
    pptx: "#C0502A",
};

/* ─── SVG sanitization ────────────────────────────────────────────── */

/**
 * Replace hardcoded fills with `currentColor` so SVG inherits color from CSS.
 * - Replaces `fill="#xxx"` on path/polygon/circle/rect/g/ellipse → `fill="currentColor"`
 * - Removes inline `<style>` blocks (e.g. f-sharp uses .st0{fill:#231f20})
 * - Removes `fill="none"` on root <svg> element (java.svg)
 * - Strips width/height from root <svg> so we control sizing
 * - Preserves `fill="none"` on child elements (transparent cutouts)
 * - Preserves `opacity` attributes
 */
function sanitizeSvg(raw: string): string {
    if (!raw) return "";
    return raw
        // Remove <style>...</style> blocks (handles f-sharp etc.)
        .replace(/<style[^>]*>[\s\S]*?<\/style>/gi, "")
        // Remove fill="none" from the root <svg> element (e.g. java.svg)
        .replace(/(<svg[^>]*)\s+fill="none"/, "$1")
        // Strip original width/height from root <svg> (we set our own)
        .replace(/(<svg[^>]*)\s+width="[^"]*"/, "$1")
        .replace(/(<svg[^>]*)\s+height="[^"]*"/, "$1")
        // Replace hardcoded fills on child elements with currentColor
        .replace(
            /(<(?:path|polygon|circle|rect|g|ellipse)\b[^>]*)\s+fill="[^"]*"/g,
            '$1 fill="currentColor"',
        )
        // Remove class attributes left over from style blocks (e.g. class="st0")
        .replace(/(<(?:path|polygon|circle|rect|g|ellipse)\b[^>]*)\s+class="[^"]*"/g, "$1");
}

/* ─── Component ────────────────────────────────────────────────────── */

interface SetiIconProps {
    /** Icon name matching a key in SETI_ICONS (e.g. "rust", "typescript") */
    name: string;
    /** Size in pixels (width & height). Default: 16 */
    size?: number;
    /** CSS class for the wrapper span */
    className?: string;
    /** Optional explicit color override (skips theme color map) */
    color?: string;
}

/**
 * Renders a Seti UI file icon as inline SVG with dark/light theme support.
 * Icons are MIT-licensed from https://github.com/jesseweed/seti-ui
 *
 * Hardcoded SVG fills are stripped at runtime so the icon color
 * is fully controlled by the active theme (dark or light).
 */
export function SetiIcon({ name, size = 20, className, color }: SetiIconProps) {
    const theme = useSettingsStore((s) => s.theme);
    const osTheme = useSettingsStore((s) => s.osTheme);
    const isDark = theme === "dark" || (theme === "system" && osTheme === "dark");

    const raw = SETI_ICONS[name] ?? SETI_ICONS["default"];

    const html = useMemo(() => {
        if (!raw) return "";
        const clean = sanitizeSvg(raw);
        return clean.replace(
            "<svg ",
            `<svg fill="currentColor" width="${size}" height="${size}" `,
        );
    }, [raw, size]);

    const iconColor =
        color ?? (isDark ? DARK_COLORS[name] : LIGHT_COLORS[name]) ?? (isDark ? "#A0A0A0" : "#6B6B6B");

    return (
        <span
            className={className}
            style={{
                display: "inline-flex",
                alignItems: "center",
                justifyContent: "center",
                width: size,
                height: size,
                flexShrink: 0,
                color: iconColor,
            }}
            dangerouslySetInnerHTML={{ __html: html }}
        />
    );
}
