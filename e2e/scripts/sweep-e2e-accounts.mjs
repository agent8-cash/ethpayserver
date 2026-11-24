#!/usr/bin/env node
/**
 * Remove the accounts abandoned mid-registration against a live deployment.
 *
 * `tests/scout.spec.ts` (and, for a few days before it was disabled here,
 * the scheduled workflow's own `browser` job) registers a fresh passkey
 * account on every run. Most of what it exercises never gets further than
 * that: no email, no wallet, no store. Each one is small, but there is no
 * job that ever removes them, so they only ever accumulate.
 *
 * This targets exactly that residue: accounts with no email and no wallet
 * address - the two things that would let a real owner reach one - that also
 * own no store at all, or whose every store is one of the
 * `synthetic-payment.spec.ts` runs this sweep's sibling
 * (`sweep-e2e-stores.mjs`) already knows how to name and clear. The email/
 * wallet check matters on its own: a merchant who signed up with real
 * contact details but has not created a first store yet would otherwise look
 * identical to this residue on the store check alone. Deletion goes through
 * `DELETE /admin/users/{id}`
 * (`server/src/api/admin/mod.rs`), the same cascade a merchant gets from
 * `DELETE /users/me` - it refuses on its own if the account ever took a
 * payment, payout or refund, so this script cannot use it to destroy
 * financial history even by mistake.
 *
 *   E2E_API_URL=https://testnet.agent8.cash E2E_REMOTE=true \
 *   E2E_API_TOKEN=ak_... node scripts/sweep-e2e-accounts.mjs [--execute]
 *
 * The token must belong to a ServerAdmin - the admin endpoints this script
 * calls refuse anyone else. Dry run by default: it lists what it would
 * remove and changes nothing. Only `--execute` issues deletes, because this
 * runs against a live server and the rows are real.
 */

/** Same shape `synthetic-payment.spec.ts` builds its store names from. */
const SYNTHETIC_STORE_NAME = /^e2e-synthetic-\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-\d{3}Z$/;

/**
 * Never delete this account, whatever the query above matches.
 *
 * It holds the xpub `synthetic-payment.spec.ts` pays into and, on a fresh
 * instance, may be the only `server_admin` there is. The admin endpoint
 * already refuses to delete any `server_admin`, so this is a second,
 * redundant guard on top of that one - the two are cheap insurance against
 * each other's bugs, not a bet that either one alone is enough.
 */
const PROTECTED_USER_IDS = new Set(['c5ae10f2-da34-4002-af3b-5a4ec8b6ec97']);

function requireEnv(name, why) {
  const v = process.env[name];
  if (!v) {
    console.error(`${name} is required — ${why}`);
    process.exit(1);
  }
  return v;
}

const execute = process.argv.slice(2).includes('--execute');

const apiUrl = requireEnv(
  'E2E_API_URL',
  'the server to sweep, e.g. https://testnet.agent8.cash',
).replace(/\/$/, '');
const token = requireEnv('E2E_API_TOKEN', 'a ServerAdmin bearer token or API key');
// Same rule as fixtures/api.ts: the deployed client's nginx proxies /api/ to the
// backend and strips the prefix, so a remote base URL needs it and a direct one
// does not.
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

/** Page through `GET /admin/users` — it caps at 200 per page regardless of `limit`. */
async function allUsers() {
  const users = [];
  const pageSize = 200;
  for (let offset = 0; ; offset += pageSize) {
    const page = await api(`/admin/users?limit=${pageSize}&offset=${offset}`);
    users.push(...page.users);
    if (users.length >= page.total || page.users.length === 0) break;
  }
  return users;
}

console.log(`sweeping ${apiUrl}${prefix}`);
console.log(execute ? 'MODE: execute\n' : 'MODE: dry run (pass --execute to delete)\n');

const users = await allUsers();
const candidates = [];
const skippedProtected = [];
const skippedOwnsOther = [];
const skippedIdentified = [];

for (const user of users) {
  if (PROTECTED_USER_IDS.has(user.id) || user.role === 'server_admin') {
    skippedProtected.push(user);
    continue;
  }

  // The residue this sweep exists for is the abandoned-mid-registration case:
  // no email, no wallet, nothing that would let its owner recover it any way
  // other than the passkey Playwright's virtual authenticator holds. An
  // account with either is reachable by a real person, so it is out of scope
  // here even if it also happens to own no store yet - a merchant who just
  // signed up and has not created a first store looks exactly like that on
  // the store check alone.
  if (user.email || user.primary_wallet_address) {
    skippedIdentified.push(user);
    continue;
  }

  const stores = await api(`/admin/users/${user.id}/stores`);
  const foreign = stores.filter((s) => !SYNTHETIC_STORE_NAME.test(s.name));
  if (foreign.length > 0) {
    skippedOwnsOther.push({ user, foreign });
    continue;
  }

  candidates.push({ user, storeCount: stores.length });
}

for (const { user, storeCount } of candidates) {
  const handle = user.email ?? user.id;
  console.log(
    `  ${execute ? 'delete' : 'would delete'}  ${user.id}  ${handle}  (${storeCount} synthetic store(s))`,
  );
}

let failed = 0;
let blocked = 0;
if (execute) {
  for (const { user } of candidates) {
    try {
      await api(`/admin/users/${user.id}`, 'DELETE');
    } catch (err) {
      // A 409 means the account took a payment somewhere along the way -
      // the query above only looked at store *names*, not their financial
      // history, so this is the endpoint's own safeguard catching what the
      // sweep could not see in advance. Keep going either way: one refusal
      // or one failure must not strand the rest of the batch.
      if (err.message.includes('→ 409')) {
        console.log(`  BLOCKED  ${user.id}  ${err.message}`);
        blocked++;
      } else {
        console.error(`  FAILED  ${user.id}  ${err.message}`);
        failed++;
      }
    }
  }
}

console.log(
  `\n${users.length} user(s) visible, ${candidates.length} matching (no email, no wallet, ` +
    `and no stores or only ${SYNTHETIC_STORE_NAME}), ${skippedProtected.length} skipped as ` +
    `admin/protected, ${skippedIdentified.length} skipped for having an email or a wallet, ` +
    `${skippedOwnsOther.length} skipped for owning a non-synthetic store.`,
);
if (!execute && candidates.length > 0) console.log('\nRe-run with --execute to delete.');
if (blocked > 0) {
  console.log(
    `\n${blocked} account(s) refused deletion because they hold financial history - ` +
      `leave them archived rather than forcing this.`,
  );
}
if (failed > 0) {
  console.error(`\n${failed} deletion(s) failed.`);
  process.exit(1);
}
