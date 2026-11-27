/**
 * Unified cache-management client (ADR-123 §3).
 *
 * Two calls, both to the Gateway — the Desktop never addresses a service's
 * loopback port, and it never computes a size or a retention rule itself:
 *
 * - `fetchCacheReport()` → `GET /api/cache/report` — every participating
 *   service's rows plus the volume watermark.
 * - `cleanupCache(service, ids)` → `POST /api/cache/cleanup` — names the
 *   categories to sweep; the owner decides how much that frees (`ids` carries
 *   no size, ADR-123 §1.3 red line 3).
 *
 * The global authFetch interceptor attaches the Bearer token, so neither
 * function handles credentials.
 */
import { getGatewayUrl } from "./config";

/** Cache = regenerable/collectable; data = never offered a delete button. */
export type StorageKind = "cache" | "data";

export interface CacheItem {
  /** Stable machine key the cleanup call echoes back. */
  id: string;
  kind: StorageKind;
  bytes: number;
  cleanable: boolean;
}

export interface CacheServiceRow {
  /** Routing key for the cleanup call: `gateway` | `user` | `embed` | `node:<id>`. */
  service: string;
  node_id?: string;
  /** `false` = no answer (process down / node offline), not "0 bytes". */
  reachable: boolean;
  error?: string;
  items: CacheItem[];
}

export interface DiskWatermark {
  total_bytes: number;
  available_bytes: number;
  used_percent: number;
  threshold_percent: number;
  alarm: boolean;
}

export interface CacheReport {
  disk: DiskWatermark;
  services: CacheServiceRow[];
}

export interface CleanupResult {
  /** Bytes the owner actually removed — it measures, we only ask. */
  freed_bytes: number;
}

export class CacheApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
    this.name = "CacheApiError";
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
      /* non-JSON error body — keep the status text */
    }
    throw new CacheApiError(res.status, message || `HTTP ${res.status}`);
  }
  return (await res.json()) as T;
}

/** Every service's storage rows + the volume watermark. */
export function fetchCacheReport(): Promise<CacheReport> {
  return request<CacheReport>("/api/cache/report");
}

/** Ask one service to run its retention sweep for `ids`; answers freed bytes. */
export function cleanupCache(service: string, ids: string[]): Promise<CleanupResult> {
  return request<CleanupResult>("/api/cache/cleanup", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ service, ids }),
  });
}