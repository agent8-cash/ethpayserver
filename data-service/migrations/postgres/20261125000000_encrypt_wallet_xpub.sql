-- Encrypt wallets.xpub at rest (#581).
--
-- A leaked xpub cannot move money - non-custodial, no spending key on this
-- server - but it is a total, permanent privacy breach: the holder derives
-- every address the merchant will ever receive payments at, forward
-- indefinitely. Until now `wallets.xpub` was plaintext, so a database dump,
-- a stolen backup or a read replica handed over every merchant's entire
-- future payment history.
--
-- Deliberately a two-column-set rather than one JSONB blob: the application
-- already has a settled encrypted-blob shape (`crypto::types::EncryptedBlob`
-- - ciphertext/iv/mac) used for `auth`'s client-side encrypted keys, and
-- three BYTEA columns bind directly with no (de)serialisation step, unlike
-- the JSONB `encrypted_symmetric_key` pattern that shape also has elsewhere.
--
-- Deliberately NOT a one-shot "encrypt in place": encryption uses a random
-- IV (same plaintext never produces the same ciphertext twice, which is
-- exactly what stops the unique index below leaking equality information),
-- and only the application can perform that encryption - there is no key
-- available to plain SQL to do it here, and there must never be one sitting
-- in a migration file either. The new columns are added nullable; `cmd/
-- migrate_postgres` backfills every existing row immediately after running
-- this file (same deploy, before `server`/`evmmonitor` start), then tightens
-- them to NOT NULL and drops the plaintext `xpub` column in one followup
-- statement once the backfill has actually run. See migrate_postgres.rs.

-- `xpub` loses its NOT NULL here, not only in the followup drop. From this
-- migration on, every write goes through the Rust code in postgres/wallet.rs,
-- which no longer supplies it at all - so an INSERT or UPDATE through the
-- application between this migration and the backfill (a deploy mid-rollout,
-- or any environment, such as a test database, that only ever runs the
-- tracked migrations and never runs migrate_postgres's backfill step) would
-- otherwise fail the old constraint on a column nothing writes to any more.
-- The column is retired in substance here; the followup drop is cleanup, not
-- correctness.
ALTER TABLE wallets ALTER COLUMN xpub DROP NOT NULL;

ALTER TABLE wallets
    ADD COLUMN xpub_ciphertext BYTEA,
    ADD COLUMN xpub_iv BYTEA,
    ADD COLUMN xpub_mac BYTEA,
    ADD COLUMN xpub_fingerprint BYTEA;

COMMENT ON COLUMN wallets.xpub_ciphertext IS
    'AES-256-CBC ciphertext of the xpub (crypto::symmetric::encrypt). NULL '
    'only transiently, for a row not yet backfilled by migrate_postgres.';
COMMENT ON COLUMN wallets.xpub_iv IS
    'Random 128-bit IV paired with xpub_ciphertext. Never reused - two rows '
    'holding the same plaintext have different ciphertext, which is why '
    'every uniqueness check and lock in postgres/wallet.rs keys on '
    'xpub_fingerprint instead of this column or xpub_ciphertext.';
COMMENT ON COLUMN wallets.xpub_mac IS
    'HMAC-SHA256 of (iv || ciphertext), checked before decrypting.';
COMMENT ON COLUMN wallets.xpub_fingerprint IS
    'Deterministic HMAC-SHA256 of the plaintext xpub, keyed by the same '
    'master key as the encryption above. Stands in for the plaintext in '
    'every equality check: encryption''s random IV means the ciphertext '
    'itself can never be compared across rows for the same key.';

-- Replaces the plaintext-keyed index. New rows populate the fingerprint
-- immediately (postgres/wallet.rs); backfilled rows get one from
-- migrate_postgres before this index is relied on for anything. NULLs are
-- distinct from each other in a unique index, so creating this now, before
-- the backfill runs, cannot falsely collide two not-yet-backfilled rows.
DROP INDEX idx_account_wallets_user_xpub;

CREATE UNIQUE INDEX idx_account_wallets_user_xpub_fingerprint
    ON wallets(user_id, namespace, xpub_fingerprint);
