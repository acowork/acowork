/**
 * ACowork App Gallery catalog — browse/install metadata for hosted apps.
 *
 * Sister of Agent Hub (`agentHub.ts`), but Apps ≠ Agents (PRD F7). The gallery
 * repo (acowork/ACoworkAppGallery) publishes `dist/index.json` with README
 * text embedded; this module merges that catalog with local `GET /api/apps`.
 */

import type { AppDto } from "./apps-api";

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
    return { ...body, base_url: base };
}

function versionsDiffer(a: string | null | undefined, b: string | null | undefined): boolean {
    if (!a || !b) return false;
    return a.trim() !== b.trim();
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
            badge =
                g.latest_version &&
                versionsDiffer(g.latest_version, local.installed_version || local.version)
                    ? "update"
                    : "installed";
        } else if (g.package?.url && g.package?.sha256) {
            badge = "available";
        }
        cards.push({
            id: g.id,
            name: g.name,
            summary: g.summary,
            icon: g.icon,
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
            icon: null,
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
