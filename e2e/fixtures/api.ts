/**
 * Thin REST client for the ethpayserver API, for tests that drive the backend
 * directly rather than through the UI.
 *
 * Path shape differs between modes and the difference is not cosmetic:
 *
 * - local:  `E2E_API_URL=http://localhost:3000` — routes are mounted at the
 *           root, so `/stores` is `/stores`.
 * - remote: `E2E_API_URL=https://testnet.agent8.cash` — the client container's
 *           nginx proxies `/api/` to the backend and strips the prefix
 *           (its nginx config, which now ships inside the payserver-client
 *           image), so `/stores` is `/api/stores`.
 *
 * `E2E_API_PREFIX` overrides the inferred prefix for any other topology.
 *
 * The checkout WebSocket is deliberately *not* under the prefix — nginx routes
 * `/checkout/ws` straight through — so `wsUrl()` builds from the origin.
 */

// `=== 'true'`, not truthiness: `E2E_REMOTE=false` would otherwise select the
// live host and the `/api` prefix, and every call 404s or hits the wrong box.
const REMOTE = process.env.E2E_REMOTE === 'true';

export const API_URL = (
  process.env.E2E_API_URL || (REMOTE ? 'https://testnet.agent8.cash' : 'http://localhost:3000')
).replace(/\/$/, '');

export const API_PREFIX = (
  process.env.E2E_API_PREFIX !== undefined ? process.env.E2E_API_PREFIX : REMOTE ? '/api' : ''
).replace(/\/$/, '');

export class ApiError extends Error {
  constructor(
    readonly method: string,
    readonly path: string,
    readonly status: number,
    readonly body: string,
  ) {
    super(`${method} ${path} → ${status}: ${body.slice(0, 500)}`);
    this.name = 'ApiError';
  }
}

export interface ApiOptions {
  method?: string;
  body?: unknown;
  /** Bearer token — a session UUID or an `ak_...` API key. */
  token?: string;
  headers?: Record<string, string>;
}

/** Call the API and return the parsed JSON body. Throws `ApiError` on non-2xx. */
export async function api<T = unknown>(path: string, opts: ApiOptions = {}): Promise<T> {
  const method = opts.method || 'GET';
  const url = `${API_URL}${API_PREFIX}${path}`;

  const headers: Record<string, string> = { ...opts.headers };
  if (opts.token) headers['Authorization'] = `Bearer ${opts.token}`;
  if (opts.body !== undefined) headers['Content-Type'] = 'application/json';

  const resp = await fetch(url, {
    method,
    headers,
    body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
  });

  const text = await resp.text();
  if (!resp.ok) throw new ApiError(method, path, resp.status, text);
  return (text ? JSON.parse(text) : undefined) as T;
}

/** WebSocket URL for a path served at the origin (not behind `API_PREFIX`). */
export function wsUrl(path: string): string {
  return `${API_URL.replace(/^http/, 'ws')}${path}`;
}

/**
 * The invoice id is a version-4 UUID: 122 random bits from the server's CSPRNG.
 * For the unauthenticated checkout socket that unguessability is the only
 * thing standing between a stranger and an invoice's live status, so a change
 * to a sequential, time-ordered or otherwise predictable id must fail here.
 *
 * Rejecting uppercase is deliberate, not pedantry: the server renders ids
 * lowercase, so an uppercase id means a different generator produced it.
 * Making the pattern case-insensitive would discard that discrimination.
 */
export const V4_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
