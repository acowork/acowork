//! Per-model protocol selection state for the provider dialogs (ADR-103 D11).
//!
//! Both the "add provider" flow and the harness edit dialog need the same four
//! things:
//!
//!   1. a per-model dropdown value (D11 position 3),
//!   2. a per-model `base_url` override (only for rows with an explicit
//!      selection — the override belongs to the selection that produced it),
//!   3. a "re-verify" action against `POST /api/providers/{id}/probe` (D10),
//!      issued **with the protocol the row selects** (ADR-103 D14: a verdict
//!      is about one route, so probing the wrong one would corrupt the gate),
//!   4. the wire payload for the next save (`model_protocols` /
//!      `model_base_urls`), which must express "leave untouched" and
//!      "clear the selection" as different things.
//!
//! Keeping that in one hook means the two dialogs cannot drift, and the pure
//! parts (option mapping, payload diffing, request estimate) stay in
//! [`crate::lib::modelProtocol`] where they are unit-tested.

import { useCallback, useMemo, useState } from "react";
import type { ModelInfo, ModelProtocolsMap } from "../../lib/types";
import { getGatewayUrl } from "../../lib/config";
import {
  RESPONSES_PROTOCOL,
  estimateProbeRequests,
  protocolOfOption,
  protocolOptionOf,
  type ProbeEstimate,
  type ProtocolBadge,
  type ProtocolOption,
  deriveProtocolBadge,
} from "../../lib/modelProtocol";

/** Result of one "re-verify" round trip (D10 contract). */
export type ProbeOutcomeSummary = {
  ok: boolean;
  /** `verified` / `degraded` / `failed`, or `error` when the call itself failed. */
  state: string;
  /** Machine-readable failure class from the Gateway, when present. */
  errorClass?: string;
  /** Human-readable detail (failure reason / HTTP error). */
  detail?: string;
  /** Fields the probe had to drop (D8 trim) — names the `degraded` badge. */
  strippedFields: string[];
};

export interface ModelProtocolState {
  /** Dropdown value per model id. */
  selections: Record<string, ProtocolOption>;
  /** base_url override per model id (empty string = inherit). */
  baseUrls: Record<string, string>;
  /** Result of the most recent verify per model id. */
  outcomes: Record<string, ProbeOutcomeSummary | undefined>;
  /** Model id currently being verified. */
  verifying: string | null;

  /** Seed the state from a model list (stored selections win). */
  hydrate: (models: ModelInfo[]) => void;
  setSelection: (modelId: string, next: ProtocolOption) => void;
  setBaseUrl: (modelId: string, next: string) => void;

  /** Badge to render for one model, from the latest known fields. */
  badgeFor: (model: ModelInfo) => ProtocolBadge;
  /** Whether the base_url input should be shown for this model (an explicit
   *  selection exists — the override belongs to the selected protocol). */
  needsBaseUrl: (modelId: string) => boolean;

  /**
   * Wire payload for the next save.
   *
   * `model_protocols` contains an entry only for models whose selection differs
   * from what the Gateway already has, so an untouched row is never rewritten
   * (and never re-probed). A `null` value clears a selection the user withdrew.
   */
  buildIntent: (models: ModelInfo[]) => {
    model_protocols: ModelProtocolsMap;
    model_base_urls: Record<string, string>;
  };

  /** How many real requests the next save will send (D12 / save hint). */
  estimate: (models: ModelInfo[]) => ProbeEstimate;

  /**
   * Re-run the probe for one model, bypassing the 24 h cache.
   *
   * ADR-103 D14: the caller passes the protocol the row *selects* (falling
   * back to the provider-level protocol when it follows the data). Probing a
   * different route than the selection would write a verdict that cannot
   * enable — or actively demotes — the stored one.
   */
  verify: (
    providerId: string,
    modelId: string,
    protocol: string,
  ) => Promise<ProbeOutcomeSummary>;
}

function initialSelections(models: ModelInfo[]): Record<string, ProtocolOption> {
  const next: Record<string, ProtocolOption> = {};
  for (const m of models) next[m.id] = protocolOptionOf(m.protocol_selected);
  return next;
}

/**
 * Read the Gateway probe response into a flat summary.
 *
 * `ProbeResponse` flattens `ProbeOutcome` into the top level (D10.1), so
 * `error` / `stripped_fields` sit next to `state` — not under an `outcome`
 * object. Reading them one level too deep silently degraded every failure
 * reason to "HTTP 400".
 */
function summarizeProbeResponse(body: unknown, httpStatus: number): ProbeOutcomeSummary {
  const obj = (body ?? {}) as Record<string, unknown>;
  const state = typeof obj.state === "string" ? obj.state : "error";
  const error = (obj.error ?? undefined) as
    | { class?: string; message?: string }
    | undefined;
  const ok = state === "verified" || state === "degraded";
  const strippedFields = Array.isArray(obj.stripped_fields)
    ? (obj.stripped_fields as unknown[]).filter(
        (f): f is string => typeof f === "string",
      )
    : [];
  return {
    ok,
    state,
    errorClass: error?.class,
    detail: ok ? undefined : error?.message ?? `HTTP ${httpStatus}`,
    strippedFields,
  };
}

export function useModelProtocolState(): ModelProtocolState {
  const [selections, setSelections] = useState<Record<string, ProtocolOption>>({});
  const [baseUrls, setBaseUrls] = useState<Record<string, string>>({});
  const [outcomes, setOutcomes] = useState<
    Record<string, ProbeOutcomeSummary | undefined>
  >({});
  const [verifying, setVerifying] = useState<string | null>(null);

  const hydrate = useCallback((models: ModelInfo[]) => {
    setSelections(initialSelections(models));
    const urls: Record<string, string> = {};
    for (const m of models) {
      // The Gateway does not project the override itself; the dialog starts
      // from "inherit" and only writes what the user types.
      urls[m.id] = "";
    }
    setBaseUrls(urls);
    setOutcomes({});
    setVerifying(null);
  }, []);

  const setSelection = useCallback((modelId: string, next: ProtocolOption) => {
    setSelections((prev) => ({ ...prev, [modelId]: next }));
    // A new selection invalidates the previous verdict, so the stale yellow
    // badge must not outlive the click that changed it.
    setOutcomes((prev) => ({ ...prev, [modelId]: undefined }));
  }, []);

  const setBaseUrl = useCallback((modelId: string, next: string) => {
    setBaseUrls((prev) => ({ ...prev, [modelId]: next }));
    setOutcomes((prev) => ({ ...prev, [modelId]: undefined }));
  }, []);

  const needsBaseUrl = useCallback(
    // The override describes the *selection* that produced it (D14): show it
    // for any explicit protocol, not just `Responses`.
    (modelId: string) => protocolOfOption(selections[modelId] ?? "follow") !== null,
    [selections],
  );

  const badgeFor = useCallback(
    (model: ModelInfo): ProtocolBadge => {
      const selection = selections[model.id] ?? protocolOptionOf(model.protocol_selected);
      const selectedWire = protocolOfOption(selection);
      const outcome = outcomes[model.id];
      // A manual re-verify in this dialog answers for the current selection,
      // whichever protocol that is — it overrides the (possibly older) stored
      // projection until the save re-plans the probe.
      if (selectedWire && outcome) {
        if (!outcome.ok) {
          return { kind: "blocked", reason: "failed", detail: outcome.detail };
        }
        if (selectedWire === RESPONSES_PROTOCOL) {
          return outcome.state === "degraded"
            ? { kind: "degraded", stripped: outcome.strippedFields }
            : { kind: "verified" };
        }
        // A passing non-Responses verdict enables the override; the `R` chip
        // means "Responses", so no chip visual applies here.
        return { kind: "none" };
      }
      // Before the save the Gateway's fields describe the *stored* state; the
      // dialog overlays the user's not-yet-saved choice so the badge answers
      // "what happens if I save now".
      const pendingChange = selectedWire !== (model.protocol_selected ?? null);
      const fields = {
        shape: model.shape,
        protocol_selected: selectedWire ?? undefined,
        protocol_active: pendingChange
          ? undefined // Pending change: nothing is enabled until the save probes it.
          : model.protocol_active,
        protocol_probe: model.protocol_probe,
      };
      return deriveProtocolBadge(fields);
    },
    [selections, outcomes],
  );

  const buildIntent = useCallback(
    (models: ModelInfo[]) => {
      const model_protocols: ModelProtocolsMap = {};
      const model_base_urls: Record<string, string> = {};
      for (const m of models) {
        const selection =
          selections[m.id] ?? protocolOptionOf(m.protocol_selected);
        const wire = protocolOfOption(selection);
        const stored = m.protocol_selected ?? null;
        if (wire !== stored) model_protocols[m.id] = wire;
        const url = (baseUrls[m.id] ?? "").trim();
        if (url) model_base_urls[m.id] = url;
      }
      return { model_protocols, model_base_urls };
    },
    [selections, baseUrls],
  );

  const estimate = useCallback(
    (models: ModelInfo[]): ProbeEstimate =>
      estimateProbeRequests(
        models.map((m) => {
          const option = selections[m.id] ?? protocolOptionOf(m.protocol_selected);
          const explicit = protocolOfOption(option);
          // The **effective** selection: an explicit choice, or the
          // provider-level protocol when the row follows the data. The
          // estimate has to mirror the Gateway's planning rule, and that rule
          // is stated over the effective selection (ADR-103 D14).
          const effective = explicit ?? m.provider_protocol ?? undefined;
          return {
            previousSelected: m.protocol_selected,
            nextSelected: effective,
            isNew: m.protocol_selected === undefined,
            providerProtocol: m.provider_protocol,
          };
        }),
      ),
    [selections],
  );

  const verify = useCallback(
    async (
      providerId: string,
      modelId: string,
      protocol: string,
    ): Promise<ProbeOutcomeSummary> => {
      const url = `${getGatewayUrl()}/api/providers/${encodeURIComponent(
        providerId,
      )}/probe?force=true`;
      setVerifying(modelId);
      try {
        // Plain `fetch`: the auth interceptor (lib/authFetch.ts) attaches the
        // bearer token for Gateway-origin requests.
        const resp = await fetch(url, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          // D14: probe the route the selection actually claims, and write the
          // verdict back about *that* protocol.
          body: JSON.stringify({ model: modelId, protocol }),
        });
        const body = await resp.json().catch(() => undefined);
        const summary = summarizeProbeResponse(body, resp.status);
        if (!resp.ok && !summary.detail) {
          summary.detail =
            (body as { error?: string } | undefined)?.error ?? `HTTP ${resp.status}`;
        }
        setOutcomes((prev) => ({ ...prev, [modelId]: summary }));
        return summary;
      } catch (e) {
        const summary: ProbeOutcomeSummary = {
          ok: false,
          state: "error",
          detail: e instanceof Error ? e.message : String(e),
          strippedFields: [],
        };
        setOutcomes((prev) => ({ ...prev, [modelId]: summary }));
        return summary;
      } finally {
        setVerifying(null);
      }
    },
    [],
  );

  return useMemo(
    () => ({
      selections,
      baseUrls,
      outcomes,
      verifying,
      hydrate,
      setSelection,
      setBaseUrl,
      badgeFor,
      needsBaseUrl,
      buildIntent,
      estimate,
      verify,
    }),
    [
      selections,
      baseUrls,
      outcomes,
      verifying,
      hydrate,
      setSelection,
      setBaseUrl,
      badgeFor,
      needsBaseUrl,
      buildIntent,
      estimate,
      verify,
    ],
  );
}