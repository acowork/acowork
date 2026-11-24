/**
 * Persistent catalog cache (catalogCache.ts) — the seed half of the
 * optimistic first paint.
 *
 * What this pins:
 *   1. round-trip: a payload written is read back identically;
 *   2. every failure mode degrades to `null` / no-op, never a throw —
 *      the cache is an optimization and must never become a failure mode
 *      (private mode, corrupt JSON, hand-edited values, absent keys);
 *   3. the shape validator is the guard against a value that parses but is
 *      not what the caller will index into;
 *   4. `sameCatalog` is the diff guard behind "unchanged payload must not
 *      re-render": equal content → true (caller keeps the old reference),
 *      different content → false (caller applies the fresh data).
 */
import { beforeEach, describe, expect, it } from "vitest";

import { clearCatalog, readCatalog, sameCatalog, writeCatalog } from "./catalogCache";

describe("catalogCache read/write", () => {
    beforeEach(() => {
        localStorage.clear();
    });

    it("round-trips a payload through localStorage", () => {
        const value = { apps: [{ id: "a", icon: "https://x/a.png" }] };
        writeCatalog("appGallery:https://g/index.json", value);
        expect(readCatalog("appGallery:https://g/index.json")).toEqual(value);
    });

    it("returns null for an absent key", () => {
        expect(readCatalog("nope")).toBeNull();
    });

    it("drops a corrupt value instead of throwing", () => {
        localStorage.setItem("acowork.cache.broken", "{not json");
        expect(readCatalog("broken")).toBeNull();
    });

    it("honours the shape validator: a wrong shape degrades to null", () => {
        writeCatalog("index", { not: "an array" });
        expect(readCatalog("index", (v) => Array.isArray(v))).toBeNull();
        writeCatalog("index", [1, 2]);
        expect(readCatalog("index", (v) => Array.isArray(v))).toEqual([1, 2]);
    });

    it("clears a key", () => {
        writeCatalog("k", { a: 1 });
        clearCatalog("k");
        expect(readCatalog("k")).toBeNull();
    });

    it("is total when localStorage itself throws (private mode)", () => {
        const original = window.localStorage;
        Object.defineProperty(window, "localStorage", {
            configurable: true,
            value: {
                getItem: () => {
                    throw new Error("SecurityError");
                },
                setItem: () => {
                    throw new Error("SecurityError");
                },
                removeItem: () => {
                    throw new Error("SecurityError");
                },
            },
        });
        try {
            expect(() => writeCatalog("k", 1)).not.toThrow();
            expect(readCatalog("k")).toBeNull();
            expect(() => clearCatalog("k")).not.toThrow();
        } finally {
            Object.defineProperty(window, "localStorage", {
                configurable: true,
                value: original,
            });
        }
    });
});

describe("sameCatalog", () => {
    it("treats equal content as equal (the no-re-render guard)", () => {
        expect(
            sameCatalog(
                [{ id: "a", icon: "https://x/a.png" }],
                [{ id: "a", icon: "https://x/a.png" }],
            ),
        ).toBe(true);
    });

    it("catches a changed field", () => {
        expect(sameCatalog([{ icon: "a" }], [{ icon: "b" }])).toBe(false);
    });

    it("catches an added or removed entry", () => {
        expect(sameCatalog([{ id: "a" }], [{ id: "a" }, { id: "b" }])).toBe(false);
    });

    it("distinguishes null from an empty catalog", () => {
        // `null` = never loaded, `[]` = loaded and genuinely empty — the two
        // must never compare equal or a cold seed would mask the fetch.
        expect(sameCatalog(null, [])).toBe(false);
    });

    it("is reference-equal cheap", () => {
        const v = { a: 1 };
        expect(sameCatalog(v, v)).toBe(true);
    });
});
