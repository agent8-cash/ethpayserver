-- Reverses the schema half of 20261125000000_encrypt_wallet_xpub.sql only.
--
-- Cannot reverse the data: there is no key available to plain SQL to decrypt
-- xpub_ciphertext back into the plaintext `xpub` column this file recreates,
-- any more than the up migration could encrypt into it. Only safe to run
-- between the up migration and migrate_postgres's phase-2 step (which drops
-- the plaintext column outright). Run after that step, the first statement
-- below fails loudly - "column xpub does not exist" - rather than silently
-- losing data, because there is no plaintext column left to index.

CREATE UNIQUE INDEX idx_account_wallets_user_xpub ON wallets(user_id, namespace, xpub);

DROP INDEX idx_account_wallets_user_xpub_fingerprint;

ALTER TABLE wallets
    DROP COLUMN xpub_ciphertext,
    DROP COLUMN xpub_iv,
    DROP COLUMN xpub_mac,
    DROP COLUMN xpub_fingerprint;

-- Restores the constraint the up migration relaxed. Only correct if every
-- row's `xpub` is still populated, i.e. the application never ran its new
-- code against this database - the moment it does, new rows stop writing
-- `xpub` at all, and this would fail with a NOT NULL violation, which is the
-- right outcome: it means reverting is no longer safe.
ALTER TABLE wallets ALTER COLUMN xpub SET NOT NULL;
