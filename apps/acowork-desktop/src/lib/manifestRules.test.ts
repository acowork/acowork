// ADR-104: boundary cases of the client-side mirror of the manifest rules.
//
// A mirror is only admissible if it can never accept something Rust rejects —
// a false rejection annoys, a false acceptance produces a dialog that says
// "fine" and then fails at create. Every case below is therefore written
// against the Rust rule it mirrors (named in the describe block), and the
// direction that matters is the one asserted: **what the client accepts must
// be a subset of what `acowork-core` accepts**.
//
// The Rust authorities:
//   - `manifest::is_valid_label`                        (labels)
//   - `ManifestConfigDefaults::validate`                (`[config]` ranges)
//   - `AgentManifest::validate`                         (agent_id: non-empty + a dot)

import { describe, it, expect } from "vitest";
import {
  CONFIG_LIMITS,
  MAX_LABELS,
  configFieldError,
  isValidAgentId,
  isValidLabel,
  parseLabels,
  parseOptionalNumber,
} from "./manifestRules";

describe("isValidLabel — mirrors manifest::is_valid_label", () => {
  it("accepts a lowercase kebab slug", () => {
    expect(isValidLabel("project-management")).toBe(true);
    expect(isValidLabel("ai")).toBe(true);
    expect(isValidLabel("v2")).toBe(true);
    expect(isValidLabel("a1-b2")).toBe(true);
  });

  it("enforces the length window (2-32)", () => {
    expect(isValidLabel("x")).toBe(false);
    expect(isValidLabel("a".repeat(32))).toBe(true);
    expect(isValidLabel("a".repeat(33))).toBe(false);
  });

  it("rejects uppercase, spaces, underscores and non-ASCII", () => {
    expect(isValidLabel("Project")).toBe(false);
    expect(isValidLabel("project management")).toBe(false);
    expect(isValidLabel("project_management")).toBe(false);
    expect(isValidLabel("项目管理")).toBe(false);
  });

  it("rejects a leading or trailing hyphen", () => {
    expect(isValidLabel("-project")).toBe(false);
    expect(isValidLabel("project-")).toBe(false);
  });

  it("counts code points, not UTF-16 units (same as Rust `chars().count()`)", () => {
    // Astral-plane characters are one char in Rust and two UTF-16 units here;
    // counting units would reject a label core accepts at length 16.
    expect(isValidLabel("𝟘".repeat(16))).toBe(false); // no ASCII letters -> rejected anyway
    expect(isValidLabel("a".repeat(31) + "𝟘")).toBe(false); // 32 chars but not [a-z0-9-]
  });
});

describe("parseLabels", () => {
  it("splits on commas and whitespace, lowercases, and dedupes", () => {
    expect(parseLabels("Agile, agile  dev-ops").labels).toEqual(["agile", "dev-ops"]);
  });

  it("reports every rejected token instead of dropping it silently", () => {
    // `x` is too short and `-bad-` has hyphens at both ends; uppercase alone is
    // *not* a rejection — it is lowered, which is why it is asserted separately.
    const { labels, rejected } = parseLabels("agile, x, -bad-, OK-1");
    expect(labels).toEqual(["agile", "ok-1"]);
    expect(rejected).toEqual(["x", "-bad-"]);
  });

  it("lowercases uppercase input rather than rejecting it (labels are kebab-only)", () => {
    expect(parseLabels("AGILE").labels).toEqual(["agile"]);
  });

  it("drops empty tokens", () => {
    expect(parseLabels(" , ,  ").labels).toEqual([]);
  });
});

describe("configFieldError — mirrors ManifestConfigDefaults::validate", () => {
  it("treats an empty field as 'no opinion', not zero", () => {
    for (const field of Object.keys(CONFIG_LIMITS) as Array<keyof typeof CONFIG_LIMITS>) {
      expect(configFieldError(field, ""), field).toBeNull();
      expect(configFieldError(field, "   "), field).toBeNull();
    }
  });

  it("enforces the temperature window 0.0-2.0", () => {
    expect(configFieldError("temperature", "0")).toBeNull();
    expect(configFieldError("temperature", "2")).toBeNull();
    expect(configFieldError("temperature", "2.1")).toBe("range");
    expect(configFieldError("temperature", "-0.1")).toBe("range");
  });

  it("rejects context_window / max_output_tokens / max_iterations <= 0", () => {
    for (const field of ["context_window", "max_output_tokens", "max_iterations"] as const) {
      expect(configFieldError(field, "0"), field).toBe("range");
      expect(configFieldError(field, "1"), field).toBeNull();
      expect(configFieldError(field, "-5"), field).toBe("range");
    }
  });

  it("refuses a fractional integer instead of truncating it", () => {
    // serde rejects `1.5` for a `u64`/`u32` field; truncating here would send a
    // number the user never typed.
    expect(configFieldError("context_window", "1.5")).toBe("integer");
    expect(configFieldError("max_iterations", "2.5")).toBe("integer");
  });

  it("enforces the compression window 0.05-0.95", () => {
    expect(configFieldError("compression_ratio_threshold", "0.05")).toBeNull();
    expect(configFieldError("compression_ratio_threshold", "0.95")).toBeNull();
    expect(configFieldError("compression_ratio_threshold", "1")).toBe("range");
    expect(configFieldError("compression_ratio_threshold", "0")).toBe("range");
  });

  it("reports garbage as a range error rather than as empty", () => {
    expect(configFieldError("temperature", "abc")).toBe("range");
  });
});

describe("parseOptionalNumber", () => {
  it("maps an empty string to null (no default), not to 0", () => {
    expect(parseOptionalNumber("")).toBeNull();
    expect(parseOptionalNumber("  ")).toBeNull();
    expect(parseOptionalNumber("0")).toBe(0);
  });

  it("maps non-numeric text to null", () => {
    expect(parseOptionalNumber("NaN-ish")).toBeNull();
  });
});

describe("isValidAgentId — stricter than core, never looser", () => {
  it("accepts a reverse-domain id", () => {
    expect(isValidAgentId("com.example.myagent")).toBe(true);
    expect(isValidAgentId("com.acowork.pm")).toBe(true);
  });

  it("rejects a bare name (core also requires a dot)", () => {
    expect(isValidAgentId("myagent")).toBe(false);
  });

  it("rejects a segment starting with a digit or containing an underscore", () => {
    expect(isValidAgentId("com.1example.a")).toBe(false);
    expect(isValidAgentId("com.ex_ample.a")).toBe(false);
  });

  it("trims before checking", () => {
    expect(isValidAgentId("  com.example.a  ")).toBe(true);
  });
});

describe("limits", () => {
  it("matches MAX_MANIFEST_LABELS in core", () => {
    expect(MAX_LABELS).toBe(12);
  });
});