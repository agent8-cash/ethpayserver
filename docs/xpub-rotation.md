# XPub Rotation Runbook

Rotate a store's extended public key (xpub) when a key is compromised, a
developer machine is breached, or as part of a scheduled key rotation policy.

## When to rotate

- **Key compromise**: xpub exposed in logs, backups, or a stolen device.
- **Personnel change**: developer who had access leaves the team.
- **Scheduled rotation**: periodic rotation as part of security hygiene.

**Rotation is API-only today.** There is no rotate control in the dashboard:
the server exposes `POST /stores/{store_id}/wallet/rotate`
(`server/src/api/stores/wallets.rs`), and the client has no call site for it -
its Wallet tab can add, view and export a key, but not rotate one. So the
`curl` form below is not merely the scriptable route, it is the only route, and
whoever rotates a key reads the warnings in "What happens during rotation"
here rather than being walked through them on screen.

Saying so plainly because the alternative is worse than the gap: a runbook that
describes a control the reader cannot find leaves them hunting the UI during
exactly the incident - a compromised key - when they have least time to spare.

## What happens during rotation

1. **All payment methods** for the store are repointed at the account wallet
   holding the new xpub, which is created if the account does not have it yet.
2. **Derivation indices are not reset.** The destination wallet carries the
   only correct position for its own key. If the account has used that xpub
   before, rotation resumes where it left off; if the key is new, it starts at
   zero because a new key has issued nothing. `rotate_methods` repoints a
   payment method at the wallet `upsert_wallet` finds-or-creates
   (`data-service/src/postgres/wallet.rs`, `wallet_rotation.rs`) and never
   touches a counter on that wallet — there is no code path left that zeroes
   one.

   This changed when wallets moved to the account. Rotation used to write the
   new xpub onto each
   payment method and zero that method's counter, which was safe only while a
   method owned its counter outright. A wallet is shared between the methods
   and stores that use its key, so zeroing it would re-issue every address that
   key has already produced - including addresses holding funds.
3. **Existing watched addresses** remain active. In-flight invoices (pending,
   processing) continue to detect payments on old-xpub-derived addresses.
4. **New invoices** derive payment addresses from the new xpub.
5. **A rotation record** is persisted per payment method for audit, capturing
   the previous xpub and the index it had reached.

Old addresses are only deactivated when their parent invoice resolves (paid,
expired, or cancelled). No manual cleanup is needed.

**Rotation is account-wide in effect.** Because a wallet can back several
stores, rotating one store onto a new key moves only that store's payment
methods; the other stores stay on the old wallet. To move an entire account,
rotate each store, or point the stores at a new wallet and make it primary.

**Rotation is scoped to one chain family.** A key belongs to one family
(`eip155` or `tron`), so rotating an Ethereum xpub never touches a store's
Tron payment methods, and vice versa — see `namespace` below. In practice
this only ever means `eip155` today: no Tron chain is enabled or monitored,
so a store cannot have a Tron payment method to rotate in the first place.
The family split exists at the derivation and storage layer regardless, and
this doc describes that layer.

## API

```
POST /stores/{store_id}/wallet/rotate
Authorization: Bearer <token>
Content-Type: application/json

{
  "xpub": "<new-xpub>",
  "reason": "key compromise",   // optional
  "namespace": "eip155"         // optional, CAIP-2 namespace; defaults to "eip155"
}
```

### Response (200 OK)

```json
{
  "store_id": "uuid",
  "new_xpub_masked": "xpub6CUG...3fDVmz",
  "methods_rotated": 3,
  "rotations": [
    {
      "id": "uuid",
      "payment_method_id": "uuid",
      "chain_id": "eip155:11155111",
      "asset_symbol": "ETH",
      "previous_xpub_masked": "xpub6D4B...cLW5",
      "previous_derivation_index": 42,
      "rotated_at": "2026-04-25T01:14:18Z"
    }
  ]
}
```

### Error codes

| Code | Meaning |
|------|---------|
| 400  | Invalid xpub, unsupported `namespace`, or all methods on that family already use the provided xpub |
| 401  | Unauthenticated |
| 403  | User lacks `canmodifystoresettings` permission |
| 404  | Store not found or no payment methods configured |
| 409  | That xpub is already registered to another account |

## Procedure

### 1. Generate a new xpub

MetaMask, Trezor Suite and Ledger Live have no export flow for an Ethereum
account-level extended public key - see [Where do I get an
xpub?](merchant-api-guide.md#where-do-i-get-an-xpub) in the integration guide
for how to actually generate one. The key must be a valid `xpub` (base58).

### 2. Rotate via API

```bash
curl -X POST https://pay.agent8.cash/api/stores/<STORE_ID>/wallet/rotate \
  -H "Authorization: Bearer <TOKEN>" \
  -H "Content-Type: application/json" \
  -d '{"xpub": "<NEW_XPUB>", "reason": "scheduled rotation"}'
```

### 3. Verify

- Create a test invoice and confirm its payment address derives from the new
  xpub (compare the first derived address at index 0 with your wallet).
- Check that any in-flight invoices still accept payments on their existing
  (old-xpub) addresses.

### 4. Revoke old key material

- Remove the old xpub from any backups, `.env` files, or password managers.
- If the old xpub was a hardware wallet account, consider disabling that
  account path to prevent accidental reuse.

## Reversal

Rotation is reversible: call the same endpoint with the original xpub. As with
any rotation, the derivation index is **not** reset — the wallet for that xpub
already exists (it's the one just rotated off), and it resumes counting from
wherever it left off rather than re-deriving indices already handed out.
`upsert_wallet`'s `ON CONFLICT (user_id, namespace, xpub) DO UPDATE` only ever
touches `name` (`data-service/src/postgres/wallet.rs`); the same find-or-create
path a forward rotation uses, so a reversal cannot zero the counter any more
than a forward rotation onto an already-seen key can. Only reverse if the
rotation was a mistake.

## Audit trail

Rotation history is stored in the `wallet_rotations` table. Each row records:

- The previous and new xpub
- Which payment method was rotated
- The derivation index at rotation time
- Optional reason string
- Timestamp
