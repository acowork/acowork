/**
 * Unified API-error mapping (ADR-087 D5.3).
 *
 * Permission denials reach the Desktop in three wire shapes today:
 *
 *   1. flat gateway   `{"error":"forbidden","code":"not_authorized",
 *                       "resource":"agent","required":"manage"}`
 *      — the ACL middleware and every Gateway `ApiError` 403.
 *   2. nested doc     `{"error":{"code":"not_authorized","message":"…"}}`
 *      — the acowork-doc / acowork-pm services behind the reverse proxy.
 *   3. runtime string `{"error":"<human string>"}`
 *      — proxied Runtime handlers that answer with their own text.
 *
 * Rather than make every call site re-learn the shapes, this module
 * parses all three once (`httpApiError`) and turns a denial into a
 * localized, actionable message — "ask the owner to grant you access"
 * instead of a bare `HTTP 403`. Stores and components should store /
 * display `HttpApiError.message` (already localized) instead of raw
 * `e.message` strings.
 */

import i18n from "../i18n";

/** The canonical machine code every permission denial carries. */
export const NOT_AUTHORIZED = "not_authorized";

/** An HTTP failure with the wire shape already parsed off. */
export class HttpApiError extends Error {
  readonly status: number;
  /** Machine-readable code: `not_authorized`, a doc-service code, or `http_error`. */
  readonly code: string;
  /** For denials: the capability tier that refused (`use`/`manage`/`admin`/`transfer`). */
  readonly required?: string;
  /** Server-provided human detail, kept for logs — UI shows `message`. */
  readonly detail?: string;

  constructor(
    status: number,
    code: string,
    message: string,
    required?: string,
    detail?: string,
  ) {
    super(message);
    this.name = "HttpApiError";
    this.status = status;
    this.code = code;
    this.required = required;
    this.detail = detail;
  }
}

/** Localized copy for a denial, refined by the refusing tier. */
export function permissionMessage(required?: string | null): string {
  switch (required) {
    case "use":
      return i18n.t("apiError.permissionUse");
    case "view":
      return i18n.t("apiError.permissionView");
    case "edit":
      return i18n.t("apiError.permissionEdit");
    case "manage":
      return i18n.t("apiError.permissionManage");
    case "admin":
      return i18n.t("apiError.permissionAdmin");
    case "transfer":
      return i18n.t("apiError.permissionTransfer");
    default:
      return i18n.t("apiError.permissionDenied");
  }
}

interface ParsedBody {
  code: string;
  required?: string;
  detail?: string;
}

/** Extract the machine code + detail from any of the three wire shapes. */
function parseBody(body: unknown): ParsedBody | null {
  if (typeof body !== "object" || body === null) return null;
  const b = body as Record<string, unknown>;
  // Shape 2: nested service envelope.
  if (typeof b.error === "object" && b.error !== null) {
    const e = b.error as Record<string, unknown>;
    return {
      code: typeof e.code === "string" ? e.code : "http_error",
      detail: typeof e.message === "string" ? e.message : undefined,
    };
  }
  // Shape 1: flat gateway body.
  if (typeof b.code === "string") {
    return {
      code: b.code,
      required: typeof b.required === "string" ? b.required : undefined,
      detail: typeof b.message === "string" ? b.message : undefined,
    };
  }
  // Shape 3: runtime string / `{"error":"not found"}`.
  if (typeof b.error === "string") {
    return { code: b.error, detail: b.error };
  }
  return null;
}

/**
 * Turn a non-OK `Response` into an `HttpApiError` whose `message` is
 * already user-facing. Consumes the body — call only when `!res.ok`.
 */
export async function httpApiError(res: Response): Promise<HttpApiError> {
  const parsed = parseBody(await res.json().catch(() => null));
  const code = parsed?.code ?? "http_error";
  const isDenied = res.status === 403 || code === NOT_AUTHORIZED;
  const message = isDenied
    ? permissionMessage(parsed?.required)
    : (parsed?.detail ?? `HTTP ${res.status}`);
  return new HttpApiError(res.status, code, message, parsed?.required, parsed?.detail);
}

/**
 * Map a thrown value to a user-facing message. Duck-types on
 * `status`/`code` fields so `DocApiError` (which cannot import this
 * module without a cycle) is covered without an instanceof check.
 */
export function friendlyErrorMessage(e: unknown, fallback: string): string {
  const err = e as { status?: number; code?: string; required?: string; message?: string };
  if (err && (err.status === 403 || err.code === NOT_AUTHORIZED)) {
    return permissionMessage(err.required);
  }
  if (e instanceof Error && e.message) return e.message;
  return fallback;
}
