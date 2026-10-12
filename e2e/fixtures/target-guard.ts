/**
 * Refuses to start a local-mode run against anything but a local test server.
 *
 * Local mode truncates databases, registers accounts and deletes them. The
 * only thing that separates a test server on this machine from the live
 * service on the same machine is a port number, so one mistyped digit in
 * `E2E_API_URL` would otherwise run those writes against real data. Remote mode
 * (`E2E_REMOTE=true`) is exempt: it targets a deployed environment on purpose.
 *
 * Rules, for each of `E2E_API_URL` and `E2E_BASE_URL`:
 * - the live service's port is refused on any host, with no override. The
 *   number comes from `E2E_RESERVED_PORTS` (comma separated), not from this
 *   file; the local port ranges below stop short of it regardless, so with the
 *   variable unset it is still refused unless the opt-in is given;
 * - a loopback host on the harness's own port range is allowed;
 * - anything else needs `E2E_ALLOW_NON_LOCAL_TARGET=true`.
 */

/** Ports that are never a valid local test target, whatever the opt-in. */
export const RESERVED_PORTS_VARIABLE = 'E2E_RESERVED_PORTS';

/** Ports a local test server or frontend dev server may use. */
const LOCAL_PORT_RANGES: Record<string, [number, number]> = {
  E2E_API_URL: [3000, 3099],
  E2E_BASE_URL: [8000, 8099],
};

// `URL.hostname` drops the brackets from an IPv6 literal; keep both spellings.
const LOOPBACK_HOSTS = new Set(['localhost', '127.0.0.1', '::1', '[::1]']);

export const OPT_IN_VARIABLE = 'E2E_ALLOW_NON_LOCAL_TARGET';

type Env = Record<string, string | undefined>;

/** Throws, naming the variable and the reason, if the target is not allowed. */
export function assertSafeTarget(env: Env): void {
  // Strict compare, as everywhere else: `E2E_REMOTE=false` must not mean remote.
  if (env.E2E_REMOTE === 'true') return;

  const reserved = (env[RESERVED_PORTS_VARIABLE] ?? '')
    .split(',')
    .map((p) => Number(p.trim()))
    .filter((p) => Number.isInteger(p) && p > 0);

  for (const [name, [low, high]] of Object.entries(LOCAL_PORT_RANGES)) {
    const raw = env[name];
    if (!raw) continue; // the local defaults are inside the range
    let url: URL;
    try {
      url = new URL(raw);
    } catch {
      throw new Error(`${name} is not a valid URL: refusing to run in local mode`);
    }
    const port = Number(url.port || (url.protocol === 'https:' ? 443 : 80));
    if (reserved.includes(port)) {
      throw new Error(
        `${name} names the live service's port (${port}): refusing to run in local mode. ` +
          'There is no override for this.',
      );
    }
    const local = LOOPBACK_HOSTS.has(url.hostname) && port >= low && port <= high;
    if (!local && env[OPT_IN_VARIABLE] !== 'true') {
      throw new Error(
        `${name}=${url.origin} is not loopback on ports ${low}-${high}: refusing to run in local mode. ` +
          `Set ${OPT_IN_VARIABLE}=true if this target is intended, or E2E_REMOTE=true for a deployed environment.`,
      );
    }
  }
}
