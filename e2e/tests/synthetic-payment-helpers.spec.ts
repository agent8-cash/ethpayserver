/**
 * Unit coverage for the pure parts of the synthetic payment run.
 *
 * The money-path spec next door is skipped unless `E2E_SYNTHETIC_PAYMENT=true`,
 * which in practice means once a night, on a runner, holding secrets nobody
 * developing this has. An off-by-one in the amount draw or the shared budget
 * would surface there as a mysterious insufficient-funds revert or a job killed
 * at its cap. These run in every suite, in milliseconds, with no network.
 */
import { test, expect } from '@playwright/test';
import { formatEther, parseEther } from 'viem';

import { ApiError } from '../fixtures/api';
import {
  AMOUNT_STEP_WEI,
  explainInvoiceCreationRefusal,
  expectRunSurvivedArchive,
  MAX_INVOICE_AMOUNT_WEI,
  MIN_INVOICE_AMOUNT_WEI,
  randomInvoiceAmountWei,
  remainingBudgetMs,
  worstCaseRunCostWei,
} from '../fixtures/synthetic-payment';

test.describe('randomInvoiceAmountWei', () => {
  test('every draw lands inside the band, on the grid', () => {
    for (let i = 0; i < 1_000; i++) {
      const wei = randomInvoiceAmountWei();
      expect(wei >= MIN_INVOICE_AMOUNT_WEI, `${wei} is below the minimum`).toBe(true);
      expect(wei <= MAX_INVOICE_AMOUNT_WEI, `${wei} is above the maximum`).toBe(true);
      expect(wei % AMOUNT_STEP_WEI, `${wei} is off the gwei grid`).toBe(0n);
    }
  });

  // The maximum is what the balance guard sizes the wallet against: a draw one
  // step past it is a wallet that runs dry a night earlier than the guard said.
  test('reaches both ends of the band and no further', () => {
    expect(randomInvoiceAmountWei(() => 0)).toBe(MIN_INVOICE_AMOUNT_WEI);
    expect(randomInvoiceAmountWei(() => 0.999999)).toBe(MAX_INVOICE_AMOUNT_WEI);
    expect(randomInvoiceAmountWei(() => 1)).toBe(MAX_INVOICE_AMOUNT_WEI);
    expect(randomInvoiceAmountWei(() => 1.5)).toBe(MAX_INVOICE_AMOUNT_WEI);
    expect(randomInvoiceAmountWei(() => -0.5)).toBe(MIN_INVOICE_AMOUNT_WEI);
  });

  test('midpoint of the band is the 0.0001 ETH this test used to pay', () => {
    expect(randomInvoiceAmountWei(() => 0.5)).toBe(parseEther('0.0001'));
  });

  // The API takes the amount as a decimal string. A value that renders as
  // "5e-5", or that loses a digit on the way back, is an invoice for the wrong
  // money and a payment that never matches it.
  test('round-trips through the decimal string the API is given', () => {
    for (let i = 0; i < 1_000; i++) {
      const wei = randomInvoiceAmountWei();
      const decimal = formatEther(wei);
      expect(decimal, `${decimal} is in scientific notation`).not.toMatch(/e/i);
      expect(decimal, `${decimal} is not a plain decimal`).toMatch(/^\d+\.\d+$/);
      expect(decimal.split('.')[1].length, `${decimal} has more than gwei precision`).toBeLessThanOrEqual(9);
      expect(parseEther(decimal), `${decimal} did not round-trip`).toBe(wei);
    }
  });
});

test.describe('remainingBudgetMs', () => {
  const now = 1_000_000;

  test('gives the full per-payment wait while the budget covers it', () => {
    expect(remainingBudgetMs(now + 600_000, 120_000, now)).toBe(120_000);
  });

  test('hands back only what is left when the budget is shorter', () => {
    expect(remainingBudgetMs(now + 30_000, 120_000, now)).toBe(30_000);
  });

  test('never waits for zero, or for negative time, on an expired budget', () => {
    expect(remainingBudgetMs(now, 120_000, now)).toBe(1_000);
    expect(remainingBudgetMs(now - 600_000, 120_000, now)).toBe(1_000);
  });
});

test.describe('worstCaseRunCostWei', () => {
  const PAYMENTS = 3;
  const GAS_FLOOR_PER_PAYMENT = parseEther('0.0005');

  // Sepolia at rest. The floor dominates, and the figure is the one the
  // LOW_BALANCE_RUNS comment quotes.
  test('uses the constant floor while gas is cheap', () => {
    const cost = worstCaseRunCostWei(PAYMENTS, 1_000_000_000n); // 1 gwei
    expect(cost).toBe(BigInt(PAYMENTS) * (MAX_INVOICE_AMOUNT_WEI + GAS_FLOOR_PER_PAYMENT));
    expect(formatEther(cost)).toBe('0.00195');
  });

  // The case the constant alone got wrong: a sustained spike where three
  // transfers cost more than the margin reserved for them, and the wallet
  // passes the guard and then drains partway through the run.
  test('follows the live gas price once it outgrows the floor', () => {
    const spike = 200_000_000_000n; // 200 gwei
    const cost = worstCaseRunCostWei(PAYMENTS, spike);
    const perPaymentGas = spike * 21_000n * 3n;
    expect(perPaymentGas > GAS_FLOOR_PER_PAYMENT, 'the spike must clear the floor').toBe(true);
    expect(cost).toBe(BigInt(PAYMENTS) * (MAX_INVOICE_AMOUNT_WEI + perPaymentGas));
  });

  test('never reserves less than the amounts it is about to send', () => {
    for (const gwei of [0n, 1n, 25n, 500n]) {
      const cost = worstCaseRunCostWei(PAYMENTS, gwei * 1_000_000_000n);
      expect(
        cost >= BigInt(PAYMENTS) * MAX_INVOICE_AMOUNT_WEI,
        `at ${gwei} gwei the reserve does not even cover the transfers`,
      ).toBe(true);
    }
  });
});

// The survival check against a fake server in each state the daily run can
// leave behind. The hard-delete case is the ablation: it is what the run did
// before the store was archived instead, and the check must go red on it.
test.describe('expectRunSurvivedArchive', () => {
  const STORE = 's1';
  const records = [
    { invoiceId: 'i1', hash: '0xaa' },
    { invoiceId: 'i2', hash: '0xBB' },
    { invoiceId: 'i3', hash: '0xcc' },
  ];

  type State = 'archived' | 'hard-deleted' | 'still-listed' | 'not-archived' | 'renamed';
  function fake(state: State) {
    return async <T>(path: string): Promise<T> => {
      // What the real `api` helper throws on a non-2xx, which is what a hard
      // delete turns the store read into.
      if (state === 'hard-deleted') throw new ApiError('GET', path, 404, 'store not found');
      if (path === `/stores/${STORE}`) {
        return {
          archived: state !== 'not-archived',
          name: state === 'renamed' ? 'merchant shop' : 'e2e-synthetic-2026-10-01T00:00:00Z',
        } as T;
      }
      if (path === '/stores') return (state === 'still-listed' ? [{ id: STORE }] : []) as T;
      const inv = records.find((r) => path === `/checkout/${r.invoiceId}`)!;
      return { payments: [{ tx_hash: inv.hash.toUpperCase() }] } as T;
    };
  }

  test('passes on an archived, unlisted, stamped store with every payment', async () => {
    await expectRunSurvivedArchive(STORE, records, 3, fake('archived'));
  });

  const expected = {
    'hard-deleted': /GET \/stores\/s1 → 404/,
    'still-listed': /still in the default GET \/stores listing/,
    'not-archived': /is not archived/,
    renamed: /not an e2e-synthetic stamp/,
  } as const;
  for (const state of Object.keys(expected) as (keyof typeof expected)[]) {
    test(`fails when the store is ${state}`, async () => {
      await expect(expectRunSurvivedArchive(STORE, records, 3, fake(state))).rejects.toThrow(
        expected[state],
      );
    });
  }

  test('fails when a payment is missing from its invoice', async () => {
    const get = async <T>(path: string): Promise<T> =>
      (path.startsWith('/checkout/') ? { payments: [] } : fake('archived')<T>(path)) as Promise<T>;
    await expect(expectRunSurvivedArchive(STORE, records, 3, get)).rejects.toThrow(/no longer shows/);
  });

  test('fails when the run recorded fewer payments than expected', async () => {
    await expect(
      expectRunSurvivedArchive(STORE, records.slice(0, 2), 3, fake('archived')),
    ).rejects.toThrow(/recorded 2/);
  });
});

test.describe('explainInvoiceCreationRefusal', () => {
  const blocked = '{"error":"invoice_creation_blocked","message":"over allowance"}';

  test('a volume refusal becomes an error saying it is permanent', () => {
    const out = explainInvoiceCreationRefusal(new ApiError('POST', '/invoices', 403, blocked));
    expect(out).toBeInstanceOf(Error);
    expect((out as Error).message).toMatch(/will not clear on a re-run/);
    expect((out as Error).message).toContain('invoice_creation_blocked');
  });

  test('other refusals and other errors pass through unchanged', () => {
    const forbidden = new ApiError('POST', '/invoices', 403, '{"error":"forbidden"}');
    const server = new ApiError('POST', '/invoices', 500, blocked);
    const plain = new TypeError('boom');
    expect(explainInvoiceCreationRefusal(forbidden)).toBe(forbidden);
    expect(explainInvoiceCreationRefusal(server)).toBe(server);
    expect(explainInvoiceCreationRefusal(plain)).toBe(plain);
  });
});
