//! Pure helpers for converting offline_providers.json-shaped model specs
//! into the `ModelInfo` + `ModelCapabilitiesInfo` shapes consumed by
//! `ModelMultiSelect`. Extracted from `AddProviderFlow.tsx` so they can
//! be unit-tested without rendering the whole dialog.

import type { ModelCapabilitiesInfo, ModelInfo } from "../../lib/types";

/** Convert one offline-style model spec (from `assets/offline_providers.json`)
 *  into the `ModelInfo` shape the multi-select picker consumes, plus a
 *  best-effort `ModelCapabilitiesInfo` for the per-model override map.
 *  Unknown fields are dropped — the picker only reads the subset below. */
export function offlineSpecToModel(
  spec: any,
  fallbackId: string,
): { info: ModelInfo; cap: ModelCapabilitiesInfo } | null {
  const id = String(spec?.id ?? fallbackId ?? "").trim();
  if (!id) return null;
  const name = String(spec?.name ?? id);
  const modalities = spec?.modalities;
  const limit = spec?.limit;
  const cost = spec?.cost;
  // Reasoning effort levels the model declares (models.dev
  // `reasoning_options`). Only the `effort` option contributes levels.
  const effortLevels = Array.isArray(spec?.reasoning_options)
    ? spec.reasoning_options
        .filter((o: any) => o?.type === "effort")
        .flatMap((o: any) => (Array.isArray(o?.values) ? o.values : []))
        .filter((v: unknown): v is string => typeof v === "string")
    : [];
  const info: ModelInfo = {
    id,
    name,
    family: spec?.family,
    reasoning: spec?.reasoning,
    tool_call: spec?.tool_call,
    attachment: spec?.attachment,
    temperature: spec?.temperature,
    release_date: spec?.release_date,
    context_window: limit?.context,
    max_tokens: limit?.output,
    knowledge: spec?.knowledge,
    input_cost: cost?.input,
    output_cost: cost?.output,
    input_modalities: Array.isArray(modalities?.input) ? modalities.input : undefined,
    output_modalities: Array.isArray(modalities?.output) ? modalities.output : undefined,
    reasoning_efforts: effortLevels.length > 0 ? effortLevels : undefined,
  };
  const cap: ModelCapabilitiesInfo = {
    context_window: limit?.context ?? 128000,
    max_output_tokens: limit?.output ?? 16384,
    supports_tool_calling: spec?.tool_call ?? true,
    supports_reasoning: spec?.reasoning ?? false,
    supports_attachment: spec?.attachment ?? false,
    supports_temperature: spec?.temperature ?? true,
    modalities:
      Array.isArray(modalities?.input) || Array.isArray(modalities?.output)
        ? {
            input: modalities?.input ?? ["text"],
            output: modalities?.output ?? ["text"],
          }
        : { input: ["text"], output: ["text"] },
    name,
    family: spec?.family,
    knowledge_cutoff: spec?.knowledge,
    reasoning_efforts: effortLevels.length > 0 ? effortLevels : undefined,
  };
  return { info, cap };
}

export interface ParsedOfflineJson {
  models: ModelInfo[];
  caps: Record<string, ModelCapabilitiesInfo>;
}

/** Accept three JSON shapes (matching what users copy from models.dev / a
 *  vendor docs page) and return model specs ready for the multi-select:
 *    1. `{ "models": { "<id>": { ... } } }`         — full provider entry from offline_providers.json
 *    2. `{ "<id>": { ... }, ... }`                  — bare models map (key = model id)
 *    3. `{ "id": "...", "name": "...", ... }`       — single model spec, auto-wrapped
 *  Returns `null` when the input doesn't look like any of the three. */
export function parseOfflineJson(raw: unknown): ParsedOfflineJson | null {
  let parsed: any = raw;
  if (typeof raw === "string") {
    try {
      parsed = JSON.parse(raw);
    } catch {
      return null;
    }
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return null;

  // Shape 1: full provider entry — take the inner `models` map.
  if (parsed.models && typeof parsed.models === "object" && !Array.isArray(parsed.models)) {
    parsed = parsed.models;
  }
  // Shape 2 / 3: now `parsed` is either a single spec or a map of specs.
  const entries: Array<[string, any]> = [];
  if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
    // Heuristic: if it has model-spec fields, treat as single spec; else as map.
    const looksLikeSpec =
      "name" in parsed || "modalities" in parsed || "limit" in parsed || "tool_call" in parsed;
    if (looksLikeSpec) {
      const id = String(parsed.id ?? "").trim();
      if (!id) return null;
      entries.push([id, parsed]);
    } else {
      for (const [k, v] of Object.entries(parsed)) {
        if (v && typeof v === "object") entries.push([k, v]);
      }
    }
  }
  if (entries.length === 0) return null;
  const models: ModelInfo[] = [];
  const caps: Record<string, ModelCapabilitiesInfo> = {};
  for (const [fallbackId, spec] of entries) {
    const m = offlineSpecToModel(spec, fallbackId);
    if (m) {
      models.push(m.info);
      caps[m.info.id] = m.cap;
    }
  }
  if (models.length === 0) return null;
  return { models, caps };
}

/** Pretty-printed demo of the full-provider-entry shape, shown inside the
 *  custom-provider dialog so users see exactly what the parser expects.
 *  Kept as a TS constant (not in i18n) because the i18n brace-linter
 *  (`scripts/check-i18n.mjs`) misfires on inner `{` / `}` inside JSON samples.
 *  Format is a JSON schema, not user copy — no translation needed. */
export const OFFLINE_JSON_EXAMPLE = `{
  "id": "my-provider",
  "name": "My Provider",
  "api": "https://api.example.com/v1",
  "models": {
    "my-model-large": {
      "id": "my-model-large",
      "name": "My Model Large",
      "tool_call": true,
      "reasoning": false,
      "attachment": true,
      "temperature": true,
      "modalities": {
        "input": ["text", "image"],
        "output": ["text"]
      },
      "limit": {
        "context": 128000,
        "output": 8192
      },
      "cost": {
        "input": 3,
        "output": 15
      }
    },
    "my-model-small": {
      "id": "my-model-small",
      "name": "My Model Small",
      "tool_call": true,
      "modalities": {
        "input": ["text"],
        "output": ["text"]
      },
      "limit": {
        "context": 32000,
        "output": 4000
      }
    }
  }
}`;