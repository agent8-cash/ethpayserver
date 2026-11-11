# Technical Requirements: Non-Custodial Architecture

ETHPayServer is a non-custodial EVM payment processor. This document specifies the technical architecture that enforces and maintains the non-custodial guarantee: merchants control all funds; the server never holds, derives, or broadcasts transactions using merchant spending keys.

## Non-Custodial Guarantee

The core claim: **A merchant who registers an extended public key (xpub) with ETHPayServer retains sole custody of the funds those addresses hold.** This guarantee is enforced by design, not by trust or operational discipline.

Three properties together prove non-custody:

1. **No spending key is ever held.** The server stores only public key material. The registration endpoint (`POST /wallets`) and payment method endpoints reject extended private keys (xprv) on the version-byte check, refusing them before storage.

2. **No transaction spending merchant funds is ever broadcast.** The signing and transaction-broadcast module (`evm::transaction`) is behind a `hot-wallet` Cargo feature (off by default). No crate in this repository builds it enabled, and no CI workflow or deployment enables it. To confirm: `git grep -n hot-wallet` across every `Cargo.toml` and `.github/workflows/*.yml` returns only the feature's own declaration, the `#[cfg(...)]` gate guarding the module, and this paragraph — see `evm/README.md:66` for the module table.

3. **The server computes addresses from the merchant's public key, not a derived spending key.** Merchants authorize payment methods from their xpub, and the server derives payment addresses using only the public information in that key.

Following directly: the server cannot move merchant funds, and the server cannot recover merchant keys if a private key is lost.

## Xpub Registration

### API Endpoints

**Register a wallet (xpub) to an account:**

```
POST /wallets
Authorization: Bearer <token>
Content-Type: application/json

{
  "name": "My Payment Processor",
  "xpub": "xpub6CUGRUm3p...",
  "namespace": "eip155"  // optional, defaults to "eip155"
}
```

**Register a payment method (derive and watch addresses for a specific chain):**

```
POST /stores/{store_id}/payment-methods
Authorization: Bearer <token>
Content-Type: application/json

{
  "name": "Ethereum Sepolia",
  "chain_id": "eip155:11155111",
  "asset_symbol": "ETH",
  "wallet_id": "<wallet-uuid>"
}
```

### Key Validation

The wallet endpoint enforces that only extended **public** keys are accepted:

- Base58-decoded key material is validated against the xpub version-byte prefix (0x04880b21).
- An xprv (private key, version bytes 0x04880483) fails validation immediately.
- Validation happens server-side before any storage or key derivation — there is no code path that stores or uses an xprv.

### Storage

- **Wallets table:** Stores one row per account xpub per namespace (e.g., "eip155" for Ethereum chains). The xpub itself and a derivation counter (initially 0) are stored in plaintext — the counter tracks the next index to derive, and the xpub is needed to compute addresses on every invoice.

- **Payment methods table:** References a wallet and specifies a chain (e.g., "eip155:1" for Ethereum mainnet) and asset. Each payment method has its own derivation index (deprecated, now stored on the wallet) — when a payment method is created, it starts at the index the wallet had reached.

- **Rotation history:** `wallet_rotations` table records each account's xpub rotation, capturing the previous xpub, the derivation index at rotation time, the affected payment method, optional reason, and timestamp.

## Client-Side Key Derivation

Address derivation happens server-side only. The merchant holds the private key; the public key only is registered.

### Derivation Method: BIP-32/44

- Derivation follows BIP-32 (hierarchical deterministic wallets) and BIP-44 (account hierarchies).
- The merchant's xpub is the root for a payment method. From it, the server derives a public address at each increment of a **derivation index**.
- **No private key material ever passes through the server.** The server calls BIP-32 public-key-only derivation (`ecdsa::derive_public_child`).

### Merchant Verification

A merchant who holds the matching private key can independently verify that an address ETHPayServer claims derives from their xpub:

1. **Export the xpub** from the same wallet as the private key (e.g., MetaMask's hardwallet account settings).
2. **Derive the address locally** at the same index using the same BIP-32 path.
3. **Compare** the address the server returned on the invoice with the locally derived address.

If they match, the address was derived from the merchant's xpub and only that xpub. If they do not match, the server did not follow the merchant's key material.

## Server-Side Address Computation

### Address Generation for Invoices

1. A merchant creates an invoice for a payment method (specifying chain and asset).
2. The server looks up the payment method's associated wallet (xpub) and current derivation index.
3. The server increments the derivation index and derives the child public key at that index.
4. The server computes the Ethereum address from the public key (Keccak-256 hash of the public key, truncated to 20 bytes).
5. The address is stored on the invoice and returned to the merchant.

### Derivation Index Management

- Derivation indices are **not reset during xpub rotation.** When a merchant rotates their key, the new wallet resumes at the index the old wallet had reached (or starts at 0 if that xpub is new to the account).
- This prevents re-issuing addresses that already hold funds.
- The wallet is shared between stores and payment methods; the index lives on the wallet, not per-method.

### Payment Watching

Once an address is generated:

1. **Blockchain monitoring** watches the address for incoming transfers.
2. A successful transfer (confirmed on-chain) marks the associated invoice as paid.
3. **The server never initiates any transaction** using the payment address. It only listens for incoming transfers.

## Passkey Authentication Flow

User authentication to ETHPayServer uses passkeys (WebAuthn) to secure account access and reduce reliance on passwords. The flow is public-key cryptography, where the user's device holds the private key and the server holds only public key material.

### Registration (Adding a Passkey)

1. **User initiates registration:**
   ```
   POST /users/devices/register-begin
   Authorization: Bearer <session_token>
   ```
   Response includes a server-generated challenge (random bytes).

2. **Browser/device prompts user:**
   The client (browser/app) passes the challenge to the user's device (hardware security key, biometric, or platform authenticator). The device creates a cryptographic credential with:
   - A unique credential ID (private to the device).
   - A public key derived from device-held private key material.
   - A signature proving possession of the private key (the response to the server's challenge).

3. **Client returns the credential:**
   ```
   POST /users/devices/register-complete
   Authorization: Bearer <session_token>
   Content-Type: application/json

   {
     "name": "My YubiKey",
     "attestation": {
       "id": "<credential_id>",
       "publicKey": "<public_key_bytes>",
       "signatureCount": 0,
       "...": "// serialized WebAuthn attestation object"
     }
   }
   ```

4. **Server validates and stores:**
   - Verify the attestation signature matches the challenge and the public key.
   - Store the credential (id, public key, name, created timestamp, active flag) in the `passkey_credentials` table.
   - Record the device in the `devices` table for device enumeration and cap enforcement.

### Authentication (Signing In)

1. **User requests a signing challenge:**
   ```
   GET /auth/challenge?email=user@example.com
   ```
   Server generates a fresh challenge and responds with:
   - Challenge (random bytes).
   - List of credential IDs registered to that account (if any).

2. **Browser/device performs signing:**
   The client prompts the device. The device:
   - Matches the challenge against its stored values (to prevent replays).
   - Signs the challenge with the private key corresponding to a credential the user selects.
   - Returns the signature and credential ID.

3. **Client completes authentication:**
   ```
   POST /auth/authenticate
   Content-Type: application/json

   {
     "email": "user@example.com",
     "credential_id": "<bytes>",
     "assertion": {
       "signature": "<signed_challenge>",
       "signatureCount": 42,
       "...": "// serialized WebAuthn assertion object"
     }
   }
   ```

4. **Server verifies the signature:**
   - Look up the credential by credential ID (stored in the assertion object as `cred_id`).
   - Retrieve the public key for that credential.
   - Verify the signature over the challenge using the public key.
   - Verify the signature counter has incremented (to detect cloned keys).
   - Issue a session token.

### Re-authentication (Sensitive Operations)

Some operations (e.g., rotating a payment key, deleting an account) require proof of recent authentication. The re-auth flow is identical to authentication, but the server uses a fresh challenge and verifies the timestamp is recent (within a configured window, typically seconds to minutes).

### Device Management

- **Device cap:** An account can register a limited number of passkeys (e.g., 10) to prevent abuse.
- **Device enumeration:** `GET /users/devices` lists all registered devices with names and last-used timestamps.
- **Device deactivation:** `DELETE /users/devices/{device_id}` deactivates a passkey, preventing future authentication with that credential.
- **Device name:** Users can rename a device for clarity (e.g., "Work YubiKey" or "Personal Biometric").

### Public Key Cryptography

The security of passkey authentication rests on the device never revealing its private key:

- The device generates a keypair and stores the private key in tamper-resistant hardware or secure enclave.
- The device signs challenges with the private key but never exports it.
- The server stores and verifies only the public key.
- An attacker who compromises the server gains the public keys and credential IDs, but cannot forge signatures without the private key.

### Security Considerations

- **Challenge freshness:** Each signing operation requires a fresh challenge from the server, preventing replay attacks (an attacker cannot re-use an old signature).
- **Signature counter:** The counter in each assertion must be greater than the last assertion from that credential. A clone or rollback would cause the counter to decrease or stall, alerting the server.
- **Session isolation:** A session token issued after successful passkey authentication is tied to that signing event and cannot be transferred to another account or device.

## Sensitive Code Paths

The following are sensitive operations and have undergone human review:

- **Xpub validation and registration** (`POST /wallets`, `POST /stores/{store_id}/payment-methods`): ensures xprv rejection and proper key storage.
- **Address derivation** (wallet service, BIP-32 child key derivation): ensures only public-key material is used.
- **Passkey registration and authentication** (auth module in commons, passkey repository, API handlers): ensures proper challenge generation, signature verification, and counter tracking.
- **Passkey credential storage** (`passkey_credentials` table): ensures credentials are stored with proper schema and indexing.

## Related Documentation

- **Merchant API Integration Guide** (`docs/merchant-api-guide.md`): How to register wallets, create invoices, and monitor payments via the public API.
- **XPub Rotation Runbook** (`docs/xpub-rotation.md`): Procedure for rotating a payment key due to compromise or scheduled rotation.
- **EVM Module** (`evm/README.md`): Blockchain interaction, address derivation, and payment monitoring — see the module table for the `hot-wallet` feature gating.
