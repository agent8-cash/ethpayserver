-- Every redemption attempt against a store invite, accepted or refused.
--
-- store_invites records only the successful redeemer (accepted_by). A refused
-- attempt changes no invite row, so without this table a forwarded code
-- leaves no trace. One row per attempt, written in the same transaction as
-- the decision it records.
--
-- token is not a foreign key: an attempt with an unknown code is still
-- recorded, and store_id stays NULL for it. Identity and store columns use
-- ON DELETE SET NULL so an audit row outlives the account or store it names.
CREATE TABLE store_invite_attempts (
    id BIGSERIAL PRIMARY KEY,
    token UUID NOT NULL,
    store_id UUID REFERENCES stores(id) ON DELETE SET NULL,
    user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    outcome VARCHAR(32) NOT NULL
        CHECK (outcome IN ('accepted', 'email_mismatch', 'already_member', 'invalid')),
    attempted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
