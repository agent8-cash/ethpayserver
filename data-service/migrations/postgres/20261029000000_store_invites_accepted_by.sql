-- Records which account redeemed an invite, not just that one was.
--
-- The token is a bearer credential: any signed-in account holding it can
-- redeem it, so a membership proved only `accepted_at`, not who exercised the
-- grant. Nullable because existing accepted rows have no recorded redeemer
-- and are left as-is - this migration only adds the column going forward.
ALTER TABLE store_invites
    ADD COLUMN accepted_by UUID REFERENCES users(id) ON DELETE CASCADE;
