// ADR-104: client-side mirror of the manifest's authoring rules.
//
// ## Why a mirror exists at all
//
// The authority is `acowork-core`: `ManifestDraft::build` re-validates every
// field and `create_agent` refuses what it rejects. What the dialog cannot
// afford is a round trip per keystroke, so the checks that decide "can the
// user leave this step" are mirrored here for instant feedback.
//
// A mirror is only safe if it **cannot accept something Rust rejects** — a
// false rejection merely annoys, but a false acceptance produces a dialog that
// says "fine" and then fails at create. So every rule below is deliberately
// narrower or equal, and `manifestRules.test.ts` pins the boundary cases
// against the Rust test names (`is_valid_label`, `draft_build_*`).
//
// Nothing here duplicates *policy that can drift silently*: limits come with
// the same numeric values quoted from `core/acowork-core/src/manifest.rs`.

/** `MAX_MANIFEST_LABELS` in `acowork-core/src/manifest.rs`. */
export const MAX_LABELS = 12;
/** `MIN_LABEL_LEN` in `acowork-core/src/manifest.rs`. */
export const MIN_LABEL_LEN = 2;
/** `MAX_LABEL_LEN` in `acowork-core/src/manifest.rs`. */
export const MAX_LABEL_LEN = 32;

/**
 * Lowercase kebab slug, 2–32 chars, no leading/trailing hyphen.
 *
 * Mirrors `manifest::is_valid_label` exactly.
 */
export function isValidLabel(label: string): boolean {
  const len = Array.from(label).length;
  if (len < MIN_LABEL_LEN || len > MAX_LABEL_LEN) return false;
  if (!/^[a-z0-9-]+$/.test(label)) return false;
  return !label.startsWith("-") && !label.endsWith("-");
}

/**
 * Parse a comma/space separated label input into chips, keeping the order and
 * dropping duplicates.
 *
 * Returns the accepted labels plus the rejected tokens so the UI can show
 * exactly which token it refused instead of silently swallowing it.
 */
export function parseLabels(input: string): {
  labels: string[];
  rejected: string[];
} {
  const seen = new Set<string>();
  const labels: string[] = [];
  const rejected: string[] = [];
  for (const raw of input.split(/[,，\s]+/)) {
    const token = raw.trim().toLowerCase();
    if (!token) continue;
    if (!isValidLabel(token)) {
      rejected.push(token);
      continue;
    }
    if (seen.has(token)) continue;
    seen.add(token);
    labels.push(token);
  }
  return { labels, rejected };
}

/** `[config]` value ranges, mirrored from `ManifestConfigDefaults::validate`. */
export const CONFIG_LIMITS = {
  temperature: { min: 0, max: 2 },
  /** 0 is rejected by core (ADR-074 §6). */
  context_window: { min: 1 },
  max_output_tokens: { min: 1 },
  max_iterations: { min: 1 },
  /** ADR-061 compaction target: saved share, 0.05–0.95. */
  compression_ratio_threshold: { min: 0.05, max: 0.95 },
} as const;

/** Empty string / non-numeric input means "no default" — not "zero". */
export function parseOptionalNumber(raw: string): number | null {
  const trimmed = raw.trim();
  if (trimmed === "") return null;
  const value = Number(trimmed);
  return Number.isFinite(value) ? value : null;
}

/** Integer-typed fields must not be silently truncated. */
export function isPositiveInteger(raw: string): boolean {
  const value = parseOptionalNumber(raw);
  return value !== null && Number.isInteger(value) && value >= CONFIG_LIMITS.max_iterations.min;
}

/**
 * Check one `[config]` field in isolation.
 *
 * Returns `null` when the value is acceptable (including "empty"), else a
 * translated error key suffix: `range` / `integer`.
 */
export function configFieldError(
  field: keyof typeof CONFIG_LIMITS,
  raw: string,
): "range" | "integer" | null {
  const value = parseOptionalNumber(raw);
  if (value === null) return raw.trim() === "" ? null : "range";
  if (field === "context_window" || field === "max_output_tokens" || field === "max_iterations") {
    if (!Number.isInteger(value)) return "integer";
  }
  const limits = CONFIG_LIMITS[field];
  if (value < limits.min) return "range";
  if ("max" in limits && value > limits.max) return "range";
  return null;
}

/**
 * Reverse-domain `agent_id` check.
 *
 * `AgentManifest::validate` only requires non-empty + a dot, so this is
 * deliberately *stricter* (each segment starts with a letter): a stricter
 * client rule can only reject something Rust would accept, never the reverse.
 */
export function isValidAgentId(value: string): boolean {
  const id = value.trim();
  if (!id.includes(".")) return false;
  return id.split(".").every((seg) => /^[A-Za-z][A-Za-z0-9-]*$/.test(seg));
}