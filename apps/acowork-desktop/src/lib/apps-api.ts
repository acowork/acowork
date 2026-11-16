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
}

export class AppApiError extends Error {
    constructor(public status: number, message: string) {
        super(message);
        this.name = "AppApiError";
    }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
    const res = await fetch(`${getGatewayUrl()}${path}`, init);
    if (!res.ok) {
        let message = res.statusText;
        try {
            const body = await res.json();
            message = body?.error ?? body?.message ?? message;
        } catch {
            // Non-JSON error body: keep the status text.
        }
        throw new AppApiError(res.status, message);
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
