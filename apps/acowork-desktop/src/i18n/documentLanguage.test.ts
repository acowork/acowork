/**
 * The shell must state its own language in the document (ADR-121 §7).
 *
 * `<html lang>` is not decoration: the hosted-app bridge reads it to tell a
 * third-party app which locale to render in. It shipped as a static
 * `lang="zh-CN"` in `index.html` and nothing rewrote it, so the interface
 * could be in English while every hosted app was told "zh-CN". These tests
 * pin the two ways that attribute is written — at startup and on a change.
 */

import { describe, it, expect, afterEach } from "vitest";
import i18n, { applyDocumentLanguage } from "./index";

const originalLang = document.documentElement.getAttribute("lang");

afterEach(() => {
    if (originalLang === null) document.documentElement.removeAttribute("lang");
    else document.documentElement.setAttribute("lang", originalLang);
});

describe("document language", () => {
    it("writes the language the shell is in", () => {
        applyDocumentLanguage("zh-TW");
        expect(document.documentElement.getAttribute("lang")).toBe("zh-TW");
    });

    /** Startup path: the module applies the detected language on import, so
     *  the attribute is already true before the first app frame mounts. */
    it("states the active i18next language", async () => {
        await i18n.changeLanguage("en");
        expect(document.documentElement.getAttribute("lang")).toBe(i18n.language);
    });

    /** The switch a user makes in 设置 → 语言 must land in the document. */
    it("follows a language change", async () => {
        await i18n.changeLanguage("ja");
        expect(document.documentElement.getAttribute("lang")).toBe("ja");
        await i18n.changeLanguage("en");
        expect(document.documentElement.getAttribute("lang")).toBe("en");
    });
});
