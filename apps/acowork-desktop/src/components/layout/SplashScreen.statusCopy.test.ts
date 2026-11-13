/**
 * The boot status line renders as `t(status.key)`, i.e. through a DYNAMIC key.
 * `scripts/check-i18n.mjs` only walks `t("literal")` call sites, so these keys
 * are invisible to it — a typo or a locale that forgot one would ship as a raw
 * key name (`splashScreen.startingGateway`) on the very screen a broken
 * install lands on.
 *
 * This test is that missing check, and it also pins the two behaviours the
 * line owes the user since ADR-115:
 *   1. it names the address the boot is aimed at (the address IS the setting);
 *   2. it does not say "local" — local/remote stopped being user-facing
 *      concepts, there is one server address.
 */
import { describe, expect, it } from "vitest";
import en from "../../i18n/locales/en.json";
import zhCN from "../../i18n/locales/zh-CN.json";
import zhTW from "../../i18n/locales/zh-TW.json";
import ja from "../../i18n/locales/ja.json";
import ko from "../../i18n/locales/ko.json";
import { SPLASH_STATUS_KEYS } from "./SplashScreen";

const LOCALES: { name: string; table: Record<string, Record<string, string>> }[] = [
  { name: "en", table: en as never },
  { name: "zh-CN", table: zhCN as never },
  { name: "zh-TW", table: zhTW as never },
  { name: "ja", table: ja as never },
  { name: "ko", table: ko as never },
];

/** `{{name}}` placeholders of a template, sorted for comparison. */
function placeholders(template: string): string[] {
  return [...template.matchAll(/\{\{(\w+)\}\}/g)].map((m) => m[1]).sort();
}

function lookup(table: Record<string, Record<string, string>>, key: string): string {
  const [ns, name] = key.split(".");
  return table[ns]?.[name] ?? "";
}

describe("SplashScreen status copy", () => {
  it("every dynamic status key resolves in every locale", () => {
    for (const { name, table } of LOCALES) {
      for (const key of SPLASH_STATUS_KEYS) {
        expect(lookup(table, key), `${name} is missing ${key}`).not.toBe("");
      }
    }
  });

  it("placeholders match the English template in every locale", () => {
    for (const key of SPLASH_STATUS_KEYS) {
      const expected = placeholders(lookup(en as never, key));
      for (const { name, table } of LOCALES) {
        expect(placeholders(lookup(table, key)), `${name} / ${key}`).toEqual(expected);
      }
    }
  });

  it("names the address instead of a mode", () => {
    // The status line has to say WHICH gateway, and must not reintroduce the
    // local/remote vocabulary that ADR-115 removed from the UI.
    expect(en.splashScreen.startingGateway).toContain("{{url}}");
    expect(en.splashScreen.connectingToGateway).toContain("{{url}}");
    for (const { name, table } of LOCALES) {
      for (const key of ["splashScreen.startingGateway", "splashScreen.connectingToGateway"]) {
        expect(lookup(table, key).toLowerCase(), `${name} / ${key}`).not.toMatch(
          /\blocal\b|本地|本機|로컬|ローカル/,
        );
      }
    }
  });

  it("keeps the timeout copy free of the retired mode vocabulary", () => {
    // It used to say "switch to a local Gateway" — there is no mode to switch,
    // the address control below is the way to change servers.
    for (const { name, table } of LOCALES) {
      const hint = lookup(table, "splashScreen.timeoutHint").toLowerCase();
      expect(hint, `${name} timeoutHint`).not.toMatch(/\blocal\b|本地|本機|로컬|ローカル/);
    }
  });
});
