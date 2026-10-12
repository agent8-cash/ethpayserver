//! Store management API endpoints.
//!
//! All endpoints require authentication. Store-level permissions are checked
//! for operations on specific stores.

mod crud;
mod invites;
mod members;
mod payment_methods;
mod settings;
mod settlement_tolerance;
mod token_policy;
mod wallets;
mod webhooks;

// Re-export all handlers and types for route registration.
pub use crud::*;
pub use invites::*;
pub use members::*;
pub use payment_methods::*;
pub use settings::*;
pub use settlement_tolerance::*;
pub use token_policy::*;
pub use wallets::*;
pub use webhooks::*;

use axum::http::StatusCode;

use auth::SessionService;
use auth::repository::UserStoreRepository;

use crate::api::extractors::key_grants_store_permission;
use crate::state::PgAppState;

/// Re-exported so this crate has exactly one masking rule. It used to keep its
/// own byte-identical copy, so a single `RotateWalletResponse` could have
/// carried two different rules the moment either changed.
pub(crate) use api_types::mask_xpub;

/// A status, optionally with a reason the caller can read.
///
/// Defined in `crate::api` - `invoices`' filter builders need the same
/// status-plus-reason shape, and this module's own doc comment already argues
/// against a second byte-identical copy.
pub use crate::api::ApiErr;

/// The status a repository error deserves, and a reason the caller can read.
///
/// `Conflict` has to survive the trip: an xpub already registered
/// to another account is refused, and that refusal is reachable from the
/// ordinary payment-method form, not just from `POST /wallets`. Collapsing
/// every repository error into a 500 turned a merchant pasting the wrong key
/// into an opaque server error with nothing to act on.
///
/// The reason travels with it, because a bare status is barely better. The
/// client renders the response body after the code (`ApiError::Http`), so a
/// 409 with no body reaches the merchant as the literal string
/// "HTTP error 409:" - a dead end with the one useful word missing. That is
/// what the payment-method form actually showed.
///
/// Only `Conflict` and `NotFound` carry their message out. Those two are
/// written by this codebase for exactly this purpose; everything else may hold
/// a database error, whose text can name columns, constraints and hosts, and
/// none of that belongs in a response.
pub fn repository_error(err: data_service::RepositoryError) -> ApiErr {
    match err {
        data_service::RepositoryError::Conflict(msg) => ApiErr(StatusCode::CONFLICT, msg),
        data_service::RepositoryError::NotFound(msg) => ApiErr(StatusCode::NOT_FOUND, msg),
        _ => ApiErr(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error".to_string(),
        ),
    }
}

/// Check if user can modify store settings (admin or has permission).
///
/// `key_scope` is the calling API key's store-permission scope, if any (see
/// `StoreScopedUser`) - `None` for session auth and for a key with no
/// narrower scope. The admin bypass is unaffected by it: a `ServerAdmin`
/// key that has actually been scoped below "unrestricted" is already
/// downgraded to `Role::User` before this runs (`validate_api_key`), so by
/// the time `user.role == ServerAdmin` reaches this function, the key (if
/// any) already grants everything - intersecting it again would be a
/// no-op.
pub(crate) async fn require_store_settings_permission<A: SessionService>(
    state: &PgAppState<A>,
    user: &auth::UserInfo,
    key_scope: Option<&[String]>,
    store_id: uuid::Uuid,
) -> Result<(), StatusCode> {
    if user.role == auth::Role::ServerAdmin {
        return Ok(());
    }

    const MODIFY_SETTINGS: &str = "ethpay.store.canmodifystoresettings";
    let has_permission = state
        .data_service
        .user_has_store_permission(user.id, auth::StoreId(store_id), MODIFY_SETTINGS)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        && key_grants_store_permission(key_scope, MODIFY_SETTINGS, auth::StoreId(store_id));

    if has_permission {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

#[cfg(test)]
mod tests;
