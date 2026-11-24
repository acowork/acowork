/**
 * ACowork App Gallery catalog — browse/install metadata for hosted apps.
 *
 * Sister of Agent Hub (`agentHub.ts`), but Apps ≠ Agents (PRD F7). The gallery
 * repo (acowork/ACoworkAppGallery) publishes `dist/index.json` with README
 * text embedded; this module merges that catalog with local `GET /api/apps`.
 */

import type { AppDto } from "./apps-api";
import { readCatalog, writeCatalog } from "./catalogCache";
import pmAppIcon from "../assets/nav-icons/projects.png";
import docAppIcon from "../assets/nav-icons/docs.png";

/**
 * Artwork for the built-in apps.
 *
 * The gallery catalog only carries icons for published gallery entries. The
 * built-in PM/DOC apps are local-only, so they had no icon at all and fell
 * back to a one-letter tile ("项" / "文"). These are the SAME files the left
 * nav uses, so the two surfaces cannot drift apart.
 */
const BUILTIN_APP_ICONS: Record<string, string> = {
    pm: pmAppIcon,
    doc: docAppIcon,
};

/** Cloudflare Pages site (see ACoworkAppGallery docs/deploy.md). */
export const DEFAULT_APP_GALLERY_INDEX_URL =
    "https://gallery.acowork.ai/data/index.json";

export interface GalleryPublisher {
    id: string;
    display_name: string;
    homepage?: string | null;
}

export interface GalleryPackage {
    url: string;
    sha256: string;
    size_bytes?: number | null;
    platforms?: string[];
}

export interface GalleryApp {
    id: string;
    name: string;
    summary: string;
    publisher: GalleryPublisher;
    license: string;
    repository: string;
    homepage?: string | null;
    upstream?: string | null;
    categories: string[];
    icon?: string | null;
    latest_version?: string | null;
    package: GalleryPackage | null;
    readme_md: string;
    source_path: string;
}

export interface GalleryIndex {
    schema_version: number;
    generated_at?: string;
    apps: GalleryApp[];
    /** Injected after fetch — origin used to resolve relative assets if needed. */
    base_url: string;
}

export type InstallBadge = "installed" | "update" | "available" | "catalog_only" | "builtin";

export type GalleryAttributionKind = "repository" | "homepage" | "upstream";

export interface GalleryAttributionLink {
    kind: GalleryAttributionKind;
    href: string;
}

/**
 * Distinct origin links for a catalog card. Homepage that repeats the
 * repository is omitted so a fork is not presented as the upstream product.
 */
export function galleryAttributionLinks(
    app: Pick<GalleryApp, "repository" | "homepage" | "upstream">,
): GalleryAttributionLink[] {
    const seen = new Set<string>();
    const links: GalleryAttributionLink[] = [];
    const push = (kind: GalleryAttributionKind, href?: string | null) => {
        const url = (href || "").trim();
        if (!url || seen.has(url)) return;
        seen.add(url);
        links.push({ kind, href: url });
    };
    push("repository", app.repository);
    push("homepage", app.homepage);
    push("upstream", app.upstream);
    return links;
}

export interface GalleryCard {
    /** Catalog id, or local-only app id. */
    id: string;
    name: string;
    summary: string;
    icon?: string | null;
    publisherLabel: string;
    versionLabel: string;
    badge: InstallBadge;
    gallery: GalleryApp | null;
    installed: AppDto | null;
}

export function galleryIndexUrl(): string {
    try {
        const fromEnv =
            typeof import.meta !== "undefined" &&
            (import.meta as ImportMeta & { env?: Record<string, string> }).env
                ?.VITE_ACOWORK_APP_GALLERY_URL;
        if (fromEnv && fromEnv.trim()) return fromEnv.trim();
    } catch {
        // ignore
    }
    if (typeof window !== "undefined") {
        const override = window.localStorage?.getItem("acowork.appGalleryUrl");
        if (override && override.trim()) return override.trim();
    }
    return DEFAULT_APP_GALLERY_INDEX_URL;
}

export async function fetchGalleryIndex(url = galleryIndexUrl()): Promise<GalleryIndex> {
    const res = await fetch(url, { method: "GET" });
    if (!res.ok) {
        throw new Error(`gallery index HTTP ${res.status}`);
    }
    const ctype = (res.headers.get("content-type") || "").toLowerCase();
    // GitHub raw often serves text/plain; reject HTML SPA fallbacks.
    if (ctype.includes("text/html")) {
        throw new Error("gallery index returned HTML, not JSON");
    }
    const body = (await res.json()) as GalleryIndex;
    if (!body || !Array.isArray(body.apps)) {
        throw new Error("gallery index missing apps[]");
    }
    const base = url.replace(/\/[^/]*$/, "");
    const index: GalleryIndex = { ...body, base_url: base };
    // Persist the last good catalog so the next entry into a surface can
    // paint real artwork on the first frame (see catalogCache.ts). Keyed by
    // URL: a staging override must not seed the production gallery.
    writeCatalog(galleryCacheKey(url), index);
    return index;
}

/** Key for the gallery cache. One entry per index URL, not across origins. */
function galleryCacheKey(url: string): string {
    return `appGallery:${url}`;
}

/**
 * The last successfully fetched catalog for `url`, or `null`.
 *
 * Synchronous by design: surfaces call this inside a `useState` initializer
 * so the FIRST render already carries the real icons — that is the whole
 * point of the optimistic seed. A malformed entry is dropped, never trusted.
 */
export function readCachedGalleryIndex(url = galleryIndexUrl()): GalleryIndex | null {
    return readCatalog<GalleryIndex>(
        galleryCacheKey(url),
        (v) => !!v && typeof v === "object" && Array.isArray((v as GalleryIndex).apps),
    );
}

/**
 * The `app id → icon URL` map the nav rail and cards resolve against.
 *
 * One derivation for both the cached seed and the fresh fetch, so the two
 * paths cannot disagree about which entries carry usable artwork.
 */
export function galleryIconMap(index: GalleryIndex | null): Record<string, string> {
    const icons: Record<string, string> = {};
    if (!index) return icons;
    for (const app of index.apps) {
        if (app.icon) icons[app.id] = app.icon;
    }
    return icons;
}

/** Numeric dotted version, ignoring a leading `v` and any `-` / `+` suffix. */
function versionParts(v: string): number[] {
    const core = v.trim().replace(/^v/i, "").split(/[-+]/)[0] ?? "";
    return core.split(".").map((p) => {
        const n = Number.parseInt(p, 10);
        return Number.isFinite(n) ? n : 0;
    });
}

/** True only when the catalog version is strictly newer than the install. */
export function catalogVersionIsNewer(
    catalog: string | null | undefined,
    installed: string | null | undefined,
): boolean {
    if (!catalog?.trim() || !installed?.trim()) return false;
    const a = versionParts(catalog);
    const b = versionParts(installed);
    const n = Math.max(a.length, b.length);
    for (let i = 0; i < n; i++) {
        const av = a[i] ?? 0;
        const bv = b[i] ?? 0;
        if (av > bv) return true;
        if (av < bv) return false;
    }
    return false;
}

export function mergeGalleryCards(
    gallery: GalleryApp[],
    installed: AppDto[],
): GalleryCard[] {
    const byId = new Map(installed.map((a) => [a.id, a]));
    const cards: GalleryCard[] = [];
    const seen = new Set<string>();

    for (const g of gallery) {
        seen.add(g.id);
        const local = byId.get(g.id) ?? null;
        let badge: InstallBadge = "catalog_only";
        if (local?.builtin) badge = "builtin";
        else if (local?.installed_version || (local && !local.builtin)) {
            badge = catalogVersionIsNewer(
                g.latest_version,
                local.installed_version || local.version,
            )
                ? "update"
                : "installed";
        } else if (g.package?.url && g.package?.sha256) {
            badge = "available";
        }
        cards.push({
            id: g.id,
            name: g.name,
            summary: g.summary,
            icon: BUILTIN_APP_ICONS[g.id] ?? g.icon,
            publisherLabel: g.publisher.display_name,
            versionLabel: local?.installed_version || g.latest_version || g.package?.url || "—",
            badge,
            gallery: g,
            installed: local,
        });
    }

    // Local-only (builtins, sideloaded) that are not in the gallery yet.
    for (const app of installed) {
        if (seen.has(app.id)) continue;
        cards.push({
            id: app.id,
            name: app.name,
            summary: app.description || app.id,
            icon: BUILTIN_APP_ICONS[app.id] ?? null,
            publisherLabel: app.builtin ? "ACowork" : app.origin || "local",
            versionLabel: app.installed_version || app.version,
            badge: app.builtin ? "builtin" : "installed",
            gallery: null,
            installed: app,
        });
    }

    return cards;
}

export function filterGalleryCards(cards: GalleryCard[], query: string): GalleryCard[] {
    const q = query.trim().toLowerCase();
    if (!q) return cards;
    return cards.filter((c) =>
        `${c.name} ${c.id} ${c.summary} ${c.publisherLabel}`.toLowerCase().includes(q),
    );
}
