/**
 * Workspace HTML preview URLs (ADR-112).
 *
 * Why this module exists
 * ----------------------
 * The workspace HTML preview must be a **real HTTP document**. A page's CSS /
 * JS / fonts / images are sub-resource loads performed by the *browser*, and a
 * browser load cannot attach an `Authorization` header. Serving the page from a
 * Blob URL with a `<base href>` aimed at the bearer-gated `/workspace-files/…`
 * route therefore 401s every sub-resource and paints an unstyled skeleton —
 * exactly the "布局一塌糊涂" bug this module fixes (ADR-112 §决策 1).
 *
 * The Gateway answers with a **capability URL**: an authenticated mint call
 * returns a ticket that rides in the *path* of a read-only preview route, so
 * the document and every relative sub-resource carry the credential for free
 * (a `?ticket=` query would be dropped by relative resolution — §决策 2).
 *
 * Boundaries
 * ----------
 * - This is a *fetch target builder*. The ticket is minted over the Gateway's
 *   bearer-authenticated API (the app-wide interceptor in `authFetch.ts`
 *   attaches the token), then handed to an `<iframe>` as a URL. Unlike
 *   `workspaceFileBlob.ts` there are deliberately no bytes here: the browser
 *   streams the page itself, which is the whole point — a blob would lose the
 *   relative-path context that makes the page render.
 * - A ticket is minted **immediately before each navigation**, not cached
 *   across them. The signing key is per Gateway process, so a restart
 *   invalidates every outstanding ticket — and a client cannot observe a
 *   restart, so any client-side cache would hold an unverifiable ticket until
 *   the Gateway happened to say otherwise. Minting per open costs one
 *   sub-millisecond HMAC round-trip on the local Gateway and removes the whole
 *   failure mode (§决策 5). Concurrency is still deduped: the document and the
 *   sub-resources it references are one navigation, one ticket.
 * - The 8-hour TTL therefore bounds only how long an **already-open** document
 *   keeps reloading its own sub-resources, not how long a link may be reused.
 */

import { getGatewayUrl } from "./config";

interface MintedTicket {
    ticket: string;
}

/**
 * Mint calls currently in flight, keyed by workspace. Holds **no** settled
 * ticket — see the module doc for why a cached capability is not safe here.
 */
const inFlight = new Map<string, Promise<MintedTicket>>();

function cacheKey(agentId: string, workspaceId: string): string {
    return `${agentId}:${workspaceId}`;
}

/**
 * Percent-encode each slash-delimited segment, preserving `/` separators.
 * Mirrors `workspaceFileBlob.ts` and the Gateway's `encode_path_segments`: the
 * route takes `{*path}` verbatim, so spaces / non-ASCII must be escaped
 * per-segment or the Gateway cannot re-derive the file path.
 */
function encodePathSegments(path: string): string {
    return path
        .replace(/\\/g, "/")
        .split("/")
        .filter((segment) => segment.length > 0)
        .map(encodeURIComponent)
        .join("/");
}

/** `POST /api/agents/{id}/workspaces/preview-ticket` (ADR-112 §决策 3). */
async function requestTicket(agentId: string, workspaceId: string): Promise<MintedTicket> {
    const base = getGatewayUrl().replace(/\/+$/, "");
    const url = `${base}/api/agents/${encodeURIComponent(agentId)}/workspaces/preview-ticket`;

    const response = await fetch(url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ workspace_id: workspaceId }),
    });
    if (!response.ok) {
        throw new Error(`preview ticket request failed: HTTP ${response.status}`);
    }

    const data = (await response.json()) as {
        ticket: string;
        expires_at: number; // Unix seconds
        ttl_secs: number;
    };
    if (!data.ticket || typeof data.expires_at !== "number") {
        throw new Error("preview ticket response was missing ticket/expires_at");
    }

    return { ticket: data.ticket };
}

/**
 * Mint a fresh ticket for this workspace.
 *
 * Always a new ticket, never a cached one: a Gateway restart rotates the
 * signing key and nothing in the Desktop can observe that, so the only way a
 * link is guaranteed to verify is to mint it immediately before handing it to
 * the browser (module doc).
 *
 * Concurrent callers for the same workspace share one in-flight mint, so a
 * double-mounted component or two preview tabs opened back-to-back still make
 * one call.
 */
export async function getPreviewTicket(agentId: string, workspaceId: string): Promise<string> {
    const key = cacheKey(agentId, workspaceId);

    const pending = inFlight.get(key);
    if (pending) return (await pending).ticket;

    const minting = requestTicket(agentId, workspaceId).finally(() => {
        // Settled either way: a cached *failure* would make every later open
        // replay it, so the map only ever holds work in progress.
        inFlight.delete(key);
    });

    inFlight.set(key, minting);
    return (await minting).ticket;
}

/**
 * Compose the URL the preview iframe navigates to. Pure, so the encoding
 * contract is testable without a network call.
 */
export function composePreviewUrl(params: {
    gatewayUrl: string;
    ticket: string;
    agentId: string;
    workspaceId: string;
    relPath: string;
}): string {
    const base = params.gatewayUrl.replace(/\/+$/, "");
    const segments = [
        "workspace-preview",
        params.ticket,
        encodeURIComponent(params.agentId),
        encodeURIComponent(params.workspaceId),
        encodePathSegments(params.relPath),
    ];
    return `${base}/${segments.join("/")}`;
}

/**
 * The renderable URL of one workspace file, ticket included.
 *
 * Throws when the mint fails; callers render an error card rather than falling
 * back to the old blob path — a silent fallback would resurrect the 401 bug
 * with no way to tell it apart from a working preview.
 */
export async function getWorkspacePreviewUrl(
    agentId: string,
    workspaceId: string,
    relPath: string,
): Promise<string> {
    const ticket = await getPreviewTicket(agentId, workspaceId);
    return composePreviewUrl({
        gatewayUrl: getGatewayUrl(),
        ticket,
        agentId,
        workspaceId,
        relPath,
    });
}

/** Test hook: forget every in-flight mint. */
export function __resetWorkspacePreviewUrlCache(): void {
    inFlight.clear();
}
