//! Which stores a listing query may read, and how a scoped API key narrows it.
//!
//! Split out of `invoices::mod` - `StoreScope` and the functions that
//! produce or narrow it are a self-contained concern from the rest of that
//! file's per-invoice handling.

use axum::http::StatusCode;

use ::types::{InvoiceQueryParams, PaymentQueryParams, StoreId};
use auth::repository::UserStoreRepository;

use crate::api::extractors::key_grants_store_permission;

/// Which stores a listing query may read.
///
/// An enum rather than `Option<StoreId>` because there are three answers, and
/// the two that mean "more than one store" are not interchangeable. Conflating
/// them is the entire bug class here: a nil-UUID sentinel that meant "all"
/// leaked every store, and an empty membership list silently meaning "no
/// filter" would be the same leak wearing different clothes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoreScope {
    /// One store, membership already checked.
    One(StoreId),
    /// Every store the caller belongs to. May be empty, which matches nothing.
    Membership(Vec<StoreId>),
    /// Every store on the server. `ServerAdmin` only.
    All,
}

impl StoreScope {
    /// Apply this scope to invoice query params.
    pub(crate) fn apply_invoice(&self, params: InvoiceQueryParams) -> InvoiceQueryParams {
        match self {
            StoreScope::One(id) => params.with_store_id(*id),
            StoreScope::Membership(ids) => params.with_store_ids(ids.clone()),
            StoreScope::All => params,
        }
    }

    /// Apply this scope to payment query params.
    pub(crate) fn apply_payment(&self, params: PaymentQueryParams) -> PaymentQueryParams {
        match self {
            StoreScope::One(id) => params.with_store_id(*id),
            StoreScope::Membership(ids) => params.with_store_ids(ids.clone()),
            StoreScope::All => params,
        }
    }
}

/// Narrow a resolved [`StoreScope`] by what the authenticating key's stored
/// permission set grants for `policy`. Session auth and an unscoped or
/// `unrestricted` key pass every store through unchanged - `None`/`unrestricted`
/// are exactly the cases `key_grants_store_permission` grants everything for -
/// so this only ever shrinks what a narrowly-scoped key can list, never widens
/// what the caller's own store membership already allowed.
///
/// `StoreScope::One` came from an explicit `?store_id=` the caller named, so a
/// key that doesn't cover it is refused outright (matching every other
/// single-store permission check in this file) rather than silently returning
/// zero rows for a store the caller asked for by ID.
pub(crate) fn narrow_scope_by_key(
    scope: StoreScope,
    key_scope: Option<&[String]>,
    policy: &str,
) -> Result<StoreScope, StatusCode> {
    match scope {
        StoreScope::One(id) => {
            if key_grants_store_permission(key_scope, policy, id) {
                Ok(StoreScope::One(id))
            } else {
                Err(StatusCode::FORBIDDEN)
            }
        }
        StoreScope::Membership(ids) => Ok(StoreScope::Membership(
            ids.into_iter()
                .filter(|id| key_grants_store_permission(key_scope, policy, *id))
                .collect(),
        )),
        StoreScope::All => Ok(StoreScope::All),
    }
}

/// Resolve the store scope for a list/export query, verifying access.
///
/// `Some(id)` is membership-checked and returned as a filter; `None` means
/// "every store" and is permitted for server admins only.
///
/// The scope is deliberately an `Option` rather than a nil-UUID sentinel. A
/// sentinel is a value a caller can also supply, and when it was one, passing
/// `store_id=00000000-0000-0000-0000-000000000000` took the `Some` arm, skipped
/// the admin check *and* the membership check, and then dropped the `WHERE
/// store_id` clause - handing any authenticated user every invoice and payment
/// in the deployment. Keep the two cases in the type; do not
/// reintroduce an in-band marker.
///
/// This is also the membership test behind the live status stream, and the
/// websocket gate calls it (`entitlement_of` in `api/ws.rs`) for every event on
/// every open socket, immediately before the send - as well as on connect and
/// on each subscription-upkeep tick. It uses only the `store_id: None` arm; the
/// `Some(store)` arm is not reached by the socket path. Loosening it here would
/// widen both who is sent a tenant's live payment events and which channels an
/// open socket subscribes to, not only what the list endpoints return. A change
/// to its semantics needs the websocket tests in
/// `server/tests/ws_store_scope.rs` run alongside the query tests.
pub(crate) async fn verify_store_access_for_query<D>(
    data_service: &D,
    user: &auth::UserInfo,
    store_id: Option<uuid::Uuid>,
) -> Result<StoreScope, StatusCode>
where
    D: UserStoreRepository + ?Sized,
{
    match store_id {
        Some(id) => {
            let is_member = data_service
                .get_user_store(user.id, StoreId(id))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .is_some();
            if !is_member && user.role != auth::Role::ServerAdmin {
                return Err(StatusCode::FORBIDDEN);
            }
            Ok(StoreScope::One(StoreId(id)))
        }
        // No store_id means "everything I can see". For an admin that is the
        // whole server; for anyone else it is their own memberships.
        //
        // This used to be a flat 400 for non-admins, which made the "All Stores"
        // sidebar option a dead end on Invoices and Payments - the client asked,
        // the server refused, and the UI reported it as "pick a store".
        // Answering with the caller's own stores is the same
        // authorisation decision the `Some` arm makes, applied to a set.
        None => {
            if user.role == auth::Role::ServerAdmin {
                return Ok(StoreScope::All);
            }
            let memberships = data_service
                .get_user_stores(user.id)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            // Deliberately still a filter when empty: a user who belongs to no
            // store sees nothing, not everything.
            Ok(StoreScope::Membership(
                memberships.into_iter().map(|m| m.store_id).collect(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::Policies;

    fn store(n: u8) -> StoreId {
        StoreId(uuid::Uuid::from_bytes([n; 16]))
    }

    /// The case the `Membership` branch exists for, and the one a listing
    /// query takes when no `store_id` is given: the owner belongs to two
    /// stores, the key is scoped to one of them, and only that one may be
    /// read. Without the `.filter(..)` this returns both, which is a
    /// cross-store read through a key that was deliberately narrowed.
    #[test]
    fn a_store_scoped_key_narrows_membership_to_the_store_it_names() {
        let (a, b) = (store(1), store(2));
        let key = [format!("{}:{}", Policies::STORE_VIEW_INVOICES, a.0)];

        assert_eq!(
            narrow_scope_by_key(
                StoreScope::Membership(vec![a, b]),
                Some(&key),
                Policies::STORE_VIEW_INVOICES,
            ),
            Ok(StoreScope::Membership(vec![a])),
            "a key scoped to one store must not read the owner's other stores"
        );
    }

    /// A key scoped to a store its owner is not a member of narrows to
    /// nothing, rather than falling back to the owner's full membership.
    /// `Membership(vec![])` matches no store, which is the point.
    #[test]
    fn a_key_scoped_to_an_unrelated_store_narrows_membership_to_nothing() {
        let (a, b, other) = (store(1), store(2), store(9));
        let key = [format!("{}:{}", Policies::STORE_VIEW_INVOICES, other.0)];

        assert_eq!(
            narrow_scope_by_key(
                StoreScope::Membership(vec![a, b]),
                Some(&key),
                Policies::STORE_VIEW_INVOICES,
            ),
            Ok(StoreScope::Membership(vec![])),
        );
    }

    /// A key with no scope at all inherits the owner's reach, so membership
    /// passes through untouched. This is the pre-scoping behaviour every key
    /// issued before this feature still relies on.
    #[test]
    fn an_unscoped_key_leaves_membership_untouched() {
        let (a, b) = (store(1), store(2));

        assert_eq!(
            narrow_scope_by_key(
                StoreScope::Membership(vec![a, b]),
                None,
                Policies::STORE_VIEW_INVOICES,
            ),
            Ok(StoreScope::Membership(vec![a, b])),
        );
    }

    /// A grant written without a store suffix means "on every store the owner
    /// can reach", so it also leaves membership untouched. Distinguishing this
    /// from the store-suffixed form above is the whole of
    /// `key_grants_store_permission`'s parsing.
    #[test]
    fn a_bare_policy_grant_leaves_membership_untouched() {
        let (a, b) = (store(1), store(2));
        let key = [Policies::STORE_VIEW_INVOICES.to_string()];

        assert_eq!(
            narrow_scope_by_key(
                StoreScope::Membership(vec![a, b]),
                Some(&key),
                Policies::STORE_VIEW_INVOICES,
            ),
            Ok(StoreScope::Membership(vec![a, b])),
        );
    }

    /// A key scoped to a *different* policy on the right store does not grant
    /// this one - otherwise any scope would collapse into every scope.
    #[test]
    fn a_key_scoped_to_another_policy_narrows_membership_to_nothing() {
        let (a, b) = (store(1), store(2));
        let key = [format!("{}:{}", Policies::STORE_MODIFY_SETTINGS, a.0)];

        assert_eq!(
            narrow_scope_by_key(
                StoreScope::Membership(vec![a, b]),
                Some(&key),
                Policies::STORE_VIEW_INVOICES,
            ),
            Ok(StoreScope::Membership(vec![])),
        );
    }
}
