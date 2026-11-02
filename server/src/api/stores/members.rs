//! Store member management endpoints: list, add, update, remove.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use uuid::Uuid;

use std::collections::HashSet;

use serde::Serialize;
use utoipa::ToSchema;

use auth::repository::{StoreRepository, StoreRoleRepository, UserStoreRepository};
use auth::{SessionService, StoreId, UserId, UserStore};
use data_service::StoreInviteWriter;

use super::super::extractors::{StoreScopedUser, key_grants_store_permission};
use crate::state::PgAppState;
pub use api_types::{AddMemberRequest, MemberResponse, UpdateMemberRequest};

/// A listed member, and whether they joined by redeeming an invite.
#[derive(Debug, Serialize, ToSchema)]
pub struct MemberListItem {
    pub user_id: Uuid,
    pub store_id: Uuid,
    pub role_id: Uuid,
    pub role_name: String,
    pub permissions: Vec<String>,
    /// Set to the member's own id when they joined by redeeming a mailed
    /// invite, since the redeeming account is the member. `null` when an
    /// owner added them directly.
    pub accepted_by: Option<Uuid>,
}

/// List members of a store.
///
/// Requires `canviewstoreusers` permission.
#[utoipa::path(
    get,
    path = "/stores/{store_id}/members",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    responses(
        (status = 200, description = "List of members", body = Vec<MemberListItem>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Store not found"),
    )
)]
pub async fn list_store_members<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<Json<Vec<MemberListItem>>, StatusCode>
where
    A: SessionService + 'static,
{
    // Check permission
    const VIEW_USERS: &str = "ethpay.store.canviewstoreusers";
    let has_permission = state
        .data_service
        .user_has_store_permission(user.id, StoreId(store_id), VIEW_USERS)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        && key_grants_store_permission(key_scope.as_deref(), VIEW_USERS, StoreId(store_id));

    if !has_permission {
        return Err(StatusCode::FORBIDDEN);
    }

    let user_stores = state
        .data_service
        .get_store_users(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let redeemers: HashSet<Uuid> = state
        .data_service
        .invite_redeemers(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter()
        .map(|u| u.0)
        .collect();

    let mut members = Vec::new();
    for us in user_stores {
        if let Ok(Some(role)) = state.data_service.get_store_role(us.store_role_id).await {
            members.push(MemberListItem {
                user_id: us.user_id.0,
                store_id: us.store_id.0,
                role_id: us.store_role_id.0,
                role_name: role.role,
                permissions: role.permissions,
                accepted_by: redeemers.contains(&us.user_id.0).then_some(us.user_id.0),
            });
        }
    }

    Ok(Json(members))
}

/// Add a member to a store.
///
/// Requires `canmodifystoreusers` permission.
#[utoipa::path(
    post,
    path = "/stores/{store_id}/members",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    request_body = AddMemberRequest,
    responses(
        (status = 201, description = "Member added", body = MemberResponse),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Store or role not found"),
    )
)]
pub async fn add_store_member<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Json(req): Json<AddMemberRequest>,
) -> Result<(StatusCode, Json<MemberResponse>), StatusCode>
where
    A: SessionService + 'static,
{
    // Check permission
    const MODIFY_USERS: &str = "ethpay.store.canmodifystoreusers";
    let has_permission = state
        .data_service
        .user_has_store_permission(user.id, StoreId(store_id), MODIFY_USERS)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        && key_grants_store_permission(key_scope.as_deref(), MODIFY_USERS, StoreId(store_id));

    if !has_permission {
        return Err(StatusCode::FORBIDDEN);
    }

    // Verify store exists
    let _ = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Get the role by name
    let role = state
        .data_service
        .get_default_role_by_name(&req.role)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::BAD_REQUEST)?;

    let user_store = UserStore::new(UserId(req.user_id), StoreId(store_id), role.id);

    state
        .data_service
        .add_user_to_store(&user_store)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((
        StatusCode::CREATED,
        Json(MemberResponse {
            user_id: req.user_id,
            store_id,
            role_id: role.id.0,
            role_name: role.role,
            permissions: role.permissions,
        }),
    ))
}

/// Update a member's role in a store.
///
/// Requires `canmodifystoreusers` permission.
#[utoipa::path(
    put,
    path = "/stores/{store_id}/members/{user_id}",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID"),
        ("user_id" = Uuid, Path, description = "User ID")
    ),
    request_body = UpdateMemberRequest,
    responses(
        (status = 200, description = "Member updated", body = MemberResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Member not found"),
    )
)]
pub async fn update_store_member<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path((store_id, target_user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateMemberRequest>,
) -> Result<Json<MemberResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    // Check permission
    const MODIFY_USERS: &str = "ethpay.store.canmodifystoreusers";
    let has_permission = state
        .data_service
        .user_has_store_permission(user.id, StoreId(store_id), MODIFY_USERS)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        && key_grants_store_permission(key_scope.as_deref(), MODIFY_USERS, StoreId(store_id));

    if !has_permission {
        return Err(StatusCode::FORBIDDEN);
    }

    // Get the new role
    let role = state
        .data_service
        .get_default_role_by_name(&req.role)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::BAD_REQUEST)?;

    let user_store = UserStore::new(UserId(target_user_id), StoreId(store_id), role.id);

    state
        .data_service
        .update_user_store(&user_store)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    Ok(Json(MemberResponse {
        user_id: target_user_id,
        store_id,
        role_id: role.id.0,
        role_name: role.role,
        permissions: role.permissions,
    }))
}

/// Remove a member from a store.
///
/// Requires `canmodifystoreusers` permission.
/// Cannot remove the store owner.
#[utoipa::path(
    delete,
    path = "/stores/{store_id}/members/{user_id}",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID"),
        ("user_id" = Uuid, Path, description = "User ID")
    ),
    responses(
        (status = 204, description = "Member removed"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions or cannot remove owner"),
        (status = 404, description = "Member not found"),
    )
)]
pub async fn remove_store_member<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path((store_id, target_user_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, StatusCode>
where
    A: SessionService + 'static,
{
    // Check permission
    const MODIFY_USERS: &str = "ethpay.store.canmodifystoreusers";
    let has_permission = state
        .data_service
        .user_has_store_permission(user.id, StoreId(store_id), MODIFY_USERS)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        && key_grants_store_permission(key_scope.as_deref(), MODIFY_USERS, StoreId(store_id));

    if !has_permission {
        return Err(StatusCode::FORBIDDEN);
    }

    // Cannot remove the store owner
    let store = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if store.owner_id.0 == target_user_id {
        return Err(StatusCode::FORBIDDEN);
    }

    state
        .data_service
        .remove_user_from_store(UserId(target_user_id), StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}
