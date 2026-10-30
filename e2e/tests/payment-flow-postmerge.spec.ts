/**
 * Post-merge payment-flow gate (live testnet).
 *
 * The named E2E suite behind the merge-to-testnet CI job
 * (`.github/workflows/ci.yml`, job `e2e-postmerge-testnet`): invoice -> pay ->
 * settle -> webhook -> payout, for two assets — native Sepolia ETH and
 * Sepolia USDC (an ERC-20) — so a broken flow on either the native or the
 * token path fails the pipeline instead of reaching users.
 *
 * Distinct from `synthetic-payment.spec.ts` (the long-running nightly dead-
 * man's-switch suite): this one is shorter, one payment per asset, and
 * carries the payout step that the nightly suite does not. It uses its own
 * spender account index (see `SPENDER_ACCOUNT_INDEX` below) so the two
 * suites, which can both be live around a merge landing near 05:37 UTC, never
 * share a nonce sequence.
 *
 * `assertPayoutCoversInvoice`/`assertPayoutSettled` (`fixtures/payment-flow-
 * assertions.ts`) carry the actual pass/fail logic for the payout step, and
 * `tests/payment-flow-assertions.spec.ts` deliberately breaks each one and
 * checks it goes red — see that file for what "a deliberately broken flow
 * turns the job red" means here, verified on every PR rather than only on a
 * live run against real Sepolia.
 *
 * Run:
 *   E2E_REMOTE=true E2E_POSTMERGE_PAYMENT=true \
 *   E2E_TEST_MNEMONIC=... E2E_SEPOLIA_RPC_URL=... \
 *     npx playwright test tests/payment-flow-postmerge.spec.ts
 *
 * Off by default, same reasoning as the nightly suite: it spends real
 * (testnet) funds and needs secrets, so the in-pipeline `e2e` job in
 * `ci.yml` (the one that starts a local, ephemeral server) must not pick it
 * up.
 */
import {
  createPublicClient,
  createWalletClient,
  erc20Abi,
  formatEther,
  formatUnits,
  http,
  parseEther,
  parseUnits,
} from 'viem';
import { HDKey, mnemonicToAccount } from 'viem/accounts';
import { sepolia } from 'viem/chains';
import { mnemonicToSeedSync } from '@scure/bip39';
import { test, expect } from '@playwright/test';

import { api, V4_UUID, wsUrl } from '../fixtures/api';
import { assertPayoutCoversInvoice, assertPayoutSettled } from '../fixtures/payment-flow-assertions';
import { createAndSettlePayout, waitForPaid, type Checkout } from '../fixtures/payment-flow';
import { registerSyntheticAccount } from '../fixtures/synthetic-account';
import { TunnelUnavailableError, WebhookSink, verifySignature } from '../fixtures/webhook-sink';

const ENABLED = process.env.E2E_POSTMERGE_PAYMENT === 'true';

const CHAIN_ID = 'eip155:11155111';
// Circle's own Sepolia USDC deployment — already seeded in
// `data-service/migrations/postgres/20260920000000_sepolia_tokens.sql`, so a
// payment on it resolves a real symbol rather than falling through to the
// unknown-token branch. No new token row needed for this suite.
const USDC_ADDRESS = '0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238';
const USDC_DECIMALS = 6;

/**
 * Reserved for this suite, distinct from `SPENDER_ACCOUNT_INDEX` (9) in
 * `tests/synthetic-payment.spec.ts`. This job and the nightly one can both be
 * live around the same merge, and two processes signing from the same
 * address read the same pending nonce — the second broadcast would replace
 * the first instead of paying its own invoice. Keep this index reserved for
 * this file alone.
 */
const SPENDER_ACCOUNT_INDEX = 10;

const SETUP_BUDGET_MS = 3 * 60_000;
const RECEIPT_TIMEOUT_MS = 3 * 60_000;
const PAID_TIMEOUT_MS = 5 * 60_000;
const WEBHOOK_TIMEOUT_MS = 2 * 60_000;
const PAYOUT_TIMEOUT_MS = 30_000;

function requireEnv(name: string, why: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required when E2E_POSTMERGE_PAYMENT=true — ${why}`);
  }
  return value;
}

interface PaymentOption {
  chain_id: string;
  asset_symbol: string;
  token_address: string | null;
  payment_address: string;
  amount: string;
}
interface Invoice {
  id: string;
  payment_options: PaymentOption[];
}
interface PaymentMethod {
  id: string;
  derivation_index: number | null;
}

/** One leg of the flow this suite runs — see the two entries below. */
interface Asset {
  label: string;
  assetSymbol: string;
  tokenAddress: string | null;
  decimals: number;
  /** Human-readable units, matching `decimals` — e.g. "0.0001" ETH or "0.01" USDC. */
  amount: string;
}

const NATIVE_ETH: Asset = {
  label: 'native ETH-on-Sepolia',
  assetSymbol: 'ETH',
  tokenAddress: null,
  decimals: 18,
  amount: '0.0001',
};

const USDC_SEPOLIA: Asset = {
  label: 'USDC-on-Sepolia (ERC-20)',
  assetSymbol: 'USDC',
  tokenAddress: USDC_ADDRESS,
  decimals: USDC_DECIMALS,
  // One cent. The spender needs this much Sepolia USDC — fund it from
  // Circle's faucet (faucet.circle.com); see e2e/README.md.
  amount: '0.01',
};

/** What the cleanup hook needs — module scope, for the same reason as `synthetic-payment.spec.ts`. */
let createdStoreId: string | null = null;
let apiToken: string | null = null;
let activeSink: WebhookSink | null = null;

test.describe('Post-merge payment flow (live testnet)', () => {
  // A retry would broadcast a second transaction against an invoice already
  // half-paid, same reasoning as the nightly suite.
  test.describe.configure({ retries: 0 });
  test.skip(
    !ENABLED,
    'Set E2E_POSTMERGE_PAYMENT=true to run the post-merge payment-flow gate (spends testnet funds)',
  );

  /**
   * Archive the store this run created, even if the test body never reached
   * its own cleanup (a Playwright timeout skips straight to hooks). Mirrors
   * `synthetic-payment.spec.ts`'s `afterEach`, without the archive-survives
   * re-read: that assertion is this suite's whole reason to exist, so
   * duplicating it in a hook that swallows its own failures would be the
   * thing most worth not doing twice.
   */
  test.afterEach(async () => {
    if (activeSink) {
      const sink = activeSink;
      activeSink = null;
      await sink
        .stop()
        .catch((err: unknown) => console.log(`::warning title=Webhook sink not stopped::${err}`));
    }
    const storeId = createdStoreId;
    createdStoreId = null;
    const token = apiToken;
    apiToken = null;
    if (!storeId || !token) return;
    try {
      await api(`/stores/${storeId}`, { method: 'DELETE', token });
      console.log(`archived store ${storeId}`);
    } catch (err) {
      console.log(`::warning title=Could not archive post-merge store ${storeId}::${err}`);
    }
  });

  for (const asset of [NATIVE_ETH, USDC_SEPOLIA]) {
    test(`${asset.label}: invoice -> pay -> settle -> webhook -> payout`, async () => {
      test.setTimeout(
        SETUP_BUDGET_MS + RECEIPT_TIMEOUT_MS + PAID_TIMEOUT_MS + WEBHOOK_TIMEOUT_MS + PAYOUT_TIMEOUT_MS,
      );

      const mnemonic = requireEnv('E2E_TEST_MNEMONIC', 'BIP39 phrase for the merchant xpub + spender');
      const rpcUrl = requireEnv('E2E_SEPOLIA_RPC_URL', 'Sepolia RPC endpoint to broadcast from');

      const runStartedAt = Date.now();
      const merchantXpub = HDKey.fromMasterSeed(mnemonicToSeedSync(mnemonic))
        .derive(`m/44'/60'/${Math.floor(runStartedAt / 1000)}'`)
        .publicExtendedKey;
      const spender = mnemonicToAccount(mnemonic, { accountIndex: SPENDER_ACCOUNT_INDEX });

      const transport = http(rpcUrl);
      const publicClient = createPublicClient({ chain: sepolia, transport });
      const walletClient = createWalletClient({ account: spender, chain: sepolia, transport });

      // Gas headroom: 60k gas covers a plain transfer (21k) and an ERC-20
      // transfer (typically 45-65k) alike, at up to 2x the live price.
      const ethBalance = await publicClient.getBalance({ address: spender.address });
      const gasPrice = await publicClient.getGasPrice();
      const gasReserve = gasPrice * 60_000n * 2n;
      const neededEth = asset.tokenAddress === null ? parseEther(asset.amount) + gasReserve : gasReserve;
      expect(
        ethBalance >= neededEth,
        `Spender ${spender.address} holds ${formatEther(ethBalance)} SepoliaETH, needs ~` +
          `${formatEther(neededEth)} for the ${asset.label} leg (value + gas). Refill it from a faucet.`,
      ).toBe(true);

      if (asset.tokenAddress) {
        const tokenAddress = asset.tokenAddress as `0x${string}`;
        const tokenBalance = await publicClient.readContract({
          address: tokenAddress,
          abi: erc20Abi,
          functionName: 'balanceOf',
          args: [spender.address],
        });
        const neededToken = parseUnits(asset.amount, asset.decimals);
        expect(
          tokenBalance >= neededToken,
          `Spender ${spender.address} holds ${formatUnits(tokenBalance, asset.decimals)} ` +
            `${asset.assetSymbol}, needs ${asset.amount}. Fund it from Circle's Sepolia USDC faucet ` +
            `(faucet.circle.com) — see e2e/README.md.`,
        ).toBe(true);
      }

      const sink = await WebhookSink.start().catch((err: unknown) => {
        if (err instanceof TunnelUnavailableError) {
          throw new Error(
            `INFRASTRUCTURE UNAVAILABLE, NOT A PAYMENT FAILURE: the Cloudflare quick tunnel could not ` +
              `be provisioned, so no payment was attempted and nothing was verified.\n${err.message}`,
          );
        }
        throw err;
      });
      activeSink = sink;

      const stamp = `${new Date(runStartedAt).toISOString().replace(/[:.]/g, '-')}-${asset.assetSymbol.toLowerCase()}`;
      const account = await registerSyntheticAccount(`e2e-postmerge-${stamp}`);
      const token = account.token;
      apiToken = token;

      const store = await api<{ id: string }>('/stores', {
        method: 'POST',
        token,
        body: { name: `e2e-postmerge-${stamp}` },
      });
      createdStoreId = store.id;

      // One payment method, matching this asset only — a fresh store per
      // asset leg so an invoice's payment options never need a rate lookup
      // for an asset this run is not exercising.
      const method = await api<PaymentMethod>(`/stores/${store.id}/payment-methods`, {
        method: 'POST',
        token,
        body: {
          chain_id: CHAIN_ID,
          token_address: asset.tokenAddress,
          asset_symbol: asset.assetSymbol,
          decimals: asset.decimals,
          xpub: merchantXpub,
        },
      });
      expect(
        typeof method.derivation_index,
        `the ${asset.label} payment method resolves to no wallet at all — it cannot derive an address`,
      ).toBe('number');

      const webhook = await api<{ webhook_secret: string | null }>(`/stores/${store.id}/webhook`, {
        method: 'PUT',
        token,
        body: { webhook_url: sink.publicUrl, enabled: true },
      });
      const secret = webhook.webhook_secret;
      expect(secret, 'webhook secret is only returned on upsert — it must be present here').toBeTruthy();

      const invoice = await api<Invoice>('/invoices', {
        method: 'POST',
        token,
        body: {
          store_id: store.id,
          currency: asset.assetSymbol,
          amount: asset.amount,
          expiration_seconds: 1_800,
          metadata: { source: 'payment-flow-postmerge', asset: asset.assetSymbol },
        },
      });
      expect(invoice.id, 'invoice id is not a v4 UUID').toMatch(V4_UUID);

      const option = invoice.payment_options.find(
        (o) => o.chain_id === CHAIN_ID && o.token_address === asset.tokenAddress,
      );
      expect(option, `no ${asset.label} payment option on invoice ${invoice.id}`).toBeDefined();
      const target = option as PaymentOption;
      const expectedAmount =
        asset.tokenAddress === null ? parseEther(asset.amount) : parseUnits(asset.amount, asset.decimals);
      expect(
        BigInt(target.amount),
        `invoice ${invoice.id} was created for ${asset.amount} ${asset.assetSymbol} but quotes ${target.amount}`,
      ).toBe(expectedAmount);

      console.log(
        `${asset.label}: invoice ${invoice.id} — ${asset.amount} ${asset.assetSymbol} to ` +
          `${target.payment_address}`,
      );

      // Subscribe before broadcasting: the socket only forwards live events.
      const paidPromise = waitForPaid(wsUrl, invoice.id, PAID_TIMEOUT_MS);
      paidPromise.catch(() => {});

      let hash: `0x${string}`;
      if (asset.tokenAddress === null) {
        hash = await walletClient.sendTransaction({
          to: target.payment_address as `0x${string}`,
          value: BigInt(target.amount),
        });
      } else {
        hash = await walletClient.writeContract({
          address: asset.tokenAddress as `0x${string}`,
          abi: erc20Abi,
          functionName: 'transfer',
          args: [target.payment_address as `0x${string}`, BigInt(target.amount)],
        });
      }
      console.log(`${asset.label}: broadcast ${hash} — https://sepolia.etherscan.io/tx/${hash}`);

      const receipt = await publicClient.waitForTransactionReceipt({ hash, timeout: RECEIPT_TIMEOUT_MS });
      expect(receipt.status, `transaction ${hash} reverted`).toBe('success');

      const { via, seen } = await paidPromise;
      console.log(`${asset.label}: paid, detected via ${via} — updates: ${seen.join(', ') || 'none'}`);

      const checkout = await api<Checkout>(`/checkout/${invoice.id}`);
      expect(checkout.is_paid, `invoice ${invoice.id} reports ${checkout.status}`).toBe(true);
      expect(
        checkout.payments.map((p) => p.tx_hash.toLowerCase()),
        `invoice ${invoice.id} is paid, but not by the transaction sent to its own address`,
      ).toContain(hash.toLowerCase());

      const delivered = await sink.waitFor(
        'payment_confirmed',
        WEBHOOK_TIMEOUT_MS,
        (hook) => hook.body.invoice_id === invoice.id,
      );
      expect(delivered.body.invoice_id).toBe(invoice.id);
      expect(delivered.body.store_id).toBe(store.id);
      expect(delivered.body.status).toBe('paid');
      expect(delivered.body.chain_id).toBe(CHAIN_ID);
      expect(
        verifySignature(delivered, secret as string),
        'X-Webhook-Signature did not match an HMAC-SHA256 of the delivered body',
      ).toBe(true);

      // Payout: the merchant's own record of having swept this invoice's
      // funds. This server never signs or broadcasts a transaction
      // (`server/src/api/payouts.rs`) — the two API calls inside
      // `createAndSettlePayout` are the whole of this step.
      const settled = await createAndSettlePayout(
        store.id,
        token,
        invoice.id,
        spender.address,
        CHAIN_ID,
        asset.assetSymbol,
        asset.tokenAddress,
      );
      assertPayoutCoversInvoice(settled, invoice.id, BigInt(target.amount), asset.assetSymbol, CHAIN_ID);
      assertPayoutSettled(settled);
      console.log(`${asset.label}: payout ${settled.id} settled for ${settled.amount} ${asset.assetSymbol}`);
    });
  }
});
