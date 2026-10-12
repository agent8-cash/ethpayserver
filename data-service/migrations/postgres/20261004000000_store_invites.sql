-- Store invites: how a store owner brings a colleague in.
--
-- Membership is created only when the owner of the invited ADDRESS accepts,
-- by presenting the token that was mailed to that address. The invite is
-- keyed on an email, never a user id, so creating one consults no account and
-- behaves identically for an address that has an account and one that does
-- not: nothing about who is registered flows back to the requester.
--
-- At most one pending invite per (store, address): re-inviting replaces the
-- token rather than stacking live ones.
CREATE TABLE store_invites (
    token UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    store_id UUID NOT NULL REFERENCES stores(id) ON DELETE CASCADE,
    -- Lower-cased and trimmed by the writer.
    email VARCHAR(255) NOT NULL,
    store_role_id UUID NOT NULL REFERENCES store_roles(id) ON DELETE CASCADE,
    invited_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    accepted_at TIMESTAMPTZ
);

CREATE UNIQUE INDEX idx_store_invites_pending
    ON store_invites(store_id, email) WHERE accepted_at IS NULL;

COMMENT ON TABLE store_invites IS
    'Pending and accepted store invites. A token is redeemed at most once and '
    'only before expires_at; redeeming it is what creates the membership.';

-- The permission that gates creating an invite. Deliberately not
-- canmodifystoreusers: that flag also gates changing a member's role and
-- removing members (co-owners included), a much wider power than adding a
-- colleague. Owner only; no other seeded role carries it.
UPDATE store_roles
   SET permissions = permissions || '["ethpay.store.caninviteusers"]'::jsonb
 WHERE store_id IS NULL
   AND role = 'Owner'
   AND NOT (permissions ? 'ethpay.store.caninviteusers');
