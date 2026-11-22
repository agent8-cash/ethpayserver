//! Store CRUD endpoints: list, create, get, update, delete.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::services::plugins::{
    FilterVerdict, StoreCreationFilterRequest, run_store_creation_filters,
};
use auth::repository::{StoreRepository, UserStoreRepository};
use auth::{SessionService, Store, StoreId};
use data_service::store_creation::{StoreCreationError, StoreCreationWriter};

use super::super::extractors::{AuthenticatedUser, StoreScopedUser, key_grants_store_permission};
use crate::metrics;
use crate::state::PgAppState;
pub use api_types::{CreateStoreRequest, StoreResponse, UpdateStoreRequest};

/// Build the wire shape from the `auth` domain type.
///
/// A free function rather than a `From` impl: `Store` belongs to `auth` and
/// `StoreResponse` to `api-types`, so neither is local here. `api-types` does not
/// depend on `auth` deliberately - it is compiled into the browser bundle and
/// `auth` is a server-side crate.
pub(crate) fn store_response(store: Store) -> StoreResponse {
    StoreResponse {
        id: store.id.0,
        name: store.name,
        website: store.website,
        owner_id: store.owner_id.0,
        archived: store.archived,
        created_at: store.created_at,
    }
}

/// Answer a by-id store request on facts already loaded.
///
/// A store that does not exist and a store the caller has no standing in must
/// be indistinguishable, so both are `NOT_FOUND`: a different status for the
/// foreign case would confirm which store ids exist. `FORBIDDEN` is reserved
/// for a caller who can already see the store (a member) but lacks the
/// authority the route needs, since refusing them reveals nothing.
pub(crate) fn store_for_caller(
    store: Option<Store>,
    caller: auth::UserId,
    is_member: bool,
    owner_only: bool,
) -> Result<Store, StatusCode> {
    let store = store.ok_or(StatusCode::NOT_FOUND)?;
    let is_owner = store.owner_id == caller;
    if !is_owner && !is_member {
        return Err(StatusCode::NOT_FOUND);
    }
    if owner_only && !is_owner {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(store)
}

/// Whether `caller` is a member of `store_id`.
async fn is_store_member<A: SessionService>(
    state: &PgAppState<A>,
    caller: auth::UserId,
    store_id: StoreId,
) -> Result<bool, StatusCode> {
    Ok(state
        .data_service
        .get_user_store(caller, store_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_some())
}

/// Query for `GET /stores`.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
pub struct ListStoresQuery {
    /// List only archived stores. Off by default: an archived store is
    /// retired, not destroyed, and should not crowd the working list.
    #[serde(default)]
    pub archived: bool,
}

/// List stores for the authenticated user.
///
/// Archived stores are left out; `archived=true` returns only them.
#[utoipa::path(
    get,
    path = "/stores",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(ListStoresQuery),
    responses(
        (status = 200, description = "List of stores", body = Vec<StoreResponse>),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn list_stores<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Query(query): Query<ListStoresQuery>,
) -> Result<Json<Vec<StoreResponse>>, StatusCode>
where
    A: SessionService + 'static,
{
    let stores = state
        .data_service
        .get_stores_for_user(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(
        stores
            .into_iter()
            .filter(|s| s.archived == query.archived)
            .map(store_response)
            .collect(),
    ))
}

/// Create a new store.
///
/// The authenticated user becomes the store owner.
#[utoipa::path(
    post,
    path = "/stores",
    tag = "stores",
    security(("bearer_auth" = [])),
    request_body = CreateStoreRequest,
    responses(
        (status = 201, description = "Store created", body = StoreResponse),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn create_store<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Json(req): Json<CreateStoreRequest>,
) -> Result<(StatusCode, Json<StoreResponse>), Response>
where
    A: SessionService + 'static,
{
    let owner_id = user.id;

    // Plugins that may refuse store creation are asked before anything is
    // written. The count is the owner's own non-archived stores, read here
    // so the filter never works from a figure it was handed earlier. With no
    // filter installed, this read does not happen.
    if !state.store_creation_filters.is_empty() {
        let owned = StoreRepository::get_stores_for_user(&*state.data_service, owner_id)
            .await
            .map_err(|e| {
                tracing::error!(owner_id = %owner_id.0, error = %e, "cannot count stores for store-creation filter");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })?;
        let existing_store_count = owned
            .iter()
            .filter(|s| s.owner_id == owner_id && !s.archived)
            .count();
        let existing_store_count = u32::try_from(existing_store_count).unwrap_or(u32::MAX);

        if let FilterVerdict::Deny { reason } = run_store_creation_filters(
            &state.store_creation_filters,
            StoreCreationFilterRequest {
                account_id: owner_id,
                existing_store_count,
            },
        )
        .await
        {
            return Err((
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "store_creation_blocked",
                    "message": reason,
                })),
            )
                .into_response());
        }
    }

    let mut store = Store::new(&req.name, owner_id);
    if let Some(website) = req.website {
        store = store.with_website(&website);
    }

    // One unit of work: the store row and the membership that owns it, or
    // neither. These were three sequential writes, each committing on its own,
    // so a failure after the first left a store belonging to nobody - invisible
    // in a UI that lists stores by membership, and undeletable through it.
    state
        .data_service
        .create_store_owned_by(&store, owner_id)
        .await
        .map_err(|e| {
            // Log the cause. Every one of the five failure paths used to become a
            // bare 500 with `|_|`, and nothing was written anywhere, so the only
            // trace was `tower_http ... classification=Status code: 500`.
            // Diagnosing the real cause took a database inspection.
            match &e {
                StoreCreationError::MissingOwnerRole => {
                    // An operator fault with a specific fix, not a transient
                    // database error, so it is worth saying so distinctly.
                    tracing::error!(
                        owner_id = %owner_id.0,
                        "cannot create store: {e}"
                    );
                }
                StoreCreationError::Repository(inner) => {
                    tracing::error!(
                        owner_id = %owner_id.0,
                        error = %inner,
                        "cannot create store"
                    );
                }
            }
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;

    metrics::record_store_created();
    Ok((StatusCode::CREATED, Json(store_response(store))))
}

/// Get a store by ID.
///
/// User must be a member of the store.
#[utoipa::path(
    get,
    path = "/stores/{store_id}",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    responses(
        (status = 200, description = "Store details", body = StoreResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Not a member of this store"),
        (status = 404, description = "Store not found"),
    )
)]
pub async fn get_store<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<Json<StoreResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let store = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let is_member = is_store_member(&state, user.id, StoreId(store_id)).await?;
    let store = store_for_caller(store, user.id, is_member, false)?;

    Ok(Json(store_response(store)))
}

/// Update a store.
///
/// Requires `canmodifystoresettings` permission.
#[utoipa::path(
    put,
    path = "/stores/{store_id}",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    request_body = UpdateStoreRequest,
    responses(
        (status = 200, description = "Store updated", body = StoreResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Store not found"),
    )
)]
pub async fn update_store<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Json(req): Json<UpdateStoreRequest>,
) -> Result<Json<StoreResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    // Check permission - the owner must have it, AND the key (if any) must
    // be scoped to grant it.
    const MODIFY_SETTINGS: &str = "ethpay.store.canmodifystoresettings";
    let has_permission = state
        .data_service
        .user_has_store_permission(user.id, StoreId(store_id), MODIFY_SETTINGS)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        && key_grants_store_permission(key_scope.as_deref(), MODIFY_SETTINGS, StoreId(store_id));

    if !has_permission {
        return Err(StatusCode::FORBIDDEN);
    }

    let mut store = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if let Some(name) = req.name {
        store.name = name;
    }
    if let Some(website) = req.website {
        store.website = Some(website);
    }

    state
        .data_service
        .update_store(&store)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(store_response(store)))
}

/// Delete (archive) a store.
///
/// Only the store owner can delete a store.
#[utoipa::path(
    delete,
    path = "/stores/{store_id}",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    responses(
        (status = 204, description = "Store deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Only store owner can delete"),
        (status = 404, description = "Store not found"),
    )
)]
pub async fn delete_store<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode>
where
    A: SessionService + 'static,
{
    // Only owner can delete
    let store = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let is_member = is_store_member(&state, user.id, StoreId(store_id)).await?;
    store_for_caller(store, user.id, is_member, true)?;

    state
        .data_service
        .archive_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}

/// Unarchive a store, making it listable and invoice-capable again.
///
/// Only the store owner can unarchive a store.
#[utoipa::path(
    post,
    path = "/stores/{store_id}/unarchive",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    responses(
        (status = 200, description = "Store unarchived", body = StoreResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Only store owner can unarchive"),
        (status = 404, description = "Store not found"),
    )
)]
pub async fn unarchive_store<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<Json<StoreResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let store = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let is_member = is_store_member(&state, user.id, StoreId(store_id)).await?;
    let mut store = store_for_caller(store, user.id, is_member, true)?;

    store.archived = false;
    state
        .data_service
        .update_store(&store)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(store_response(store)))
}
