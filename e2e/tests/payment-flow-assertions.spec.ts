/**
 * Self-test for `fixtures/payment-flow-assertions.ts`.
 *
 * Touches no network, no database, no browser — it runs in `e2e-static-checks`
 * on every PR (`E2E_REMOTE=true`, see `.github/workflows/ci.yml`). Its whole
 * job is the acceptance bullet "a deliberately broken flow turns the job red":
 * each `catches…` test below *is* that deliberately broken flow, reduced to
 * the one field it breaks, and the assertion under test must throw on every
 * one of them. If any stops throwing, this file goes red on every PR from then
 * on — long before a 20+ minute live run against real Sepolia would notice.
 */
import { test, expect } from '@playwright/test';

import { assertPayoutCoversInvoice, assertPayoutSettled, type PayoutLike } from '../fixtures/payment-flow-assertions';

const GOOD: PayoutLike = {
  invoice_ids: ['inv_1'],
  amount: '1000',
  asset_symbol: 'USDC',
  chain_id: 'eip155:11155111',
  status: 'confirmed',
};

test.describe('assertPayoutCoversInvoice', () => {
  test('passes a payout that actually covers the invoice', () => {
    expect(() =>
      assertPayoutCoversInvoice(GOOD, 'inv_1', 1000n, 'USDC', 'eip155:11155111'),
    ).not.toThrow();
  });

  test('catches a payout that does not name the invoice it claims to cover', () => {
    expect(() =>
      assertPayoutCoversInvoice(
        { ...GOOD, invoice_ids: ['inv_other'] },
        'inv_1',
        1000n,
        'USDC',
        'eip155:11155111',
      ),
    ).toThrow(/does not claim invoice/);
  });

  test('catches an amount that drifted from what was actually paid', () => {
    expect(() =>
      assertPayoutCoversInvoice({ ...GOOD, amount: '999' }, 'inv_1', 1000n, 'USDC', 'eip155:11155111'),
    ).toThrow(/amount/);
  });

  test('catches the wrong asset', () => {
    expect(() =>
      assertPayoutCoversInvoice({ ...GOOD, asset_symbol: 'ETH' }, 'inv_1', 1000n, 'USDC', 'eip155:11155111'),
    ).toThrow(/asset/);
  });

  test('catches the wrong chain', () => {
    expect(() =>
      assertPayoutCoversInvoice(
        { ...GOOD, chain_id: 'eip155:1' },
        'inv_1',
        1000n,
        'USDC',
        'eip155:11155111',
      ),
    ).toThrow(/chain/);
  });

  test('catches an amount field that is not an integer', () => {
    expect(() =>
      assertPayoutCoversInvoice({ ...GOOD, amount: 'not-a-number' }, 'inv_1', 1000n, 'USDC', 'eip155:11155111'),
    ).toThrow(/not an integer/);
  });
});

test.describe('assertPayoutSettled', () => {
  test('passes a confirmed payout', () => {
    expect(() => assertPayoutSettled(GOOD)).not.toThrow();
  });

  test('catches a payout still pending after the settle call', () => {
    expect(() => assertPayoutSettled({ ...GOOD, status: 'pending' })).toThrow(/pending/);
  });
});
