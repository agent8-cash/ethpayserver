# Capped-allowance pull subscriptions — design

Status: design only. Nothing in this document is built. No contract is
deployed, no approval flow is merged, and no key or signer is touched by it.
Where a decision needs a human ruling rather than more analysis, this
document says so explicitly instead of guessing.

## 0. Problem

A subscription here today is an invoice plus a renewal notice: the server
warns the customer that the current period is ending (`server/src/api/standing_warning.rs`),
and the customer must act again or the subscription lapses. Every period is a
fresh chance to churn, and no notification schedule changes that — the
customer still has to do something.

The fix category is a **capped ERC-20 allowance**: the customer signs one
approval, sized to a handful of periods rather than unlimited, and the
merchant pulls against it on schedule. The customer does nothing per period
until the cap runs out. This document designs that shape for this product
specifically — non-custodial, self-hosted, merchants holding their own keys —
not a copy of how any other service does it.

## 1. Allowance shape

- **Capped, never unlimited.** The approval amount is always a finite
  multiple of one period's charge. `approve(spender, type(uint256).max)` is
  never requested by anything this server generates, and the checkout flow
  refuses to render an approval request that isn't bounded to a specific
  token amount.
- **N periods + 1 buffer period, N merchant-chosen, server-clamped to
  `3..=12`, default 6.** The plan (what N charges per period, in what token)
  is a private-billing concept and out of scope here; this server's only job
  is to refuse to construct an approval request outside the clamp, so a
  misconfigured plan can't accidentally produce either a near-unlimited
  allowance or a cap so tight it exhausts before the merchant can react. The
  buffer period absorbs one late or skipped pull cycle without forcing an
  immediate re-approval.
- **At exhaustion**, the subscription does not need a new concept: it drops
  into the *existing* invoice-and-renewal-notice path. The same
  `X-Account-Warning` / `X-Account-Checkout-Url` mechanism that already warns
  a customer before a period lapses is reused, except the thing it now asks
  for is a fresh approval transaction on the same subscription id, not a
  brand-new signup. That is "re-up without a full re-subscribe": the
  subscription identity, merchant relationship and billing history are
  untouched — only the allowance is refreshed. No new UI concept, no new
  endpoint shape; the exhaustion warning is the renewal warning this server
  already ships, pointed at a different action.

## 2. Who pulls, and what's in the path

Three options were on the table: a router contract of ours, a per-store
contract of ours, or a direct `transferFrom` by the merchant's own key.

**Chosen: direct `transferFrom` by the merchant's own key. No contract of
ours goes into the payment path.**

Today this server never holds a spending key for anything — merchants hand
over an `xpub`, the server derives receiving *addresses* from it
(`evm/src/wallet.rs::derive_evm_address`), and `validate_xpub` refuses an
`xprv` outright. The merchant, not the server, holds the private key behind
every derived address, in their own wallet, off this server entirely. That
property does not change for pull subscriptions: the ERC-20 `spender` in the
approval is one of the merchant's own derived addresses, and the merchant's
own wallet — not this server — is what calls `transferFrom(customer,
merchant_address, amount)` against the token contract on the pull date. The
`IERC20` ABI this needs (`allowance`, `approve`, `transferFrom`, the
`Approval` event) already exists in `evm/src/tokens.rs`, unused until now.

This server's job is three things, none of which is custody or signing:

1. **Build the approval request** the customer signs client-side: `spender`
   = the merchant's derived address for this subscription (reusing existing
   derivation, no new address logic), `amount` = the capped allowance from
   §1.
2. **Observe allowance state on-chain.** The EVM monitor already watches
   `Transfer` events per address (`evm/src/monitor/`); this adds `Approval`
   events and periodic `allowance()` reads for the same (owner, spender,
   token) tuples it already watches, same pipeline, new event kind.
3. **Tell the merchant a pull is due**, via the existing webhook mechanism
   (`api-types/src/webhook.rs` pattern) — a "pull due" notification, not a
   pull instruction with any key material in it. The merchant's own system
   decides how and when to actually submit the `transferFrom`.

Trust cost of this choice: **none added.** The server's custody posture is
identical to today's — it never sees or touches a spending key, for the
merchant's receiving address or anyone else's. A router or per-store contract
of ours would have been a new piece of infrastructure between payer and
merchant that doesn't exist today, and would have reopened exactly the
question this product's non-custodial pitch exists to foreclose. Rejected
for that reason, not for engineering cost.

## 3. Revocation

The customer cancels unilaterally, on-chain, by calling the token contract's
own `approve(spender, 0)` from their own wallet — standard ERC-20, available
to any approval holder regardless of what this server or the merchant do or
don't do. Nothing new has to be built for the *capability* to exist; it's
already inherent in the mechanism chosen in §2 (plain ERC-20 allowance, no
wrapper contract to add friction).

What this server adds is **detection**: the same `Approval`-event
observation from §2.2 catches an allowance dropping to zero (or below the
amount a pull would need) for a tracked (owner, spender, token) tuple, flags
it, and the same webhook path from §2.3 tells the merchant immediately so
billing stops expecting pulls for that period instead of finding out from a
failed pull later.

## 4. Chain coverage

The ticket cites "six configured chains"; the actual enumeration in code
today is wider and worth stating precisely rather than against a number that
may already be stale: `evm/src/network.rs::EvmNetwork` lists twelve EVM
networks (Ethereum, Polygon, Arbitrum, Optimism, Base, Avalanche, BSC, zkSync,
Linea, Scroll, Fantom, Gnosis), and `ChainId` (payserver-commons,
`types/src/types/chain.rs`) carries five CAIP-2 namespaces: `eip155` (EVM),
`tron`, `solana`, `monero`, `bip122` (Bitcoin-family).

The rule that actually governs coverage, independent of which specific chains
are enabled on a given deploy:

| Namespace | ERC-20 allowance/`transferFrom`? | Pull-capable? |
| --- | --- | --- |
| `eip155` (any `EvmNetwork`) | Yes — standard ERC-20, uniform ABI across every EVM chain | **Yes, all of them, uniformly.** No chain-by-chain carve-out needed; this is the point of ERC-20 standardization. |
| `tron` | No — TRC-20 is a different token standard/ABI; `ChainFamily::Tron` here only derives addresses, there is no Tron EVM/RPC/monitor module in either repo | No — degrades to invoice. Matches the ticket's own example: Tron-as-one-time-only is a real constraint here too, not a one-off decision made elsewhere. |
| `solana`, `monero`, `bip122` | No — no account-based allowance concept exists on any of these (UTXO or otherwise); `ChainFamily::from_namespace` already returns `None` for them, i.e. not even address derivation is implemented | No, structurally, not just "not yet." Always degrades to invoice. |

So the design doesn't need a hand-maintained chain list at all: "is this
chain's `ChainId` namespace `eip155`?" (`ChainId::is_evm()`, which already
exists) is the entire pull-eligibility check. Anything else is invoice-only,
today and for the foreseeable future, because the underlying chain has no
allowance primitive to build on.

## 5. Schema impact

Billing's own tables (plan, price, commission) are private and out of scope
for this repo by the public-repo content rule — this server has never held
that data and shouldn't start now. What *this* repo needs is on-chain
mechanism state, the same category `account_standing` already holds for the
good-standing mirror, and it can follow that table's existing pattern (push
in from private billing / observed by the monitor, read by a filter) rather
than inventing a new sync shape:

```
pull_allowance_state(
  subscription_ref   text primary key,   -- opaque id from private billing; no plan/price here
  chain_id           text not null,      -- CAIP-2, e.g. "eip155:1"
  token_address      text not null,
  owner_address      text not null,      -- customer
  spender_address    text not null,      -- merchant's derived address
  periods_capped     integer not null,   -- N + buffer, a count, not an amount
  periods_remaining  integer not null,
  allowance_observed text not null,      -- raw token units, last read/observed on-chain
  last_pull_at       timestamptz null,
  last_pull_tx_hash  text null,
  revocation_detected      boolean not null default false,
  revocation_detected_at   timestamptz null,
  exhausted                boolean not null default false
)
```

Every field here is mechanism state (addresses, counts, raw token units,
timestamps) — no fiat price, no commission, nothing the public-repo content
rule reserves for `payserver-billing`. The one shared-type change is on the
`payserver-commons` side: a new `ChainEventKind` variant so the wire protocol
between chain monitors and this server can carry an approval observation as a
first-class event instead of overloading `PaymentDetected`. See the
companion note in `payserver-commons`.

## 6. Failure semantics

A failed pull must distinguish three causes, because (per the ticket) the
retry policy differs by cause — retry scheduling itself is a separate ticket
and this section only names the taxonomy, not the policy:

| Cause | How it's told apart | Shape |
| --- | --- | --- |
| **Insufficient balance** | Allowance in `pull_allowance_state` is still present and unexhausted, but the token's `balanceOf(owner)` was below the pull amount at call time | Transient, customer-side. Likely retryable soon without any new approval. |
| **Revoked** | `revocation_detected = true` (observed per §3) | Not retryable as a pull. The merchant's own `transferFrom` will also simply revert on-chain if attempted, so this is belt-and-suspenders, not the only guard. Falls back to the §1 re-approval path. |
| **Exhausted** | `periods_remaining = 0`, reached by successful pulls, not by a revocation event | Not a failure in the "something broke" sense — an expected end-of-cap. Already distinguished from revocation by *how* the allowance reached zero, not by its value alone. Falls back to the same §1 re-approval path as revocation, but billing should not log it as an incident the way a revocation might warrant flagging. |

All three are derivable from fields the schema in §5 already needs for other
reasons (allowance value, remaining-periods counter, a revocation flag) — no
fourth field or state machine is needed just to tell these apart.

## 7. The one open question

Per the correction on this ticket: the decision this document produces is
exactly one question, posed against a concrete proposal rather than a blank
page.

**Does a contract of ours go into the payment path?**

This document's proposal: **no.** §2 chose direct `transferFrom` by the
merchant's own key specifically to keep this server's trust posture
identical to today's xpub-derivation model — nothing of ours sits between
payer and merchant, which is a real part of what's sold today. That
preserves the status quo rather than changing it; it is a recommendation, not
a foreclosure. The person who rules on this trade-off is Gus, against this
proposal.

## Non-goals (explicitly out of scope for this ticket)

- No contract deployment, no approval flow merged, no key or signer touched.
- Retry **scheduling** policy (the cause taxonomy is in scope, the schedule
  isn't — separate ticket).
- The actual billing/plan/pricing schema — private, in `payserver-billing`.
- Reading or depending on any competitor's deployed contracts; this design
  starts from what fits this server's own custody model, not from a
  teardown.
