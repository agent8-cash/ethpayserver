# Payment Flow, End to End

This page walks the public payment flow in order, from a new account to a
delivered webhook. Each step names the endpoint or event that performs it, so
it can be followed by hand against a running instance.

Base URL: `https://your-instance.example.com`. Authenticated calls send
`Authorization: Bearer <token>`. For field-level detail, see
[merchant-api-guide.md](merchant-api-guide.md) and the OpenAPI spec at
`GET /api-docs/openapi.json`.

## Flow

| # | Step | Call | Result |
|---|------|------|--------|
| 1 | Signup | `POST /auth/passkey/new-user/start`, then `POST /auth/passkey/new-user/complete` | New account, with a passkey as its credential |
| 2 | Store creation | `POST /stores` | Store record; `id` is used in every later store call |
| 3 | xpub registration | `POST /wallets`, then `PUT /stores/{store_id}/wallet` | Public key stored and bound to the store |
| 4 | Payment method | `POST /stores/{store_id}/payment-methods` | Chain and asset the store accepts, derived from the store's xpub |
| 5 | Invoice creation | `POST /invoices` | Invoice in status `pending`, with payment options |
| 6 | Payment detection | Chain watcher (no call) | Invoice moves to `processing`; event `payment_detected` |
| 7 | Confirmation | Chain watcher (no call) | Invoice moves to `paid`; event `payment_confirmed` |
| 8 | Webhook delivery | `PUT /stores/{store_id}/webhook` configures the target once | Signed `POST` of each event to your URL |

## 1. Signup

Create a user with a passkey:

```bash
curl -X POST https://your-instance.example.com/auth/passkey/new-user/start \
  -H "Content-Type: application/json" -d '{ ... }'
# Complete the browser passkey ceremony with the returned challenge, then:
curl -X POST https://your-instance.example.com/auth/passkey/new-user/complete \
  -H "Content-Type: application/json" -d '{ ... }'
```

The `{ ... }` bodies are defined by the OpenAPI spec. The response to
`complete` establishes the session used for the calls below.

## 2. Store creation

```bash
curl -X POST https://your-instance.example.com/stores \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"name": "My Shop", "website": "https://myshop.example.com"}'
```

Keep the returned store `id`.

## 3. xpub registration

An xpub is a BIP-32 extended public key. Only the public key is ever sent.
A key that begins with `xprv` is rejected by `POST /wallets` and by
`POST /stores/{id}/payment-methods`, so a private key cannot be stored by
mistake.

1. Register the key as a wallet with `POST /wallets`, sending `{"xpub": "...", "name": "..."}`. Its `id` is returned.
2. Bind the wallet to the store:

```bash
curl -X PUT https://your-instance.example.com/stores/{store_id}/wallet \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"wallet_id": "<wallet id from step 1>"}'
```

Check the derived addresses before taking a payment with
`GET /stores/{store_id}/wallet`, and `GET /wallets/{id}/addresses`.

## 4. Payment method

```bash
curl -X POST https://your-instance.example.com/stores/{store_id}/payment-methods \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"chain_id": "eip155:1"}'
```

`chain_id` is a CAIP-2 string. Each payment method derives its addresses
from the store's xpub.

## 5. Invoice creation

```bash
curl -X POST https://your-instance.example.com/invoices \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: <unique key per order>" \
  -d '{"store_id": "{store_id}", "amount": "25.00", "currency": "USD"}'
```

Send `Idempotency-Key` on every create, so a retried request returns the
original invoice and does not create a second one. Amounts are decimal
strings in the invoice currency. The payer opens `/checkout/{invoice_id}`.

## 6. Payment detection

No call is needed. The chain watcher matches on-chain transfers to the
invoice's derived addresses. When a transfer is first seen:

- The invoice status becomes `processing`.
- The `payment_detected` event is sent to the store's webhook.

Poll `GET /invoices/{invoice_id}/status`, or subscribe to the WebSocket at
`/ws?token=<session_token>` for live `invoice_status` messages.

## 7. Confirmation

Once the transfer reaches the confirmation threshold:

- The invoice status becomes `paid` when the full amount is confirmed.
- The `payment_confirmed` event is sent.

If the payment is retracted by a chain reorganization, `payment_reorged` is
sent and the invoice status is recalculated. A payment that arrives after
expiry is reported as `late_paid`.

## 8. Webhook delivery

Configure the target once:

```bash
curl -X PUT https://your-instance.example.com/stores/{store_id}/webhook \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"webhook_url": "https://merchant.example.com/hooks/payserver", "enabled": true}'
```

Each delivery is a `POST` with a JSON body. Its `event_type` is one of
`payment_detected`, `payment_confirmed`, `payment_reorged`,
`invoice_expired`, `invoice_cancelled`, or `late_paid`. Verify the signature
on each delivery before acting on it.

Delivery is at-least-once. Dedupe on the `X-Webhook-Idempotency-Key` header,
not on `event_id`.

## Manual test on testnet

1. Run steps 1 to 5 and keep the invoice `id` and the checkout URL.
2. Open the checkout URL and send the displayed amount to the displayed
   address on the testnet chain.
3. Watch `GET /invoices/{invoice_id}/status` move from `pending` to
   `processing` to `paid`.
4. Confirm your webhook endpoint received `payment_detected` and then
   `payment_confirmed`, each with a valid signature.

This page stops at webhook delivery. Subscriptions, entitlements, and
renewals are out of scope here.
