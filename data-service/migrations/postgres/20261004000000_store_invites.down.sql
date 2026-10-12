UPDATE store_roles
   SET permissions = permissions - 'ethpay.store.caninviteusers'
 WHERE store_id IS NULL AND role = 'Owner';

DROP TABLE IF EXISTS store_invites;
