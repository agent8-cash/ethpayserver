#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Authorization-boundary tests for the invoice/payment list store scope.
//!
//! Regression coverage for the store-scope leak. The nil UUID used to be an
//! in-band "all
//! stores" sentinel, so `store_id=00000000-0000-0000-0000-000000000000` took
//! the `Some` arm of the scope match, skipped the admin check *and* the
//! membership check, and then dropped the `WHERE store_id` clause - handing any
//! authenticated user every invoice and payment in the deployment.
//!
//! The scope is now an `Option` with no in-band marker. These tests pin that:
//! the nil UUID must be treated as an ordinary store id, never as "all".

use super::super::{StoreScope, verify_store_access_for_query};
use ::types::StoreId;
use async_trait::async_trait;
use auth::store::{UserStore, UserStoreInfo, Store};
use auth::{Result, Role, UserId, UserInfo, repository::{StoreRepository, UserStoreRepository}};
use axum::http::StatusCode;
use chrono::Utc;
use uuid::Uuid;

/// Store repository stub. `member_of` is every store the user belongs to;
/// anything else returns "not a member". Both `get_user_store` (single-store
/// gate) and `get_stores_for_user` (membership scoping) are exercised.
struct StubStores {
    member_of: Vec<Uuid>,
}

#[async_trait]
impl StoreRepository for StubStores {
    async fn create_store(&self, _: &Store) -> Result<()> {
        unimplemented!("not exercised by the store-scope gate")
    }

    async fn get_store(&self, _: StoreId) -> Result<Option<Store>> {
        unimplemented!("not exercised by the store-scope gate")
    }

    async fn get_stores_for_user(&self, user_id: UserId) -> Result<Vec<Store>> {
        Ok(self
            .member_of
            .iter()
            .map(|id| Store {
                id: StoreId(*id),
                name: format!("store-{}", id),
                website: None,
                owner_id: user_id,
                archived: false,
                created_at: Utc::now(),
            })
            .collect())
    }

    async fn get_stores_owned_by(&self, _: UserId) -> Result<Vec<Store>> {
        unimplemented!("not exercised by the store-scope gate")
    }

    async fn update_store(&self, _: &Store) -> Result<()> {
        unimplemented!("not exercised by the store-scope gate")
    }

    async fn archive_store(&self, _: StoreId) -> Result<()> {
        unimplemented!("not exercised by the store-scope gate")
    }

    async fn delete_store(&self, _: StoreId) -> Result<()> {
        unimplemented!("not exercised by the store-scope gate")
    }
}

#[async_trait]
impl UserStoreRepository for StubStores {
    async fn get_user_store(
        &self,
        user_id: UserId,
        store_id: StoreId,
    ) -> Result<Option<UserStore>> {
        Ok(self
            .member_of
            .contains(&store_id.0)
            .then(|| UserStore::new(user_id, store_id, auth::store::StoreRoleId(Uuid::new_v4()))))
    }

    async fn add_user_to_store(&self, _: &UserStore) -> Result<()> {
        unimplemented!("not exercised by the store-scope gate")
    }
    async fn get_user_stores(&self, user_id: UserId) -> Result<Vec<UserStore>> {
        Ok(self
            .member_of
            .iter()
            .map(|id| {
                UserStore::new(
                    user_id,
                    StoreId(*id),
                    auth::store::StoreRoleId(Uuid::new_v4()),
                )
            })
            .collect())
    }
    async fn get_store_users(&self, _: StoreId) -> Result<Vec<UserStore>> {
        unimplemented!("not exercised by the store-scope gate")
    }
    async fn update_user_store(&self, _: &UserStore) -> Result<()> {
        unimplemented!("not exercised by the store-scope gate")
    }
    async fn remove_user_from_store(&self, _: UserId, _: StoreId) -> Result<()> {
        unimplemented!("not exercised by the store-scope gate")
    }
    async fn user_has_store_permission(&self, _: UserId, _: StoreId, _: &str) -> Result<bool> {
        unimplemented!("not exercised by the store-scope gate")
    }
    async fn get_user_store_info(&self, _: UserId, _: StoreId) -> Result<Option<UserStoreInfo>> {
        unimplemented!("not exercised by the store-scope gate")
    }
    async fn get_user_store_infos(&self, _: UserId) -> Result<Vec<UserStoreInfo>> {
        unimplemented!("not exercised by the store-scope gate")
    }
}

fn user_with_role(role: Role) -> UserInfo {
    UserInfo {
        id: UserId(Uuid::new_v4()),
        email: Some("merchant@example.com".to_string()),
        primary_wallet_address: None,
        created_at: Utc::now(),
        last_login_at: None,
        role,
    }
}

// =========================================================================
// The nil UUID must never mean "every store"
// =========================================================================

/// The exploit, pinned. A non-admin who is a member of exactly one real store
/// asks for the nil store. Before the fix this returned `Ok(None)` - no filter,
/// every tenant's rows. It must be a membership failure like any other store.
#[tokio::test]
async fn non_admin_passing_nil_store_id_is_forbidden_not_granted_all_stores() {
    let own_store = Uuid::new_v4();
    let repo = StubStores {
        member_of: vec![own_store],
    };
    let user = user_with_role(Role::User);

    let result = verify_store_access_for_query(&repo, &user, Some(Uuid::nil())).await;

    assert_eq!(
        result.unwrap_err(),
        StatusCode::FORBIDDEN,
        "nil store_id must be membership-checked, not treated as an all-stores sentinel"
    );
}

/// The other half of the old bug: even when the gate was passed, the nil id
/// suppressed the SQL filter. A member of the nil store must get it back as a
/// real, applied filter rather than `None`.
#[tokio::test]
async fn nil_store_id_is_returned_as_an_ordinary_scoped_filter() {
    let repo = StubStores {
        member_of: vec![Uuid::nil()],
    };
    let user = user_with_role(Role::User);

    let scope = verify_store_access_for_query(&repo, &user, Some(Uuid::nil()))
        .await
        .expect("member of the nil store is allowed to query it");

    assert_eq!(
        scope,
        StoreScope::One(StoreId(Uuid::nil())),
        "nil must round-trip as a filter, never collapse to the unfiltered case"
    );
}

// =========================================================================
// The surrounding boundary, so the gate can't be loosened unnoticed
// =========================================================================

/// This used to assert a 400, which is what made "All Stores" a dead end for
/// every merchant. Omitting `store_id` now means "everything I can
/// see", and for a non-admin that is their own memberships - the same
/// authorisation decision the single-store arm makes, applied to a set.
#[tokio::test]
async fn non_admin_without_store_id_is_scoped_to_their_memberships() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let repo = StubStores {
        member_of: vec![a, b],
    };
    let user = user_with_role(Role::User);

    let scope = verify_store_access_for_query(&repo, &user, None)
        .await
        .expect("a merchant may list across their own stores");

    match scope {
        StoreScope::Membership(ids) => {
            assert_eq!(ids, vec![StoreId(a), StoreId(b)]);
        }
        other => panic!("expected Membership, got {other:?}"),
    }
}

/// The trap in the membership arm: an empty list must stay a filter that
/// matches nothing. If it ever degrades to "no filter", a user who belongs to
/// no store reads every store on the server - the same leak by another route.
#[tokio::test]
async fn non_admin_with_no_stores_is_scoped_to_nothing_not_everything() {
    let repo = StubStores { member_of: vec![] };
    let user = user_with_role(Role::User);

    let scope = verify_store_access_for_query(&repo, &user, None)
        .await
        .expect("a user with no stores gets an empty scope, not an error");

    assert_eq!(
        scope,
        StoreScope::Membership(vec![]),
        "no memberships must mean no rows, never the unfiltered case"
    );
    assert_ne!(scope, StoreScope::All, "a non-admin can never reach All");
}

#[tokio::test]
async fn non_admin_querying_a_store_they_do_not_belong_to_is_forbidden() {
    let repo = StubStores {
        member_of: vec![Uuid::new_v4()],
    };
    let user = user_with_role(Role::User);

    let result = verify_store_access_for_query(&repo, &user, Some(Uuid::new_v4())).await;

    assert_eq!(result.unwrap_err(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn non_admin_querying_their_own_store_is_scoped_to_it() {
    let own_store = Uuid::new_v4();
    let repo = StubStores {
        member_of: vec![own_store],
    };
    let user = user_with_role(Role::User);

    let scope = verify_store_access_for_query(&repo, &user, Some(own_store))
        .await
        .expect("member may query their own store");

    assert_eq!(scope, StoreScope::One(StoreId(own_store)));
}

/// The intended all-stores path: admins only,
/// and only by omitting `store_id` entirely.
#[tokio::test]
async fn admin_without_store_id_queries_every_store() {
    let repo = StubStores { member_of: vec![] };
    let user = user_with_role(Role::ServerAdmin);

    let scope = verify_store_access_for_query(&repo, &user, None)
        .await
        .expect("admin may query all stores");

    assert_eq!(
        scope,
        StoreScope::All,
        "admin all-stores query must be unfiltered"
    );
}

/// An admin naming a store they are not a member of is still scoped to that
/// store - the role widens *reach*, it must not silently widen the filter.
#[tokio::test]
async fn admin_with_store_id_stays_scoped_to_that_store() {
    let other_store = Uuid::new_v4();
    let repo = StubStores { member_of: vec![] };
    let user = user_with_role(Role::ServerAdmin);

    let scope = verify_store_access_for_query(&repo, &user, Some(other_store))
        .await
        .expect("admin may query any store");

    assert_eq!(scope, StoreScope::One(StoreId(other_store)));
}

// =========================================================================
// Handler wiring
// =========================================================================

/// The tests above pin `verify_store_access_for_query`, which was always
/// correct - the bug was a *second*, sentinel-based copy of the scope
/// logic inlined in the two list handlers. Unit tests of the helper therefore
/// cannot catch that regression class on their own, and there is no handler
/// harness (`PgAppState` is pinned to the concrete Postgres service, so the
/// handlers are unreachable without a live database).
///
/// This closes the gap the cheap way: the handlers must own no nil-UUID logic
/// of their own. If someone reintroduces a sentinel instead of calling the
/// helper, this fails.
#[test]
fn list_handlers_do_not_reintroduce_a_nil_uuid_sentinel() {
    for (name, src) in [
        ("list.rs", include_str!("../list.rs")),
        ("payments.rs", include_str!("../payments.rs")),
        ("csv_export.rs", include_str!("../csv_export.rs")),
    ] {
        assert!(
            !src.contains("nil()"),
            "{name} references a nil UUID. Store scope must stay an Option \
             resolved by verify_store_access_for_query - an in-band sentinel a \
             caller can also supply is what leaked every store."
        );
        assert!(
            src.contains("verify_store_access_for_query"),
            "{name} must resolve store scope through verify_store_access_for_query"
        );
    }
}

// A foreign invoice and a missing one must answer alike, or the status code
// confirms which invoice ids exist.
mod invoice_visibility {
    use super::super::super::visible_invoice;
    use ::types::{InvoiceId, InvoiceStatus, StoreId, traits::InvoiceData};
    use axum::http::StatusCode;
    use uuid::Uuid;

    fn an_invoice() -> InvoiceData {
        InvoiceData {
            id: InvoiceId("inv-1".to_string()),
            store_id: StoreId(Uuid::from_bytes([3; 16])),
            currency: "USD".to_string(),
            status: InvoiceStatus::Pending,
            amount: "10".to_string(),
            amount_received: "0".to_string(),
            created_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now(),
            metadata: None,
            customer_email: None,
            extra: None,
        }
    }

    #[test]
    fn a_foreign_invoice_answers_the_same_as_a_missing_one() {
        let foreign = visible_invoice(Some(an_invoice()), false, false, None);
        let missing = visible_invoice(None, false, false, None);
        assert_eq!(foreign.unwrap_err(), StatusCode::NOT_FOUND);
        assert_eq!(missing.unwrap_err(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_member_and_an_admin_still_see_it() {
        assert!(visible_invoice(Some(an_invoice()), false, true, None).is_ok());
        assert!(visible_invoice(Some(an_invoice()), true, false, None).is_ok());
    }
}
