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
 * - the live service's port is refused on any host, with no override. The set
 *   is `RESERVED_PORTS` below, which `E2E_RESERVED_PORTS` (comma separated) may
 *   ADD to and can never empty, so the refusal needs no configuration to work;
 * - a loopback host on the harness's own port range is allowed;
 * - anything else needs `E2E_ALLOW_NON_LOCAL_TARGET=true`.
 */

/**
 * Ports that are never a valid local test target, whatever the opt-in.
 *
 * HARD-CODED, AND THE VARIABLE ONLY ADDS TO IT. This was a lone
 * `LIVE_SERVICE_PORT = 3100` constant and briefly became "whatever
 * `E2E_RESERVED_PORTS` says", which is a different and weaker control: with the
 * variable unset the set was EMPTY, so the no-override refusal below could not
 * fire at all and the live port fell through to the loopback/range check -
 * where `E2E_ALLOW_NON_LOCAL_TARGET=true` lets it through. An unconditional
 * refusal had become an overridable one, and nothing in a run says so.
 *
 * A guard that is inert unless somebody sets a variable is not a guard. So the
 * known port stays in code (it is already published in
 * `docker/nginx/example.conf`, so this discloses nothing new) and the variable
 * is a UNION with it: an environment may reserve MORE ports, never fewer.
 */
const RESERVED_PORTS: readonly number[] = [3100];

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

  // Union, not replacement: the variable can only widen the refusal.
  const reserved = new Set<number>(RESERVED_PORTS);
  for (const part of (env[RESERVED_PORTS_VARIABLE] ?? '').split(',')) {
    const text = part.trim();
    if (text === '') continue; // an unset or empty variable, or a trailing comma
    const p = Number(text);
    // Dropping a mistyped entry would leave the operator believing a port is
    // reserved when nothing was added.
    if (!Number.isInteger(p) || p <= 0 || p > 65535) {
      throw new Error(
        `${RESERVED_PORTS_VARIABLE} entry "${text}" is not a port number: refusing to run in local mode. ` +
          'Use a comma-separated list such as "3001,3002".',
      );
    }
    reserved.add(p);
  }

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
    if (reserved.has(port)) {
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
