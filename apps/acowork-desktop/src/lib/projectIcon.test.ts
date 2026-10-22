/**
 * projectIcon.ts — 图标引用解析 + URL 构造（纯逻辑，无 DOM）。
 *
 * `config` / `builtinIcons` 打桩，避免拉入 settingsStore 与 Vite glob，
 * 让断言确定化。
 */
import { describe, it, expect, vi } from "vitest";

vi.mock("./config", () => ({ getGatewayUrl: () => "http://gw.test" }));
vi.mock("./builtinIcons", () => ({
  BUILTIN_ICONS: { "icon-01": "a.jpg", "icon-02": "b.jpg" },
}));

import { parseProjectIcon, resolveProjectIconUrl } from "./projectIcon";

describe("parseProjectIcon", () => {
  it("returns a builtin ref when the id is bundled", () => {
    expect(parseProjectIcon({ icon: { kind: "builtin", id: "icon-01" } })).toEqual({
      kind: "builtin",
      id: "icon-01",
    });
  });

  it("nulls a builtin ref whose id is not bundled (→ default icon)", () => {
    expect(parseProjectIcon({ icon: { kind: "builtin", id: "nope" } })).toBeNull();
  });

  it("returns a custom ref with its path", () => {
    expect(
      parseProjectIcon({ icon: { kind: "custom", path: "projects/p-1/icon/icon-01.png" } }),
    ).toEqual({ kind: "custom", path: "projects/p-1/icon/icon-01.png" });
  });

  it("nulls missing / malformed metadata (backward compatible)", () => {
    expect(parseProjectIcon({})).toBeNull();
    expect(parseProjectIcon(null)).toBeNull();
    expect(parseProjectIcon(undefined)).toBeNull();
    expect(parseProjectIcon({ icon: "icon-01" })).toBeNull();
    expect(parseProjectIcon({ icon: { kind: "weird" } })).toBeNull();
    expect(parseProjectIcon({ icon: { kind: "custom", path: "" } })).toBeNull();
  });
});

describe("resolveProjectIconUrl", () => {
  it("builds the icon-file URL with the path percent-encoded", () => {
    expect(resolveProjectIconUrl("p-1", "projects/p-1/icon/icon-01.png")).toBe(
      "http://gw.test/api/pm/projects/p-1/icon-file?path=projects%2Fp-1%2Ficon%2Ficon-01.png",
    );
  });
});
