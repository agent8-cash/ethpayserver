-- Records which account redeemed an invite, not just that one was.
--
-- The token is a bearer credential: any signed-in account holding it can
-- redeem it, so a membership proved only `accepted_at`, not who exercised the
-- grant. Nullable because existing accepted rows have no recorded redeemer
-- and are left as-is - this migration only adds the column going forward.
--
-- ON DELETE SET NULL, not CASCADE like `invited_by`: this column is
-- incidental metadata on an already-made membership, not identity-defining
-- the way store_id or invited_by are. Cascading would delete the whole audit
-- row - email, invited_by, accepted_at and all - the moment the redeemer's
-- account is removed, which is exactly backwards for a column added to make
-- redemption auditable.
ALTER TABLE store_invites
    ADD COLUMN accepted_by UUID REFERENCES users(id) ON DELETE SET NULL;
