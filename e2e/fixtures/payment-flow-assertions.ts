/**
 * Pure assertion helpers for `tests/payment-flow-postmerge.spec.ts` — the
 * post-merge gate that exercises invoice -> pay -> settle -> webhook -> payout
 * against live testnet.
 *
 * Factored out so "a deliberately broken flow turns the job red" has
 * something to test directly, rather than trusting that an `expect()` buried
 * inside a 20+ minute live run against real Sepolia would actually fire.
 * `tests/payment-flow-assertions.spec.ts` breaks each one and asserts it
 * throws — that file runs in `e2e-static-checks` on every PR, so a change
 * that quietly weakens one of these (the exact way a guard can rot without
 * anyone noticing, per `AGENTS.md`) fails long before a nightly or a
 * post-merge run would ever reach it.
 */

/** The shape of `PayoutResponse` this module cares about — see `server/src/api/payouts.rs`. */
export interface PayoutLike {
  invoice_ids: string[];
  amount: string;
  asset_symbol: string;
  chain_id: string;
  status: string;
}

/**
 * Throws unless `payout` is a payout that genuinely covers `invoiceId`: it
 * names that invoice, for the right asset on the right chain, for exactly the
 * amount the invoice was paid.
 *
 * This is the money-path check the whole payout step exists for — a payout
 * that drifts on any one of these fields either shorts the merchant (wrong
 * amount) or settles the wrong claim (wrong invoice/asset/chain) while the
 * API still answers 200.
 */
export function assertPayoutCoversInvoice(
  payout: PayoutLike,
  invoiceId: string,
  expectedAmount: bigint,
  expectedAsset: string,
  expectedChain: string,
): void {
  if (!payout.invoice_ids.includes(invoiceId)) {
    throw new Error(
      `payout does not claim invoice ${invoiceId} (claims: ${payout.invoice_ids.join(', ') || 'nothing'})`,
    );
  }
  if (payout.asset_symbol !== expectedAsset) {
    throw new Error(`payout asset ${payout.asset_symbol} !== expected ${expectedAsset}`);
  }
  if (payout.chain_id !== expectedChain) {
    throw new Error(`payout chain ${payout.chain_id} !== expected ${expectedChain}`);
  }
  let actualAmount: bigint;
  try {
    actualAmount = BigInt(payout.amount);
  } catch {
    throw new Error(`payout amount '${payout.amount}' is not an integer`);
  }
  if (actualAmount !== expectedAmount) {
    throw new Error(`payout amount ${actualAmount} !== expected ${expectedAmount}`);
  }
}

/**
 * Throws unless `payout.status` is `confirmed` — the state `POST
 * /payouts/{id}/settle` is supposed to move a `pending` payout into
 * (`server/src/api/payouts.rs::settle_payout`).
 */
export function assertPayoutSettled(payout: PayoutLike): void {
  if (payout.status !== 'confirmed') {
    throw new Error(`payout status is '${payout.status}', expected 'confirmed' after settling`);
  }
}
