/**
 * Hosted-application API client (ADR-116).
 *
 * All calls go through the Gateway (`getGatewayUrl()`); the Desktop never
 * addresses an app's loopback port directly. The global authFetch interceptor
 * attaches the Bearer token, so none of these functions handle credentials.
 */
import { getGatewayUrl } from "./config";

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
    nav_label: string;
    nav_icon: string;
    nav_order: number;
    health_path: string;
    permissions: string[];
    installed_version?: string | null;
    fingerprint?: string | null;
    origin?: string | null;
}

export interface AppVersionInfo {
    version: string;
    installed_at: string;
    fingerprint?: string | null;
    current: boolean;
}

export interface InstallResult {
    installed_version: string;
    fingerprint?: string | null;
    origin?: string;
    app?: AppDto;
    app_id?: string;
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
    return body.apps ?? [];
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

/** Upload a local `.app` package (`Content-Type: application/octet-stream`). */
export async function installAppPackage(file: Blob, onProgress?: (pct: number) => void): Promise<InstallResult> {
    // Progress is best-effort: fetch has no upload progress without XHR.
    onProgress?.(0);
    const result = await request<InstallResult>("/api/apps/install", {
        method: "POST",
        headers: { "Content-Type": "application/octet-stream" },
        body: file,
    });
    onProgress?.(100);
    return result;
}

/** Install from a gallery / Release URL (Gateway verifies sha256). */
export async function installAppFromUrl(url: string, sha256: string): Promise<InstallResult> {
    return request<InstallResult>("/api/apps/install-url", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ url, sha256 }),
    });
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

/** Map stable install error codes to Chinese copy for the workbench. */
export function installErrorMessage(err: unknown): string {
    if (err instanceof AppApiError && err.code) {
        const map: Record<string, string> = {
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
        };
        return map[err.code] ?? err.message;
    }
    return err instanceof Error ? err.message : String(err);
}
