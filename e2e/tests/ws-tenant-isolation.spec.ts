/**
 * Live tenant isolation of the status socket (`/ws`).
 *
 * Two ordinary accounts, A and B, each hold one open socket. A creates and owns
 * store S1 only and B creates and owns S2 only, using their own session ids as
 * bearer tokens. Each adds its own payment method and creates its own invoices,
 * so there is no member route and no database insert: creating a store needs
 * only an authenticated user, who becomes its owner, and owning a store is
 * all the socket's gate asks for.
 *
 * This spec seeds two OWNERS rather than members. It still covers the member
 * path because the gate resolves access solely through `user_stores` and
 * ignores the role, and `create_store_owned_by` writes that row for an owner.
 * If the gate is ever changed to special-case `stores.owner_id`, this spec
 * stops covering membership and a member-specific spec becomes necessary.
 *
 * Events are produced by expiring real invoices (a one-second expiry; the
 * invoice cleanup service broadcasts `expired`), so nothing is paid and no
 * wallet is touched. They are published one at a time, each only after its
 * intended recipient has received it:
 *
 *   1. an invoice in S2   - B must receive it
 *   2. an invoice in S1   - A must receive it
 *   3. a second one in S2 - B must receive it
 *
 * That order is the point. A held an open socket while (1) was broadcast, so
 * receiving (2) at all, with (1) absent from its frames, shows A's socket was
 * live and still withheld the other tenant's event. The same holds for B
 * across (2) and (3). Only A against (3) has nothing published after it, so
 * that one pair gets a bounded settle window instead.
 *
 * Positive controls come first and are required on both sockets: a dead
 * socket also "received nothing foreign".
 *
 * Local and remote run the same path. They differ only in how the admin token
 * is obtained: a server-admin user created straight in the database locally, the
 * `E2E_API_TOKEN` key remotely. The admin owns nothing under test; it only
 * deletes the two accounts afterwards.
 *
 * The isolation verdict does not depend on any expiry arriving. A wait that
 * times out is recorded, the isolation check still runs on the frames that did
 * arrive, and a leak fails the run. With no leak but a missing own event the
 * run fails as NOT VERIFIED (isolation could not run), never as a bare timeout.
 *
 * If an ordinary account is refused invoice creation on standing, setup has
 * not worked and nothing has been tested: that is reported as NOT VERIFIED,
 * naming the status and body, and never as an isolation result.
 *
 * Off unless `E2E_WS_ISOLATION=true`; once on, missing configuration is a hard
 * failure. The remote run needs `E2E_API_TOKEN`, a server admin key.
 */
import { appendFileSync } from 'node:fs';

import { test, expect } from '@playwright/test';
import { randomBytes } from 'node:crypto';
import { HDKey, generatePrivateKey, privateKeyToAccount } from 'viem/accounts';

import { api, ApiError, V4_UUID } from '../fixtures/api';
import { createUserWithApiKey } from '../fixtures/db';
import {
  judgeIsolation,
  type Frame,
  openAuthenticatedSocket,
  type RecordingSocket,
} from '../fixtures/ws-isolation';

const ENABLED = process.env.E2E_WS_ISOLATION === 'true';
const REMOTE = process.env.E2E_REMOTE === 'true';

const CHAIN_ID = 'eip155:11155111';
const MERCHANT_PATH = "m/44'/60'/0'";
/** The cleanup service sweeps on a 60s fallback tick and on block events. */
const EXPIRY_WAIT_MS = 3 * 60_000;
/**
 * An expired invoice keeps its address watched for a grace period, after which
 * the next sweep unwatches it, and deleting an account is refused with 409
 * while any watch is active. On a local server started with a short grace and
 * tick this normally clears within seconds; the generous bound is the fallback
 * for a remote server whose cleanup settings are not ours (60s + 60s by
 * default). Teardown waits for the real state rather than forcing it.
 */
const UNWATCH_WAIT_MS = 4 * 60_000;
const UNWATCH_POLL_MS = 5_000;
/** For the one pair no later event orders: how long a leak has to show up. */
const SETTLE_MS = 8_000;

interface Account {
  userId: string;
  session: string;
}

function requireEnv(name: string, why: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required when E2E_WS_ISOLATION=true - ${why}`);
  return value;
}

const b64 = (n: number) => randomBytes(n).toString('base64');

/** A throwaway wallet-signup account. Never an admin, never a store owner. */
async function registerOrdinaryAccount(label: string): Promise<Account> {
  const key = privateKeyToAccount(generatePrivateKey());
  const start = await api<{ user_id: string; address: string; challenge_message: string }>(
    '/auth/wallet/new-user/start',
    { method: 'POST', body: { address: key.address, wallet_name: label } },
  );
  const signature = await key.signMessage({ message: start.challenge_message });
  const done = await api<{ session_id: string }>('/auth/wallet/new-user/complete', {
    method: 'POST',
    body: {
      user_id: start.user_id,
      address: start.address,
      signature,
      wallet_name: label,
      kdf_params: { algorithm: 'argon2id', memory_kb: 65536, iterations: 3, parallelism: 4, salt: b64(16) },
      encrypted_symmetric_key: { ciphertext: b64(48), iv: b64(16), mac: b64(32) },
      recovery_verification_hash: b64(32),
      device_name: label,
      device_type: 'api_client',
    },
  });
  return { userId: start.user_id, session: done.session_id };
}

/** Same name shape `scripts/sweep-e2e-stores.mjs` recognises. */
function storeName(offsetMs: number): string {
  return `e2e-synthetic-${new Date(Date.now() + offsetMs).toISOString().replace(/[:.]/g, '-')}`;
}

/** Retry only the 409 for a still-watched address; any other failure is final. */
async function deleteAccountWhenUnwatched(userId: string, token: string): Promise<void> {
  const started = Date.now();
  const deadline = started + UNWATCH_WAIT_MS;
  let refused = 0;
  for (;;) {
    try {
      await api(`/admin/users/${userId}`, { method: 'DELETE', token });
      if (refused > 0) console.log(`account ${userId}: delete refused ${refused}x with 409, then succeeded after ${Date.now() - started}ms`);
      return;
    } catch (e) {
      if (!(e instanceof ApiError) || e.status !== 409 || Date.now() >= deadline) throw e;
      refused++;
      if (refused === 1) console.log(`account ${userId}: delete refused 409 (${e.body.slice(0, 120)}), waiting`);
      await new Promise((r) => setTimeout(r, UNWATCH_POLL_MS));
    }
  }
}

let adminToken: string | null = null;
let stores: { id: string; session: string }[] = [];
let accounts: Account[] = [];
let sockets: RecordingSocket[] = [];

test.describe('Status socket tenant isolation', () => {
  test.describe.configure({ retries: 0 });
  test.skip(!ENABLED, 'Set E2E_WS_ISOLATION=true to run the live /ws isolation test');

  /**
   * Close the sockets, have each owner archive its own store and the admin
   * delete both ordinary accounts. Archive rather than hard delete, as the synthetic payment spec does. A
   * cleanup failure fails the run only when the test itself passed, so it
   * never buries the isolation result, and it is announced either way.
   */
  test.afterEach(async ({}, testInfo) => {
    for (const s of sockets) s.close();
    sockets = [];

    const token = adminToken;
    const owned = stores;
    const users = accounts;
    stores = [];
    accounts = [];
    if (!token) return;

    const failures: string[] = [];
    for (const { id, session } of owned) {
      await api(`/stores/${id}`, { method: 'DELETE', token: session }).catch((e) =>
        failures.push(`archive store ${id}: ${e}`),
      );
    }
    for (const u of users) {
      await deleteAccountWhenUnwatched(u.userId, token).catch((e) =>
        failures.push(`delete account ${u.userId}: ${e}`),
      );
    }
    if (failures.length === 0) return;

    const testPassed = testInfo.status === testInfo.expectedStatus;
    const msg =
      (testPassed
        ? 'The isolation assertions PASSED; only the teardown failed (this is not an isolation result). '
        : 'The test itself failed; teardown also left residue. ') +
      `ws isolation cleanup left residue: ${failures.join('; ')}`;
    console.log(`::error title=ws isolation cleanup failed::${msg}`);
    if (process.env.GITHUB_STEP_SUMMARY) {
      appendFileSync(process.env.GITHUB_STEP_SUMMARY, `### ❌ ws isolation cleanup failed\n\n${msg}\n`);
    }
    if (testPassed) throw new Error(msg);
  });

  test('a socket receives its own stores\' events and no other store\'s', async () => {
    test.setTimeout(4 * EXPIRY_WAIT_MS);
    const token = REMOTE
      ? requireEnv('E2E_API_TOKEN', 'a server admin API key that deletes the test accounts')
      : (await createUserWithApiKey('server_admin')).apiKey;
    adminToken = token;

    const stamp = Date.now().toString(36);
    const a = await registerOrdinaryAccount(`e2e-synthetic-ws-a-${stamp}`);
    accounts.push(a);
    const b = await registerOrdinaryAccount(`e2e-synthetic-ws-b-${stamp}`);
    accounts.push(b);

    const xpub = () => HDKey.fromMasterSeed(randomBytes(32)).derive(MERCHANT_PATH).publicExtendedKey;
    async function makeStore(offsetMs: number, owner: Account): Promise<string> {
      const store = await api<{ id: string }>('/stores', {
        method: 'POST',
        token: owner.session,
        body: { name: storeName(offsetMs) },
      });
      stores.push({ id: store.id, session: owner.session });
      await api(`/stores/${store.id}/payment-methods`, {
        method: 'POST',
        token: owner.session,
        body: { chain_id: CHAIN_ID, token_address: null, asset_symbol: 'ETH', decimals: 18, xpub: xpub() },
      });
      return store.id;
    }
    const s1 = await makeStore(0, a);
    const s2 = await makeStore(1, b);

    // Both sockets are acknowledged before the first event exists.
    const socketA = await openAuthenticatedSocket(a.session);
    sockets.push(socketA);
    const socketB = await openAuthenticatedSocket(b.session);
    sockets.push(socketB);

    async function expireOne(storeId: string, owner: Account): Promise<string> {
      const invoice = await api<{ id: string }>('/invoices', {
        method: 'POST',
        token: owner.session,
        body: {
          store_id: storeId,
          currency: 'ETH',
          amount: '0.0001',
          expiration_seconds: 1,
          metadata: { source: 'ws-tenant-isolation' },
        },
      }).catch((e) => {
        if (e instanceof ApiError && (e.status === 402 || e.status === 403)) {
          throw new Error(
            `NOT VERIFIED - setup refused invoice creation: ${e.status} ${e.body.slice(0, 500)}. ` +
              'This is a setup failure, not an isolation result.',
          );
        }
        throw e;
      });
      expect(invoice.id, 'invoice id is not a v4 UUID').toMatch(V4_UUID);
      return invoice.id;
    }

    // A wait that times out is recorded, not thrown: the isolation verdict
    // below must run on whatever frames arrived, so a broken expiry path cannot
    // take the isolation check with it. Once one event is missing, later waits
    // would only repeat the same timeout, so the remaining invoices are still
    // created but not waited on. If the socket was closed meanwhile (an idle
    // proxy timeout, say) it records nothing, so a leak after the closure is
    // invisible: that run is reported NOT VERIFIED, never as a clean pass.
    let precondition: string | null = null;
    async function expectOwnEvent(socket: RecordingSocket, invoiceId: string, who: string): Promise<void> {
      if (precondition) return;
      let frame: Frame;
      try {
        frame = await socket.waitForInvoice(invoiceId, EXPIRY_WAIT_MS);
      } catch (e) {
        precondition = `${who}: ${(e as Error).message}`;
        return;
      }
      // Outside the try: a wrong status is a product failure, not a missing precondition.
      expect(frame.status, `${who} was told something other than expiry about its own invoice`).toBe('expired');
    }

    const s2First = await expireOne(s2, b);
    await expectOwnEvent(socketB, s2First, 'B');

    const s1Only = await expireOne(s1, a);
    await expectOwnEvent(socketA, s1Only, 'A');

    const s2Second = await expireOne(s2, b);
    await expectOwnEvent(socketB, s2Second, 'B');

    // The one pair nothing orders: S2's second event against A.
    await new Promise((r) => setTimeout(r, SETTLE_MS));

    // Isolation first (a leak is a failure even with no own event), then the
    // positive controls, whose absence is NOT VERIFIED rather than a failure.
    judgeIsolation([
      { label: 'A', frames: socketA.frames, own: [s1Only] },
      { label: 'B', frames: socketB.frames, own: [s2First, s2Second] },
    ]);
    if (precondition) {
      throw new Error(
        `NOT VERIFIED - isolation could not run: an expected expiry event never arrived (${precondition}). ` +
          'No foreign frame was seen, which proves nothing. This is not an isolation result.',
      );
    }

    console.log(
      `A (S1 owner) frames: ${socketA.frames.length}, B (S2 owner) frames: ${socketB.frames.length}; ` +
        `A saw ${s1Only} and nothing of ${s2First}/${s2Second}; B saw ${s2First} and ${s2Second}, nothing of ${s1Only}`,
    );
  });
});
