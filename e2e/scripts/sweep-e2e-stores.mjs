#!/usr/bin/env node
/**
 * Archive the stores the e2e runs left behind.
 *
 * The synthetic-payment spec and the full e2e suite create stores on every run
 * and for a long time removed none of them. This archives them: out of
 * `GET /stores`, closed to new invoices, but the row and its payment history
 * stay, and `POST /stores/{id}/unarchive` brings one back. Like BTCPay, nothing
 * here destroys history.
 *
 *   E2E_API_URL=https://testnet.agent8.cash E2E_REMOTE=true \
 *   E2E_API_TOKEN=ak_... node scripts/sweep-e2e-stores.mjs [--execute]
 *
 * Dry run by default: it lists what it would archive and changes nothing. Only
 * `--execute` archives, because this runs against a live server. `--execute`
 * refuses any host other than testnet.agent8.cash or localhost unless `--force`.
 *
 * - `GET /stores` returns only the live stores the token's own user can see, so
 *   this can never reach another account's stores, and an already-archived
 *   store is not listed again.
 * - It calls the self-service `DELETE /stores/{id}`, which archives, so any
 *   token able to manage the store works; no admin token is needed. Because
 *   archiving is reversible the old exact-timestamp gate is gone: every store
 *   named `e2e-...` is swept. A store without that prefix is never touched.
 * - It does not call `DELETE /admin/stores/{id}` (a hard delete, still gated to
 *   the synthetic name shape on the server). That is the escape hatch, not
 *   this script's job.
 */

/** Everything the e2e specs name: `e2e-synthetic-<stamp>`, hand-made `e2e-scratch`, and so on. */
const E2E_STORE_PREFIX = 'e2e-';

function requireEnv(name, why) {
  const v = process.env[name];
  if (!v) {
    console.error(`${name} is required — ${why}`);
    process.exit(1);
  }
  return v;
}

const argv = process.argv.slice(2);
const execute = argv.includes('--execute');
const force = argv.includes('--force');

const apiUrl = requireEnv(
  'E2E_API_URL',
  'the server to sweep, e.g. https://testnet.agent8.cash',
).replace(/\/$/, '');
// Archiving closes a store to new invoices, so pointing --execute at mainnet
// with a real merchant's key could silence a live store whose name happens to
// start with `e2e-`. Refuse anything that is not testnet or local unless forced.
if (execute && !force && !/^https?:\/\/(testnet\.random\.cash|localhost|127\.0\.0\.1)(:\d+)?(\/|$)/.test(apiUrl)) {
  console.error(`refusing to --execute against ${apiUrl}: not testnet or local. Pass --force if you mean it.`);
  process.exit(1);
}
const token = requireEnv(
  'E2E_API_TOKEN',
  'API key (ak_...) for the account that owns the stores — archiving is owner-only',
);
// Same rule as fixtures/api.ts: the deployed client's nginx proxies /api/ to the
// backend and strips the prefix, so a remote base URL needs it and a direct one
// does not. Getting it wrong 404s loudly rather than silently finding nothing.
const prefix = (
  process.env.E2E_API_PREFIX !== undefined
    ? process.env.E2E_API_PREFIX
    : process.env.E2E_REMOTE === 'true'
      ? '/api'
      : ''
).replace(/\/$/, '');

async function api(path, method = 'GET') {
  const resp = await fetch(`${apiUrl}${prefix}${path}`, {
    method,
    headers: { Authorization: `Bearer ${token}` },
  });
  const text = await resp.text();
  if (!resp.ok) throw new Error(`${method} ${path} → ${resp.status}: ${text.slice(0, 300)}`);
  return text ? JSON.parse(text) : undefined;
}

console.log(`sweeping ${apiUrl}${prefix}`);
console.log(execute ? 'MODE: execute\n' : 'MODE: dry run (pass --execute to archive)\n');

const stores = await api('/stores');

const matched = stores.filter((s) => s.name.startsWith(E2E_STORE_PREFIX));

for (const store of matched) {
  console.log(`  ${execute ? 'archive' : 'would archive'}  ${store.id}  ${store.name}`);
}

let failed = 0;
if (execute) {
  for (const store of matched) {
    try {
      await api(`/stores/${store.id}`, 'DELETE');
    } catch (err) {
      // Keep going and fail at the end: one 403 must not strand the rest.
      console.error(`  FAILED  ${store.id}  ${store.name} — ${err.message}`);
      failed++;
    }
  }
}

console.log(`\n${stores.length} live store(s) visible to this token, ${matched.length} named "${E2E_STORE_PREFIX}*".`);
if (!execute && matched.length > 0) console.log('\nRe-run with --execute to archive.');
if (failed > 0) {
  console.error(`\n${failed} archive(s) failed.`);
  process.exit(1);
}
