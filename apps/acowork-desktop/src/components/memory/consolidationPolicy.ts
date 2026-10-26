import type {
  CompactModelRef,
  DistillerStatus,
  ForgettingStatus,
} from "../../lib/types";

/**
 * Mapping between the memory service's consolidation policy and the two
 * memory-panel cards (记忆沉淀 / 记忆遗忘).
 *
 * ADR-101 P1e moved the distiller / forgetting knobs out of
 * `agent_config.json` and into the memory service, which owns the timer they
 * configure and persists them per instance as
 * `{instance_dir}/consolidation.json`. The cards therefore speak the service's
 * own shape, not the retired `AgentConfig` one:
 *
 * - the transport carries **seconds** (`interval_secs` / `idle_secs`) while the
 *   inputs show minutes;
 * - the write is a nested `{ distiller | forgetting }` **patch** — the service
 *   runs `deny_unknown_fields` and treats an absent key as "keep current", so
 *   a card must send exactly the fields it means to change and must never send
 *   the run statistics back;
 * - there is no "unset" state any more. Every field always holds a concrete
 *   value (the instance's `consolidation.json`, else the service config), so an
 *   empty / out-of-range input means "restore what the server says", not
 *   "clear the override".
 */

/** Form values of the 记忆沉淀 card. */
export interface DistillerForm {
  enabled: boolean;
  models: CompactModelRef[];
  /** Minutes, as text (what `<input type="number">` holds). */
  intervalMinutes: string;
  /** Episodes, as text. */
  accumulation: string;
  /** Minutes, as text. */
  idleMinutes: string;
}

/** Form values of the 记忆遗忘 card. */
export interface ForgettingForm {
  enabled: boolean;
  /** Days, as text. */
  halfLifeDays: string;
  /** `(0, 1]`, as text. */
  dormantThreshold: string;
  /** Days, as text. */
  archiveDays: string;
}

export const SECS_PER_MINUTE = 60;

/**
 * `interval_secs` / `idle_secs` → the minutes text an input shows.
 *
 * Rounded to hundredths: a service value that is not a whole number of minutes
 * (a hand-edited `consolidation.json`) must not fill the input with a long
 * floating-point tail, and re-saving the rounded value is a no-op at the
 * second granularity the transport uses.
 */
export function secsToMinutesInput(secs: number | null | undefined): string {
  if (secs == null || !Number.isFinite(secs) || secs <= 0) return "";
  return String(Math.round((secs / SECS_PER_MINUTE) * 100) / 100);
}

/** Minutes text → whole seconds; `null` when the text is not a usable duration. */
export function minutesToSecs(raw: string): number | null {
  const n = parseNumber(raw);
  if (n == null || n <= 0) return null;
  const secs = Math.round(n * SECS_PER_MINUTE);
  return secs >= 1 ? secs : null;
}

/** Positive whole count (accumulation threshold, archive days). */
export function parsePositiveInt(raw: string): number | null {
  const n = parseNumber(raw);
  if (n == null) return null;
  const whole = Math.round(n);
  return whole >= 1 ? whole : null;
}

/** Dormant threshold — the service accepts `(0, 1]` and rejects the rest. */
export function parseDormantThreshold(raw: string): number | null {
  const n = parseNumber(raw);
  if (n == null || n <= 0 || n > 1) return null;
  return n;
}

/**
 * `distiller` half of the status payload → card form values.
 *
 * `null` status (the fetch has not landed yet) leaves the card at its
 * defaults rather than throwing, which is what the caller's fail-soft load
 * path relies on.
 */
export function distillerForm(status: DistillerStatus | null | undefined): DistillerForm {
  return {
    enabled: !!status?.enabled,
    models: (status?.models ?? []).map(
      (m): CompactModelRef => ({
        provider_id: m.provider_id,
        model_id: m.model_id,
      }),
    ),
    intervalMinutes: secsToMinutesInput(status?.interval_secs),
    accumulation:
      status?.accumulation_threshold != null
        ? String(status.accumulation_threshold)
        : "",
    idleMinutes: secsToMinutesInput(status?.idle_secs),
  };
}

/** `forgetting` half of the status payload → card form values. */
export function forgettingForm(
  status: ForgettingStatus | null | undefined,
): ForgettingForm {
  return {
    enabled: !!status?.enabled,
    halfLifeDays: status?.half_life_days != null ? String(status.half_life_days) : "",
    dormantThreshold:
      status?.dormant_threshold != null
        ? String(Math.round(status.dormant_threshold * 1000) / 1000)
        : "",
    archiveDays: status?.archive_days != null ? String(status.archive_days) : "",
  };
}

function parseNumber(raw: string): number | null {
  const trimmed = raw.trim();
  if (trimmed === "") return null;
  const n = Number(trimmed);
  return Number.isFinite(n) ? n : null;
}
