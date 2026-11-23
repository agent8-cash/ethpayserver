# Technical Requirements: Non-Custodial Verification

This document is for a merchant or auditor who needs to check the non-custodial claim. Each section states a property, the code that enforces it, and how to check it. Billing and subscriptions are out of scope.

Related: [Merchant API Guide](merchant-api-guide.md), [Xpub Rotation](xpub-rotation.md).

## Claim and Summary

ETHPayServer holds public keys only. It never receives a mnemonic or an extended private key, and its default build contains no code that signs or broadcasts a transaction. Payment addresses are derived from the merchant's account xpub using public-key derivation alone.

| Property | Enforced by | How to check |
|----------|-------------|--------------|
| Extended private keys are refused | Version-byte check in the xpub decoder (`evm/src/wallet/xpub_deriver.rs`) | `git grep -n an_xprv_is_never_accepted_as_an_xpub` finds the test in `evm/src/wallet/tests.rs` |
| No signing or broadcast code in the default build | The `transaction` module is compiled only with the `hot-wallet` feature, which is off by default (`evm/Cargo.toml`, `evm/src/lib.rs`) | `git grep -n hot-wallet` returns only the feature declaration, the module gate, and the module's own documentation |
| Address derivation uses public keys only | `XpubDeriver` holds the account xpub and nothing else (`evm/src/wallet/xpub_deriver.rs`) | Read the `XpubDeriver` struct; it has no private-key field |

## 1. Xpub Registration

### Endpoints

| Method and path | Purpose |
|-----------------|---------|
| `POST /wallets` | Register an account xpub (bearer auth) |
| `GET /wallets/{wallet_id}/xpub` | Export the stored xpub |
| `GET /wallets/{wallet_id}/addresses` | List derived addresses with index, derivation path, and a `used` flag |
| `PUT /stores/{store_id}/wallet` | Attach a store to a wallet |
| `POST /stores/{store_id}/wallet/rotate` | Rotate the store's wallet to a new xpub |
| `POST /stores/{store_id}/payment-methods` | Create a payment method for one chain |

### Validation at Registration

- The key is decoded as base58 against the xpub version bytes. An `xprv` is refused on its version bytes before anything is stored.
- The key must be at account depth (`m/44'/<coin>'/<account>'`, depth 3). Shallower or deeper keys are refused with `invalid_xpub_depth`.
- BIP-49 `ypub` and BIP-84 `zpub` keys are rewritten to the `xpub` with the same key material, so one form is stored.
- The server returns three verification addresses (`VERIFICATION_ADDRESS_COUNT = 3`), derived from the registered key, so the merchant can compare them with their own wallet before any invoice uses the key.

### Storage

- `wallets`: owning account, `xpub` (public, stored as text), and `derivation_index`. The counter is the single derivation counter for that key.
- `store_wallets`: one row per store, pointing at one wallet. The foreign key is `ON DELETE RESTRICT`, so a wallet that a store still uses cannot be deleted. Deleting it would otherwise silently move the store's payments to another key.
- `wallet_rotations`: each rotation records the previous xpub, the new xpub, the previous derivation index, the payment method, an optional reason, and the time.

## 2. Client-Side Key Derivation

Mnemonics are handled only by an offline command-line tool that the merchant runs on their own machine. The server never receives a mnemonic.

### The `derive-xpub` Tool

- `derive-xpub generate` creates a new 24-word mnemonic. It prints the mnemonic to stderr and the account xpub, path, and verification addresses to stdout.
- `derive-xpub from-existing` reads a mnemonic from stdin and prints the same outputs.
- The tool's source makes no network calls. Its output is the xpub and public addresses only.

### Derivation Method

- Account keys follow BIP-44. For Ethereum the account path is `m/44'/60'/0'`.
- From the account xpub, the address at index `i` is the child at `0/i`, derived with public-key (non-hardened) child derivation.
- An Ethereum address is the last 20 bytes of the Keccak-256 hash of the uncompressed public key (`evm/src/wallet/hd_wallet.rs`).

### Merchant Verification

1. Run `derive-xpub from-existing` with the mnemonic the merchant holds, and record the xpub and verification addresses.
2. Compare the xpub with the one shown in the dashboard, or with `GET /wallets/{wallet_id}/xpub`.
3. Compare the verification addresses with the ones returned at registration.
4. For any invoice address, look up its index in `GET /wallets/{wallet_id}/addresses` and derive that index locally. The two addresses must match.

If they match, the address was derived from the merchant's xpub alone. A mismatch means the server did not derive it from that key.

## 3. Server-Side Address Computation

### Index Allocation

- Creating an invoice allocates the next index with one statement: `UPDATE wallets SET derivation_index = derivation_index + 1 ... RETURNING` (`data-service/src/postgres/wallet.rs`). The row lock on the wallet serialises concurrent invoices and any payment methods that share the key.
- Indices are never reused, so an address is issued at most once per key.

### Derivation

- The server builds an `XpubDeriver` from the stored namespace and xpub, then calls `derive_address(index)` (`evm/src/wallet/xpub_deriver.rs`).
- The deriver walks the external chain (`0`) and then the index, using public child derivation only.
- Invoice creation runs this in `server/src/api/invoices/payment_options.rs`. The address is stored on the invoice and returned to the payer.

### No Spending Key

- The only module that can sign or broadcast a transaction is `evm::transaction`, and it compiles only with the `hot-wallet` feature (off by default; see the summary table).
- Outside that module, the default build contains no private-key signer and no call that sends a raw transaction.
- The server holds no mnemonic and no extended private key (`git grep -i mnemonic` in the server, data-service, and commons sources finds only test code and a comment about recovery).

### Payment Watching

The payment monitor reads the chain for incoming transfers to the derived addresses and marks invoices paid when a transfer confirms. It does not initiate transfers.

## 4. Passkey Authentication

Users sign in with passkeys (WebAuthn). The device holds the private key. The server stores only the public key and the credential ID. The routes below are mounted under `/auth`, and the flow is implemented in the shared auth crate.

### Routes

| Step | Method and path |
|------|-----------------|
| New account with a passkey | `POST /auth/passkey/new-user/start`, then `POST /auth/passkey/new-user/complete` |
| Add a passkey to a signed-in account | `POST /auth/passkey/register/start`, then `POST /auth/passkey/register/complete` |
| Sign in | `POST /auth/passkey/login/start` (returns a `challenge_id`), then `POST /auth/passkey/login/complete` |
| List and revoke | `GET /auth/devices`, `DELETE /auth/devices/{id}`, `GET /auth/passkeys`, `DELETE /auth/passkeys/{id}` |

### Registration

1. The server generates a registration challenge, stores it with the user identifier, and returns the options.
2. The device creates a credential and signs the challenge.
3. The server checks the registration response against the stored challenge and identifier, then stores the credential ID and public key in `passkey_credentials`. The challenge is consumed on completion.

### Sign-In

1. The server generates a discoverable-credential challenge with no credential list, stores it under a random `challenge_id`, and returns it.
2. The device signs the challenge with the key for the credential the user selects.
3. On completion the server consumes the challenge. A challenge can be used once, and one older than 10 minutes is rejected.
4. The server looks up the passkey by credential ID, refuses locked accounts, and verifies the signature with the stored public key.
5. On success it updates the stored signature counter and last-used time, clears failed-login counts, and issues a session tied to a device record.

### Signature Counter

The WebAuthn library rejects an assertion whose counter is non-zero and not greater than the stored counter, which signals a possibly cloned authenticator. Its default is to enforce this, and the auth crate does not disable it.

### Freshness for Sensitive Operations

- A session is fresh for 5 minutes after it is created. The `FreshlyAuthenticatedUser` extractor (`server/src/api/extractors.rs`) requires a fresh session.
- Adding a device (`POST /me/devices`) requires a fresh session and is refused otherwise.
- Wallet rotation accepts a stale session but records a `not_fresh` alert in metrics. It does not refuse the request. Operators who need refusal must change the handler.

### Device Limit

- Each account may hold up to 10 devices (`max_devices_per_user`, default 10).
- At the cap, a login does not fail. It reuses the least recently used device.

### Recovery Material

The `users` table stores the client-wrapped account key (`encrypted_symmetric_key`), its key-derivation parameters (`kdf_params`), and `recovery_verification_hash`, which is `base64(SHA-256(Argon2id(recovery phrase, identifier)))`. The server stores a one-way verification hash of the recovery phrase and never the phrase itself.

## 5. Scope Limits of This Repository

- The browser or app code that collects passkeys and runs the sign-in ceremony is not in this repository. The server-side checks above are verifiable here. The client-side checks are not.
- The `derive-xpub` tool is the only in-repository code that handles a mnemonic. Any other client that derives keys is outside this document.

## Sensitive Code Paths

Changes in these areas are human-reviewed before merge:

- Xpub validation and registration (`POST /wallets`, `POST /stores/{store_id}/payment-methods`).
- Address derivation (`XpubDeriver`, index allocation).
- Passkey registration and sign-in (auth crate, passkey repository, auth routes).
- The `hot-wallet` feature gate and the `transaction` module.
