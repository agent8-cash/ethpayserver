import { test, expect, register, login } from '../fixtures/auth';
import { resetDatabase } from '../fixtures/db';

// Skipped remotely, and the reason matters because two previous ones were wrong.
//
// NOT the RP ID. The README claimed the virtual authenticator's RP ID
// ("localhost") could not match a remote domain, but
// `WebAuthn.addVirtualAuthenticator` has no RP ID parameter - it comes from the
// page's origin at credentials.create() time. The server agrees, logging
// `rp_id=testnet.agent8.cash` at startup, and a single registration against live
// testnet completes end to end: start -> complete -> /auth/me all 200, session
// stored. Verified 2026-09-06.
//
// NOT resetDatabase() either. fixtures/db.ts returns early when E2E_REMOTE is
// true, so it was already a no-op remotely and could not have blocked anything.
//
// The actual blocker is RATE LIMITING. The auth tier allows 5 requests per
// minute per IP (RATE_LIMIT_AUTH, server/src/api/rate_limit.rs), and this spec
// performs five registrations plus a login in well under a minute. Running it
// remotely gives `HTTP 429: Too many requests` and three of five tests fail -
// measured, not assumed. scout.spec.ts registers once, which is why it passes
// remotely and this does not.
//
// Lifting this needs a decision, not a flag: either the spec paces itself under
// 5/min, or test runs get a higher limit. Tracked separately.
const SKIP_AUTH = process.env.E2E_SKIP_AUTH
  ? process.env.E2E_SKIP_AUTH === 'true'
  : process.env.E2E_REMOTE === 'true';

test.describe('Authentication', () => {
  test.skip(() => SKIP_AUTH, 'Skipped: auth endpoints rate-limit at 5/min/IP (see header)');

  // Local hygiene only - fixtures/db.ts makes this a no-op when E2E_REMOTE is
  // set, so it never touches a shared database.
  test.beforeAll(async () => {
    await resetDatabase();
  });

  test('register new account with passkey', async ({ withAuthenticator: page }) => {
    await register(page);

    await expect(page).toHaveURL(/\/(evm)?$/);
    await expect(page.locator('.ps-auth-page')).not.toBeVisible();
  });

  test('registration surfaces the recovery phrase and account id', async ({
    withAuthenticator: page,
  }) => {
    const credentials = await register(page);

    // Assert on the count, never on the array: a failing `toHaveLength` prints
    // the received value, which would put real recovery material into CI logs
    // and the uploaded playwright-report artifact. register_page.rs withholds
    // Debug from mnemonic_words for the same reason.
    //
    // Word validity is not re-checked here - the fixture already validates the
    // BIP39 checksum and refuses to return a phrase that fails, which catches
    // reordering and substitution that a per-word regex cannot.
    expect(credentials.mnemonic.length, 'phrase should be 24 words').toBe(24);

    // A passkey-only account has no email and no wallet, so the account id is
    // the ONLY identifier it can present at recovery. Losing it means the phrase
    // alone cannot recover the account, so its presence is load-bearing rather
    // than cosmetic.
    expect(credentials.accountId, 'passkey-only accounts must be shown an account id').toBeTruthy();
    expect(credentials.accountId).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i,
    );
  });

  test('login with existing passkey', async ({ withAuthenticator: page }) => {
    // Register first so the virtual authenticator holds a credential
    await register(page);

    // Clear the session but keep the virtual authenticator state
    await page.context().clearCookies();
    await page.evaluate(() => localStorage.clear());

    // Login re-uses the resident credential from the authenticator
    await login(page);
    await expect(page).toHaveURL(/\/(evm)?$/);
  });

  test('session persists across page reload', async ({ withAuthenticator: page }) => {
    await register(page);
    await expect(page).toHaveURL(/\/(evm)?$/);

    await page.reload();

    await expect(page).toHaveURL(/\/(evm)?$/);
    await expect(page.locator('.ps-auth-page')).not.toBeVisible();
  });

  test('unauthenticated users are redirected to login', async ({ page }) => {
    await page.goto('/evm/stores');
    await expect(page).toHaveURL(/\/login/);
  });
});
