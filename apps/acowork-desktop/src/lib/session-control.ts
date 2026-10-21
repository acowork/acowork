//! Session control plane over Gateway HTTP (ADR-076 §决策 4).
//!
//! These four operations used to be published as MQTT control commands
//! through the Tauri `mqtt_publish_control` command, which the Desktop
//! sent straight to the broker. That path carries no identity — the broker
//! cannot stamp one — so the Gateway could not authenticate the caller and
//! the Runtime could not record or check a session's owner. They now go
//! through the Gateway's authenticated HTTP API, which forwards the
//! resolved account as a header on the reverse proxy.
//!
//! (`create_session` could not return the new id over MQTT either; the
//! HTTP response carries it. The Desktop still waits for the
//! `session_created` event, so behaviour is unchanged there.)

import { getGatewayUrl } from "./config";
import { with503Retry, WRITE_503_RETRY } from "./httpRetry";
import { log } from "./logger";
import { NOT_AUTHORIZED, permissionMessage } from "./api-error";

/**
 * A control-plane call the Gateway refused on authorization grounds
 * (403 `not_authorized`) or because the resource is filtered away from
 * this account (404 — ADR-087 D5 answers 404 rather than 403 so a
 * private agent's existence is not leaked).
 *
 * Carrying the status as a field (instead of only embedding
 * `HTTP 404` in the message) lets callers distinguish "you may not use
 * this agent" from a transient network/503 failure and render a reason
 * instead of an endless loading state.
 */
export class SessionAccessError extends Error {
  constructor(
    readonly status: number,
    readonly agentId: string,
    message: string,
    /** For 403: the capability tier that refused (`use`/`manage`/…). */
    readonly required?: string,
  ) {
    super(message);
    this.name = "SessionAccessError";
  }
}

/** Did this failure mean "the Gateway refused you", not "try again later"? */
export function isAccessDenied(err: unknown): boolean {
  return err instanceof SessionAccessError && (err.status === 403 || err.status === 404);
}

async function controlRequest(
  method: "POST" | "DELETE" | "PUT",
  path: string,
  body?: unknown,
): Promise<void> {
  // Bug B v3: the Runtime's `session manager not ready` window (between
  // Gateway discovering an agent and its HTTP port landing in the reverse
  // proxy) makes every control op 503 for a second or two. Retry inside
  // the shared helper so all six callers recover transparently instead of
  // each logging a hard error.
  const resp = await with503Retry(
    () =>
      fetch(`${getGatewayUrl()}${path}`, {
        method,
        headers: body === undefined ? undefined : { "content-type": "application/json" },
        body: body === undefined ? undefined : JSON.stringify(body),
      }),
    { policy: WRITE_503_RETRY, tag: `session-control ${method} ${path}`, logger: log },
  );
  if (!resp.ok) {
    const text = await resp.text();
    // 401/403 = authenticated but not authorized; 404 = the resource is
    // not visible to this account (ADR-087 D5). Both are terminal for
    // this agent — a retry cannot help.
    if (resp.status === 401 || resp.status === 403 || resp.status === 404) {
      const agentId = path.split("/")[3] ?? "";
      // ADR-087 D5.3: the Gateway denial is a flat JSON body — extract
      // the refusing tier so callers render "需要使用权/管理权" instead
      // of a raw `HTTP 403: {"error":"forbidden",…}` dump.
      let required: string | undefined;
      try {
        const body = JSON.parse(text) as { code?: string; required?: string };
        if (body?.code === NOT_AUTHORIZED && typeof body.required === "string") {
          required = body.required;
        }
      } catch {
        /* non-JSON denial — fall through to the raw text */
      }
      const message =
        resp.status === 403 && required !== undefined
          ? permissionMessage(required)
          : `HTTP ${resp.status}: ${text}`;
      throw new SessionAccessError(resp.status, agentId, message, required);
    }
    throw new Error(`HTTP ${resp.status}: ${text}`);
  }
}

/** `POST /api/agents/{id}/sessions` — the new session is owned by the caller. */
export function createSession(
  agentId: string,
  body: { workspace_id?: string } = {},
): Promise<void> {
  return controlRequest("POST", `/api/agents/${agentId}/sessions`, body);
}

/** `POST /api/agents/{id}/sessions/{sid}/open` — ADR-038 activation, idempotent. */
export function openSession(agentId: string, sessionId: string): Promise<void> {
  return controlRequest("POST", `/api/agents/${agentId}/sessions/${sessionId}/open`);
}

/**
 * `GET /api/agents/{id}/sessions/{sid}` — is this session still there?
 *
 * Used by the Chat surfaces before a "view run" drill: the message's exec
 * pointer outlives the session it names (design 27 §5.5.5), and opening a
 * deleted session cold would strand the user on an empty exec tab whose
 * every load fails into the retry banner.
 *
 * Only a definitive 404 answers `false`. The Runtime returns 404 exactly
 * when the session has neither persisted meta nor a live snapshot — i.e. it
 * was deleted — while a stopped/booting agent yields 503s and network
 * failures yield none, and those are inconclusive: they answer `true` so a
 * probe hiccup can never block a drill into a session that does exist.
 */
export async function sessionExists(agentId: string, sessionId: string): Promise<boolean> {
  try {
    const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}`);
    return resp.status !== 404;
  } catch {
    // Inconclusive (no connection / Gateway still booting) — let the jump
    // proceed on the existing best-effort path.
    return true;
  }
}

/**
 * `POST /api/agents/{id}/sessions/{sid}/close` — graceful close.
 *
 * Idempotent on the Runtime side (an already-closed session is a no-op),
 * so callers may fire it unconditionally.
 */
export function closeSession(agentId: string, sessionId: string): Promise<void> {
  return controlRequest("POST", `/api/agents/${agentId}/sessions/${sessionId}/close`);
}

/** `DELETE /api/agents/{id}/sessions/{sid}` — removes the session and its files. */
export function deleteSession(agentId: string, sessionId: string): Promise<void> {
  return controlRequest("DELETE", `/api/agents/${agentId}/sessions/${sessionId}`);
}

/**
 * `DELETE /api/agents/{id}/sessions` — clear every session the caller owns.
 * The Runtime removes each session's meta row (SQLite) and its conversation
 * JSONL file; sessions merely shared with the caller (read-only) survive.
 */
export function clearSessions(agentId: string): Promise<void> {
  return controlRequest("DELETE", `/api/agents/${agentId}/sessions`);
}

/**
 * `PUT /api/agents/{id}/sessions/{sid}/workspace` — switch the session's
 * workspace.
 *
 * ADR-076 §决策 4: replaces the MQTT `workspace_switch` command. This is
 * *not* a `patchSessionConfig({ workspace_id })` call — the config path only
 * rewrites meta, leaving the session's tools pointed at the old directory
 * (see the Runtime-side `route_workspace_switch`).
 */
export function setSessionWorkspace(
  agentId: string,
  sessionId: string,
  workspaceId: string,
): Promise<void> {
  return controlRequest("PUT", `/api/agents/${agentId}/sessions/${sessionId}/workspace`, {
    workspace_id: workspaceId,
  });
}

/**
 * `PUT /api/agents/{id}/sessions/{sid}/config` — partial config update.
 *
 * ADR-076 §决策 4: replaces the MQTT `model_switch`, `reasoning_effort` and
 * `update_session_title` commands. Field names are the backend's
 * `SessionConfigDelta` names (snake_case `reasoning_effort`, not the
 * frontend's camelCase), and absent fields are left untouched.
 */
export function patchSessionConfig(
  agentId: string,
  sessionId: string,
  patch: {
    model?: string;
    provider?: string;
    /** Multi-account: which API key of the provider (empty = first). */
    account_id?: string;
    reasoning_effort?: string;
    title?: string;
    temperature?: number;
    context_window?: number;
  },
): Promise<void> {
  return controlRequest("PUT", `/api/agents/${agentId}/sessions/${sessionId}/config`, patch);
}

/**
 * `PUT /api/agents/{id}/sessions/{sid}/visibility` — share or unshare.
 *
 * `null` clears the field, restoring the public default (ADR-076 §决策 4:
 * absent means public, so an unshared-but-unset session is the same state
 * as a fresh one). Not yet wired to any UI — the toggle is reserved.
 */
export function setSessionVisibility(
  agentId: string,
  sessionId: string,
  visibility: "public" | "private" | null,
): Promise<void> {
  return controlRequest("PUT", `/api/agents/${agentId}/sessions/${sessionId}/visibility`, {
    visibility,
  });
}

// ─────────────────────────────────────────────────────────────────────
// Session *actions* (ADR-076 §决策 4, second wave)
//
// These eight calls used to be `invoke("mqtt_publish_control", …)`, which
// published straight to the broker. That path carries no identity, so
// anyone able to reach the broker could drive another account's session —
// including approving a risky tool call (`approved: true`), which is
// arbitrary command execution. They now go through the Gateway's
// authenticated HTTP API like the lifecycle calls above.
//
// The HTTP response only acknowledges routing (202): the action's outcome
// still arrives on the MQTT event plane, so callers keep treating these as
// fire-and-forget, exactly as before.
// ─────────────────────────────────────────────────────────────────────

/** `CompressType::SUMMARY` — the value the context-usage menu sends. */
export const COMPRESS_SUMMARY = 1;

/** `CompressType::CANCEL` (ADR-083) — abort the in-flight context compaction.
 *  Same endpoint as `SUMMARY`; the Runtime routes the `compress_type` to the
 *  running compaction's cancel handle. */
export const COMPRESS_CANCEL = 3;

/** `POST .../sessions/{sid}/messages` — send a user chat message. */
export function sendMessage(
  agentId: string,
  sessionId: string,
  body: {
    content: string;
    message_id: string;
    /** Slash-command *name* only; the Runtime resolves the instructions. */
    command?: string;
    /** Opaque JSON string (not an object) — the Runtime parses it. */
    params_json?: string;
  },
): Promise<void> {
  return controlRequest("POST", `/api/agents/${agentId}/sessions/${sessionId}/messages`, body);
}

/** `POST .../sessions/{sid}/stop` — interrupt the current generation. */
export function stopSession(
  agentId: string,
  sessionId: string,
  reason = "user_requested",
): Promise<void> {
  return controlRequest("POST", `/api/agents/${agentId}/sessions/${sessionId}/stop`, { reason });
}

/** `POST .../sessions/{sid}/continue` — resume after an iteration-limit pause. */
export function continueSession(
  agentId: string,
  sessionId: string,
  reason = "user_requested",
): Promise<void> {
  return controlRequest("POST", `/api/agents/${agentId}/sessions/${sessionId}/continue`, {
    reason,
  });
}

/**
 * `POST .../sessions/{sid}/approval` — the user's tool-risk decision.
 *
 * This is the call that used to be spoofable over MQTT.
 */
export function sendApproval(
  agentId: string,
  sessionId: string,
  body: {
    request_id: string;
    approved: boolean;
    allow_all_session?: boolean;
    reason?: string;
  },
): Promise<void> {
  return controlRequest("POST", `/api/agents/${agentId}/sessions/${sessionId}/approval`, body);
}

/** `POST .../sessions/{sid}/answer` — answer an `ask_user_question` prompt. */
export function sendAnswer(
  agentId: string,
  sessionId: string,
  body: { request_id: string; answer: string },
): Promise<void> {
  return controlRequest("POST", `/api/agents/${agentId}/sessions/${sessionId}/answer`, body);
}

/**
 * `POST .../sessions/{sid}/cancel-tool` — ADR-045: abort a single in-flight
 * tool. The surrounding iteration continues; an unknown `tool_call_id` is a
 * no-op (race against natural completion).
 */
export function cancelTool(
  agentId: string,
  sessionId: string,
  toolCallId: string,
): Promise<void> {
  return controlRequest("POST", `/api/agents/${agentId}/sessions/${sessionId}/cancel-tool`, {
    tool_call_id: toolCallId,
  });
}

/** `POST .../sessions/{sid}/compress` — user-initiated context compression. */
export function compressSession(
  agentId: string,
  sessionId: string,
  compressType: number = COMPRESS_SUMMARY,
): Promise<void> {
  return controlRequest("POST", `/api/agents/${agentId}/sessions/${sessionId}/compress`, {
    compress_type: compressType,
  });
}
