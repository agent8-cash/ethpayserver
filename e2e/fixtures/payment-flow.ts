/**
 * Shared helpers for `tests/payment-flow-postmerge.spec.ts`.
 *
 * Split out from `fixtures/synthetic-payment.ts` rather than imported from
 * it: that file backs the long-lived nightly spec (`synthetic-payment.spec.ts`)
 * and this one backs a separate, merge-triggered spec with its own spender
 * account index (`SPENDER_ACCOUNT_INDEX` in `tests/payment-flow-postmerge.spec.ts`)
 * — keeping them apart means a change aimed at one run can never silently
 * touch the other's nonce sequence or budget constants.
 */
import { api } from './api';

export interface Checkout {
  status: string;
  amount_received: string;
  is_paid: boolean;
  payments: { tx_hash: string }[];
}

/**
 * Subscribe to the public checkout socket and resolve once the invoice
 * reports `paid`.
 *
 * Reconnects on drop — the socket only carries live events and replays
 * nothing on connect, so a transition landing inside a reconnect gap would
 * otherwise be missed forever; each gap is closed by re-reading
 * `GET /checkout/{id}`. Mirrors `waitForPaid` in `tests/synthetic-payment.spec.ts`
 * (not imported from there — see this file's header).
 */
export async function waitForPaid(
  wsUrl: (path: string) => string,
  invoiceId: string,
  timeoutMs: number,
): Promise<{ via: string; seen: string[] }> {
  const seen: string[] = [];
  const deadline = Date.now() + timeoutMs;
  const url = `${wsUrl('/checkout/ws')}?invoice_id=${encodeURIComponent(invoiceId)}`;

  for (;;) {
    const paid = await new Promise<boolean>((resolve) => {
      let timer: ReturnType<typeof setTimeout>;
      const socket = new WebSocket(url);
      const settle = (value: boolean) => {
        clearTimeout(timer);
        socket.close();
        resolve(value);
      };
      timer = setTimeout(() => settle(false), Math.max(1_000, deadline - Date.now()));
      socket.onerror = () => settle(false);
      socket.onclose = () => settle(false);
      socket.onmessage = (event: { data: unknown }) => {
        const update = JSON.parse(String(event.data)) as { type: string; status?: string };
        seen.push(update.status ? `${update.type}:${update.status}` : update.type);
        if (update.type === 'invoice_status' && update.status === 'paid') settle(true);
      };
    });
    if (paid) return { via: 'checkout WebSocket', seen };

    const checkout = await api<Checkout>(`/checkout/${invoiceId}`).catch(() => null);
    if (checkout?.is_paid) return { via: 'checkout API after a WebSocket drop', seen };

    if (Date.now() >= deadline) {
      throw new Error(
        `Invoice ${invoiceId} did not reach 'paid' within ${timeoutMs}ms. ` +
          `WebSocket saw: [${seen.join(', ') || 'nothing'}]. ` +
          `Checkout API reports: ${checkout ? `${checkout.status} (received ${checkout.amount_received})` : 'unreachable'}.`,
      );
    }
    await new Promise((r) => setTimeout(r, 2_000));
  }
}

export interface PayoutResponse {
  id: string;
  invoice_ids: string[];
  amount: string;
  asset_symbol: string;
  chain_id: string;
  status: string;
}

/**
 * Record a payout over this one invoice and settle it — the two API calls
 * `POST /stores/{id}/payouts` and `POST /stores/{id}/payouts/{id}/settle`,
 * which are the whole payout step: this server never signs or broadcasts a
 * transaction (`server/src/api/payouts.rs`), so "payout" here means the
 * merchant's own record of having swept the funds, not an on-chain send.
 *
 * `tx_hash` is a synthetic placeholder — a well-formed hash-shaped string the
 * server stores without verifying — standing in for the hash a merchant's own
 * sweep would have produced. Deliberately *not* the customer's payment hash:
 * reusing it would make a payout that claims the wrong transaction read as
 * correct.
 */
export async function createAndSettlePayout(
  storeId: string,
  token: string,
  invoiceId: string,
  destinationAddress: string,
  chainId: string,
  assetSymbol: string,
  tokenAddress: string | null,
): Promise<PayoutResponse> {
  const payout = await api<PayoutResponse>(`/stores/${storeId}/payouts`, {
    method: 'POST',
    token,
    body: {
      invoice_ids: [invoiceId],
      destination_address: destinationAddress,
      chain_id: chainId,
      asset_symbol: assetSymbol,
      token_address: tokenAddress,
    },
  });

  const placeholderTxHash = `0x${Array.from({ length: 64 }, () =>
    Math.floor(Math.random() * 16).toString(16),
  ).join('')}`;

  return api<PayoutResponse>(`/stores/${storeId}/payouts/${payout.id}/settle`, {
    method: 'POST',
    token,
    body: { tx_hash: placeholderTxHash },
  });
}
