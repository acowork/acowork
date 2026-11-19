/**
 * Which installed apps earn a left-nav slot (ADR-116).
 *
 * Built-in PM/DOC keep their dedicated `projects` / `docs` views; only
 * non-builtin webview apps (e.g. FigCraft (ACowork)) appear as dynamic nav icons.
 */
import type { AppDto } from "./apps-api";

export function hostedNavApps(apps: AppDto[]): AppDto[] {
    return apps
        .filter(
            (a) =>
                a.enabled &&
                !a.builtin &&
                a.ui_mode === "webview" &&
                a.status !== "disabled",
        )
        .slice()
        .sort((a, b) => a.nav_order - b.nav_order || a.id.localeCompare(b.id));
}

/**
 * Resolve a nav glyph.
 *
 * Manifest `icon` / `nav_icon` win when they are absolute URLs. Otherwise the
 * gallery catalog icon (keyed by app id) is the published artwork — local
 * installs often store only a short name such as `"figcraft"`.
 */
export function hostedAppIconSrc(
    app: AppDto,
    galleryIcons?: Readonly<Record<string, string | null | undefined>>,
): string | null {
    for (const raw of [app.nav_icon, app.icon]) {
        const icon = (raw || "").trim();
        if (/^https?:\/\//i.test(icon)) return icon;
    }
    const fromGallery = (galleryIcons?.[app.id] || "").trim();
    if (/^https?:\/\//i.test(fromGallery)) return fromGallery;
    return null;
}

export function hostedAppLetter(app: AppDto): string {
    const label = (app.nav_label || app.name || app.id).trim();
    return (label.charAt(0) || "?").toUpperCase();
}
