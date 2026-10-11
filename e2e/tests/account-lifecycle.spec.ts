import { test, expect, register, login, logout } from '../fixtures/auth';
import { resetDatabase } from '../fixtures/db';
import { createStoreReadyForInvoices } from '../fixtures/payment-methods';
import { createInvoice } from '../fixtures/invoices';

/**
 * The whole life of a passkey account: register, use it, log back in, delete it,
 * and prove it is gone.
 *
 * Deliberately separate from `synthetic-payment.spec.ts`. That one spends real
 * Sepolia ETH, needs secrets and runs nightly; this needs no chain, no funds and
 * no secrets, so it can gate a merge - which is where lifecycle regressions
 * actually need catching. Folding them together would also mean a chain outage
 * reporting as an auth failure.
 *
 * Skipped remotely for the same reason `auth.spec.ts` is, and it is not the RP
 * ID: the auth tier allows 5 requests per minute per IP, and a lifecycle run
 * performs a registration, a login and a deletion in seconds. Remotely that is
 * `429`. Measured in the header of `auth.spec.ts`, not assumed here.
 */
const SKIP_AUTH = process.env.E2E_SKIP_AUTH
  ? process.env.E2E_SKIP_AUTH === 'true'
  : process.env.E2E_REMOTE === 'true';

test.describe('Account lifecycle (passkey)', () => {
  test.skip(() => SKIP_AUTH, 'Skipped: auth endpoints rate-limit at 5/min/IP');

  test.beforeAll(async () => {
    await resetDatabase();
  });

  test('an account can be created, used, returned to, and destroyed', async ({
    withAuthenticator: page,
  }) => {
    // ---- create -------------------------------------------------------
    const { accountId } = await register(page);
    expect(accountId, 'a passkey-only account must surface its id: it is the only handle it has').toBeTruthy();

    // ---- use ----------------------------------------------------------
    // Real state, so deletion has something to remove and the refusal rule is
    // actually exercised rather than assumed. An unpaid invoice must NOT block
    // deletion - only payments, payouts and refunds do.
    const store = `lifecycle-${Date.now()}`;
    await createStoreReadyForInvoices(page, store);
    await createInvoice(page, '0.001');

    // ---- return -------------------------------------------------------
    // Logging out and back in is the step that proves the credential persisted,
    // rather than the session merely surviving a reload.
    await logout(page);
    await login(page);

    // ---- destroy ------------------------------------------------------
    await page.goto('/evm/settings');
    await page.locator('.settings-tab', { hasText: /account/i }).first().click();

    const danger = page.locator('.ps-card-danger');
    await expect(danger, 'the Danger Zone should be on the Account tab').toBeVisible();

    await danger.locator('button', { hasText: /delete account/i }).click();

    // Typed confirmation: the real button stays disabled until what is typed
    // matches the account's own handle. Asserting the disabled state first is
    // the point - a one-click delete is what this flow exists to prevent.
    const confirm = page.locator('.form-group', { hasText: /to confirm/i });
    await expect(confirm).toBeVisible();
    const destroy = confirm.locator('button', { hasText: /permanently delete/i });
    await expect(destroy, 'must not be armed before the handle is typed').toBeDisabled();

    await confirm.locator('input[type="text"]').fill(accountId!);
    await expect(destroy).toBeEnabled();
    await destroy.click();

    // ---- prove it is gone ---------------------------------------------
    await page.waitForURL(/\/login/, { timeout: 15_000 });

    // The session died with the account. Asking for a protected page must land
    // back on login rather than rendering from a stale token.
    await page.goto('/evm/settings');
    await expect(page).toHaveURL(/\/login/);

    // And the passkey itself no longer opens anything. This is the assertion
    // that separates "logged out" from "deleted": the credential is still in
    // the virtual authenticator, and it must no longer resolve to an account.
    await page.goto('/login');
    await page.locator('.ps-auth-tab', { hasText: /passkey/i }).click();
    await page.locator('.ps-passkey-button').click();
    await expect(
      page,
      'a deleted account must not be reachable by its surviving passkey',
    ).toHaveURL(/\/login/, { timeout: 15_000 });
  });

  test('an account that has taken a payment refuses deletion, and says why', async ({
    withAuthenticator: page,
  }) => {
    // The other half of the rule, and the one worth protecting: deleting a
    // merchant who traded would cascade through stores and invoices into
    // payments and erase their financial history. The server refuses; this
    // asserts the refusal reaches the merchant as a readable reason rather than
    // a bare failure.
    //
    // Skipped until the suite can seed a payment without spending on-chain -
    // the synthetic-payment run does it with real ETH and is nightly. Left in
    // place, and failing loudly if someone removes the skip without adding the
    // seed, rather than filed away and forgotten.
    test.skip(true, 'needs a way to seed a confirmed payment without on-chain spend');

    const store = `traded-${Date.now()}`;
    await createStoreReadyForInvoices(page, store);
    // ... seed a confirmed payment here ...

    await page.goto('/evm/settings');
    const danger = page.locator('.ps-card-danger');
    await danger.locator('button', { hasText: /delete account/i }).click();
    const confirm = page.locator('.form-group', { hasText: /to confirm/i });
    await confirm.locator('input[type="text"]').fill('whatever');
    await confirm.locator('button', { hasText: /permanently delete/i }).click();

    await expect(
      confirm.locator('[style*="color-error"]'),
      'the refusal must name what is holding the account, not just fail',
    ).toContainText(/payment/i);
  });
});
