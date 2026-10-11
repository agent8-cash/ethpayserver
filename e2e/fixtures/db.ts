import { Client } from 'pg';

const DATABASE_URL =
  process.env.E2E_DATABASE_URL || 'postgres://postgres:postgres@localhost:5432/ethpayserver_e2e';

// `payouts.store_id` and `refunds.store_id` reference `stores(id)` with no ON
// DELETE action, so they must be cleared before the row-level `DELETE FROM
// stores` below or it raises a foreign-key violation out of `beforeAll` and
// errors the whole spec file. TRUNCATE ... CASCADE used to absorb that.
//
// `users`, `stores` and `store_roles` are deliberately absent — they are
// cleared with row-level DELETEs below.
//
// `store_roles` holds the four global default roles (`store_id IS NULL`) seeded
// by migration 20241214000001, and `create_store` looks up 'Owner' there for
// every store it creates. TRUNCATE dropped them, so from the first reset onward
// every store creation answered HTTP 500 and every test needing a store failed.
//
// Listing the table is not the only way to lose it: TRUNCATE ... CASCADE
// truncates referencing tables wholesale and follows the chain, so truncating
// `users` reaches `stores` (via `stores.owner_id`) and `stores` reaches
// `store_roles`. Row-level DELETEs cascade per row instead, which takes the
// per-store roles and leaves the defaults.
const TABLES_TO_TRUNCATE = [
  'api_keys',
  'payouts',
  'refunds',
  'payment_events',
  'payments',
  'watched_addresses',
  'payment_options',
  'invoices',
  'store_payment_methods',
  'store_webhooks',
  'user_stores',
  'discoverable_authentication_challenges',
  'wallet_challenges',
  'passkey_authentication_challenges',
  'passkey_registration_challenges',
  'wallet_credentials',
  'passkey_credentials',
  'sessions',
  'devices',
];

export async function resetDatabase(): Promise<void> {
  // E2E_SKIP_DB_RESET stays a presence flag (README documents it as set/unset);
  // E2E_REMOTE is compared strictly so `E2E_REMOTE=false` cannot mean remote.
  if (process.env.E2E_SKIP_DB_RESET || process.env.E2E_REMOTE === 'true') {
    return;
  }
  const client = new Client({ connectionString: DATABASE_URL });
  await client.connect();
  try {
    await client.query(`TRUNCATE TABLE ${TABLES_TO_TRUNCATE.join(', ')} CASCADE`);
    await client.query('DELETE FROM stores');
    await client.query('DELETE FROM users');
    await client.query('DELETE FROM store_roles WHERE store_id IS NOT NULL');
  } finally {
    await client.end();
  }
}

/**
 * A user with an API key, created directly in the database.
 *
 * Registration goes through WebAuthn in a browser, which is the right way to
 * test registration and a poor way to get a credential for a suite that is
 * testing something else. Inserting the row skips a virtual authenticator, a
 * page load and a ceremony, none of which this returns anything about.
 *
 * The three encrypted columns are `NOT NULL` and hold client-side material
 * the server never reads back for an API-key request, so a placeholder is
 * honest here rather than lazy - a real value would suggest this account can
 * log in, and it cannot.
 */
export async function createUserWithApiKey(
  role: 'user' | 'server_admin' = 'user',
): Promise<{ userId: string; apiKey: string }> {
  const crypto = await import('node:crypto');
  // `ak_` because that is the shape `validate_api_key` looks for, and a key
  // that does not start with it fails for a reason that reads as "wrong
  // credential" rather than "wrong prefix".
  const apiKey = `ak_e2e_${crypto.randomBytes(18).toString('hex')}`;
  // SHA-256, and it has to be: `auth::api::api_keys` hashes the key exactly
  // this way, so anything else here produces a credential `validate_api_key`
  // can never match. Not a password hash and not meant to be - the value is
  // 144 bits of randomness this process just generated, so there is no
  // guessing to slow down and nothing for a work factor to buy. Code scanning
  // flags it on the identifier's name; the alert is dismissed as a false
  // positive with that reason.
  const keyHash = crypto.createHash('sha256').update(apiKey).digest('hex');

  const client = new Client({ connectionString: DATABASE_URL });
  await client.connect();
  try {
    const { rows } = await client.query(
      // These two columns are JSONB, and every value in them has to DESERIALISE,
      // not merely parse. `row_to_user` (data-service/src/postgres/auth/user.rs)
      // does `serde_json::from_value` into `KdfParams` and `EncryptedBlob`, so
      // `{}` is accepted by Postgres and then fails in the server as
      // `500: Failed to resolve user` on every authenticated request - which
      // reads as a broken endpoint rather than a broken fixture.
      //
      // Shapes are taken from payserver-commons `crypto/src/types.rs`, not from
      // the column COMMENTs in the migration: those say `memory_mib` and
      // `{ciphertext_base64, nonce_base64, tag_base64}`, and the actual fields
      // are `memory_kb` and `{ciphertext, iv, mac}`. The comments are stale.
      //
      // Byte fields go through `base64_bytes`, which is STANDARD base64 with
      // padding. The values are never decrypted - these tests authenticate with
      // the API key created below, never with a password - so any well-formed
      // blob does; they are zeroed rather than random to read as obviously inert.
      `INSERT INTO users (email, kdf_params, encrypted_symmetric_key,
                          recovery_verification_hash, role)
       VALUES ($1, $2, $3, 'e2e-placeholder', $4)
       RETURNING id`,
      [
        `e2e-${crypto.randomBytes(6).toString('hex')}@example.test`,
        JSON.stringify({
          algorithm: 'argon2id',
          memory_kb: 65536,
          iterations: 3,
          parallelism: 4,
          salt: Buffer.alloc(16).toString('base64'),
        }),
        JSON.stringify({
          ciphertext: Buffer.alloc(32).toString('base64'),
          iv: Buffer.alloc(16).toString('base64'),
          mac: Buffer.alloc(32).toString('base64'),
        }),
        role,
      ],
    );
    const userId = rows[0].id as string;

    // `id` is supplied, unlike for `users` above. The two tables differ:
    // `users.id` is `UUID PRIMARY KEY DEFAULT uuid_generate_v4()`, while
    // `api_keys.id` is `UUID PRIMARY KEY` with no default, so omitting it is
    // `null value in column "id" violates not-null constraint` rather than a
    // generated key. Production never hits this because the server generates
    // the id in `auth::api::api_keys`; only a fixture writing the row directly
    // has to know.
    await client.query(
      `INSERT INTO api_keys (id, user_id, name, key_hash, key_prefix, is_active)
       VALUES ($1, $2, 'e2e', $3, $4, true)`,
      [crypto.randomUUID(), userId, keyHash, apiKey.slice(0, 12)],
    );
    return { userId, apiKey };
  } finally {
    await client.end();
  }
}
