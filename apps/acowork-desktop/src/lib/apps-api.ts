/**
 * Hosted-application API client (ADR-116).
 *
 * All calls go through the Gateway (`getGatewayUrl()`); the Desktop never
 * addresses an app's loopback port directly. The global authFetch interceptor
 * attaches the Bearer token, so none of these functions handle credentials.
 */
import { getGatewayUrl } from "./config";
import { readCatalog, writeCatalog } from "./catalogCache";

export interface AppDto {
    id: string;
    name: string;
    version: string;
    description: string;
    icon: string;
    builtin: boolean;
    enabled: boolean;
    status: string;
    port: number | null;
    runtime: string;
    mcp_path: string;
    mcp_gateway_path: string;
    mcp_url: string | null;
    ui_path: string;
    ui_mode: string;
    /** ADR-121 preview channel: `library` | `visual` | `none`. */
    canvas_kind?: string;
    /** Template such as `/?file={workspace_file}` for visual apps. */
    canvas_preview_path?: string;
    /** Workspace file extensions this visual app owns (no leading dot). */
    canvas_extensions?: string[];
    nav_label: string;
    nav_icon: string;
    nav_order: number;
    health_path: string;
    permissions: string[];
    installed_version?: string | null;
    /** Unix-second install stamp — nav slots append in install order. */
    installed_at?: string | null;
    fingerprint?: string | null;
    origin?: string | null;
}

export interface AppVersionInfo {
    version: string;
    installed_at: string;
    fingerprint?: string | null;
    current: boolean;
}

export class AppApiError extends Error {
    code?: string;
    constructor(public status: number, message: string, code?: string) {
        super(message);
        this.name = "AppApiError";
        this.code = code;
    }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
    const res = await fetch(`${getGatewayUrl()}${path}`, init);
    if (!res.ok) {
        let message = res.statusText;
        let code: string | undefined;
        try {
            const body = await res.json();
            message = body?.error ?? body?.message ?? message;
            code = body?.code;
        } catch {
            // Non-JSON error body: keep the status text.
        }
        throw new AppApiError(res.status, message, code);
    }
    if (res.status === 204) {
        return undefined as T;
    }
    return (await res.json()) as T;
}

export async function listApps(): Promise<AppDto[]> {
    const body = await request<{ apps: AppDto[] }>("/api/apps");
    const apps = body.apps ?? [];
    // Persist for the optimistic nav seed (see catalogCache.ts). Keyed by
    // Gateway: switching deployments must not paint the other Gateway's
    // installs while the fresh list is in flight.
    writeCatalog(installedAppsCacheKey(), apps);
    return apps;
}

function installedAppsCacheKey(): string {
    return `installedApps:${getGatewayUrl()}`;
}

/**
 * The last successfully fetched app list for the CURRENT Gateway, or `null`.
 * Synchronous — the nav rail seeds its first frame from this so installed
 * apps and their slots exist before `/api/apps` answers (the Gateway may
 * still be starting). Reconciliation is the caller's job: the live list
 * overwrites the seed whenever it differs.
 */
export function readCachedInstalledApps(): AppDto[] | null {
    return readCatalog<AppDto[]>(
        installedAppsCacheKey(),
        (v) => Array.isArray(v),
    );
}

export async function setAppEnabled(id: string, enabled: boolean): Promise<void> {
    await request(`/api/apps/${encodeURIComponent(id)}/${enabled ? "enable" : "disable"}`, {
        method: "POST",
    });
}

/**
 * Mint a short-lived capability ticket for the app's sandboxed iframe.
 * The ticket is passed in the URL fragment — never a query string, never
 * localStorage — and exchanged for a scoped session inside the app channel.
 */
export async function mintAppTicket(id: string): Promise<string> {
    const body = await request<{ ticket: string }>(
        `/api/apps/${encodeURIComponent(id)}/session-ticket`,
        { method: "POST" },
    );
    return body.ticket;
}

/** The Gateway refuses a package it has to stream past this size (`MAX_APP_PACKAGE_BYTES`). */
const MAX_APP_PACKAGE_BYTES = 200 * 1024 * 1024;

/**
 * One install running (or just finished) on the Gateway.
 *
 * The Gateway owns this state, not the view that started it: that is what
 * makes a running install survive switching panels (the panel remounts and
 * refetches `listInstallJobs`), and what lets the Gateway answer a duplicate
 * submit with the job that is already downloading.
 */
export interface InstallJobDto {
    job_id: string;
    /** Card the Desktop asked for. Display key only — never the installed id. */
    hint_id?: string | null;
    phase: "download" | "verify" | "install" | "start" | "done" | "failed";
    received_bytes: number;
    /** 0 when the server sent no `Content-Length`. */
    total_bytes: number;
    started_at: number;
    finished_at?: number | null;
    app_id?: string | null;
    installed_version?: string | null;
    error_code?: string | null;
    error?: string | null;
}

/** Whether a job still needs polling. */
export function installJobActive(job: InstallJobDto): boolean {
    return job.phase !== "done" && job.phase !== "failed";
}

/** Download progress in whole percent, or `null` when the size is unknown. */
export function installJobPct(job: InstallJobDto): number | null {
    if (!job.total_bytes) return null;
    return Math.min(100, Math.max(0, Math.round((job.received_bytes / job.total_bytes) * 100)));
}

/**
 * Hand an uploaded `.app` to the Gateway. The upload is a loopback copy; the
 * slow part (unpack, signature, spawn) runs as a job the caller polls.
 */
export async function startAppInstallFromPackage(
    file: Blob,
    hintId?: string,
): Promise<InstallJobDto> {
    // Checked here because the refusal that matters is the body cap: it answers
    // 413 without the CORS headers the renderer needs, so the browser reports
    // "Failed to fetch" and the user never learns what went wrong.
    if (file.size > MAX_APP_PACKAGE_BYTES) {
        throw new AppApiError(413, "应用包超过 200 MB 限制", "package_too_large");
    }
    const query = hintId ? `?hint_id=${encodeURIComponent(hintId)}` : "";
    return asInstallJob(
        await request<InstallJobDto>(`/api/apps/install${query}`, {
            method: "POST",
            headers: { "Content-Type": "application/octet-stream" },
            body: file,
        }),
    );
}

/**
 * Hand a gallery / Release URL install to the Gateway (it verifies sha256
 * while downloading). Answers immediately with the job.
 */
export async function startAppInstallFromUrl(
    url: string,
    sha256: string,
    hintId?: string,
): Promise<InstallJobDto> {
    return asInstallJob(
        await request<InstallJobDto>("/api/apps/install-url", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ url, sha256, hint_id: hintId ?? null }),
        }),
    );
}

/**
 * A Gateway built before install jobs answers the install routes with a plain
 * result and has no `/api/apps/install-jobs`. Say so instead of rendering a
 * job-less card that never moves.
 */
function asInstallJob(body: InstallJobDto): InstallJobDto {
    if (!body?.job_id) {
        throw new AppApiError(500, "网关需要重新构建以支持安装任务（install jobs）", "install_jobs_unsupported");
    }
    return body;
}

/** Every job the Gateway knows, running first. Poll this while one is active. */
export async function listInstallJobs(): Promise<InstallJobDto[]> {
    const body = await request<{ jobs: InstallJobDto[] }>("/api/apps/install-jobs");
    return body.jobs ?? [];
}

export async function listAppVersions(id: string): Promise<{ current: string; versions: AppVersionInfo[] }> {
    return request(`/api/apps/${encodeURIComponent(id)}/versions`);
}

export async function rollbackApp(id: string, version: string): Promise<void> {
    await request(`/api/apps/${encodeURIComponent(id)}/rollback`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ version }),
    });
}

export async function uninstallApp(id: string, purgeData = false): Promise<void> {
    const q = purgeData ? "?purge_data=true" : "";
    await request(`/api/apps/${encodeURIComponent(id)}${q}`, { method: "DELETE" });
}

/**
 * Stable install error codes → workbench copy.
 *
 * `package_malformed` covers a dozen unrelated gateway failures (zip entry,
 * missing manifest, and every download error), and the localized line alone
 * reads as "your package is bad" while the truth may be a reset mid-download.
 * So the gateway's own detail is kept alongside it.
 */
const INSTALL_ERROR_COPY: Record<string, string> = {
    package_too_large: "应用包超过 200 MB 限制",
    package_malformed: "应用包格式无效",
    package_tampered: "应用包已被篡改",
    unsigned_package: "未签名的应用包已被拒绝",
    signer_mismatch: "签名证书与已安装版本不一致",
    app_api_unsupported: "应用契约版本不受当前平台支持",
    platform_unsupported: "应用包不支持当前操作系统/架构",
    app_id_reserved: "不能覆盖内置应用",
    version_exists: "该版本已安装",
    binary_missing: "应用包缺少可执行文件",
    platform_signature_required: "该应用需要平台签名",
    app_not_installed: "应用未安装",
    invalid_app_id: "应用 ID 无效",
    gateway_unavailable: "网关数据目录不可用",
    uninstall_failed: "卸载失败：安装目录被占用或无法移入回收站",
};

/** Copy for a failure reported by a code + detail pair (a failed install job). */
export function installErrorText(code?: string | null, message?: string | null): string {
    const localized = code ? INSTALL_ERROR_COPY[code] : undefined;
    if (!localized) return message || "";
    return message ? `${localized}（${message}）` : localized;
}

/** Copy for a failure that rejected the request itself. */
export function installErrorMessage(err: unknown): string {
    if (err instanceof AppApiError) return installErrorText(err.code, err.message);
    return err instanceof Error ? err.message : String(err);
}
