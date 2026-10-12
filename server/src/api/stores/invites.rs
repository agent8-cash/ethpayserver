//! Store invites: bring a colleague into a store by consent.
//!
//! `POST /stores/{id}/invites` takes an email address, never a user id, and
//! answers `202` with an empty body whether or not the address has an account.
//! It cannot do otherwise: the handler never looks an account up. A create
//! endpoint leaks through its effect, not just its status (an add-by-user-id
//! route either makes the user a member or 404s, and the member list read
//! afterwards gives the answer away), so the only endpoint that cannot reveal
//! who is registered is one that does not take a user id.
//!
//! The membership is created when the holder of the mailed token redeems it
//! at `POST /users/me/invites/accept`. Until then nothing exists.
//!
//! Limitation: the code is a bearer token. Any signed-in account that holds it
//! can redeem it, and `store_invites` records who sent an invite and when it
//! was accepted but not who accepted it. A membership therefore proves
//! possession of a token, not the identity of the addressee.
//!
//! Gated on `caninviteusers`, held by Owner only. It is not
//! `canmodifystoreusers`, which also gates changing roles and removing
//! members and so is a different decision.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use auth::repository::{StoreRepository, StoreRoleRepository, UserStoreRepository};
use auth::{SessionService, StoreId};
use data_service::{InviteAcceptance, StoreInviteWriter};

use super::super::extractors::{AuthenticatedUser, StoreScopedUser, key_grants_store_permission};
use super::super::users::email::looks_like_an_email;
use crate::services::email::AccountNotice;
use crate::state::PgAppState;

/// How long a mailed invite remains redeemable.
const INVITE_TTL: chrono::Duration = chrono::Duration::days(7);

const INVITE_USERS: &str = "ethpay.store.caninviteusers";

/// Roles an invite may grant. Never `Owner`: ownership is not something an
/// invite hands out.
const INVITABLE_ROLES: [&str; 3] = ["Manager", "Employee", "Guest"];

/// The role granted when the request names none: the least that is still
/// useful.
const DEFAULT_INVITE_ROLE: &str = "Guest";

/// Defined here rather than in `api-types`: landing a shared DTO there is a
/// cross-repository merge this change cannot complete on its own, and these
/// bodies are one or two primitive fields.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateInviteRequest {
    pub email: String,
    /// `Manager`, `Employee` or `Guest`. Defaults to `Guest`.
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AcceptInviteRequest {
    pub token: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AcceptInviteResponse {
    pub store_id: Uuid,
    pub role_name: String,
}

/// Invite an email address to a store.
///
/// The response is the same for an address that has an account and one that
/// does not. Membership is created only when the address owner accepts.
#[utoipa::path(
    post,
    path = "/stores/{store_id}/invites",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(("store_id" = Uuid, Path, description = "Store ID")),
    request_body = CreateInviteRequest,
    responses(
        (status = 202, description = "Invite accepted for delivery"),
        (status = 400, description = "Invalid email address or role"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 503, description = "Email is not configured on this server"),
    )
)]
pub async fn create_store_invite<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Json(req): Json<CreateInviteRequest>,
) -> Result<StatusCode, StatusCode>
where
    A: SessionService + 'static,
{
    let has_permission = state
        .data_service
        .user_has_store_permission(user.id, StoreId(store_id), INVITE_USERS)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        && key_grants_store_permission(key_scope.as_deref(), INVITE_USERS, StoreId(store_id));
    if !has_permission {
        return Err(StatusCode::FORBIDDEN);
    }

    let email = req.email.trim().to_string();
    if !looks_like_an_email(&email) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let role_name = req.role.as_deref().unwrap_or(DEFAULT_INVITE_ROLE);
    if !INVITABLE_ROLES.contains(&role_name) {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Before any state exists: a pending invite nobody can be told about is
    // the silent failure `request_email_change` already refuses. Depends on
    // server configuration only, never on the address.
    if !state.email_sender.is_configured() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let store = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let role = state
        .data_service
        .get_default_role_by_name(role_name)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let token = state
        .data_service
        .create_store_invite(
            StoreId(store_id),
            &email,
            role.id,
            user.id,
            Utc::now() + INVITE_TTL,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let notice = AccountNotice {
        subject: format!("You have been invited to {} on random.cash", store.name),
        body: format!(
            "You have been invited to join the store \"{store}\" as {role_name}.\n\
             \n\
             Sign in (or create an account), then enter this code to accept:\n\
             \n\
             {token}\n\
             \n\
             It expires in {days} days and can only be used once. If you were not \
             expecting this, ignore this message - nothing changes unless the code \
             is entered.\n",
            store = store.name,
            days = INVITE_TTL.num_days(),
        ),
    };
    // A delivery failure is logged, not surfaced: whether the transport
    // accepted the message must not depend on the address in a way the
    // requester can read.
    if let Err(e) = state
        .email_sender
        .send_account_notice(&email, &notice)
        .await
    {
        tracing::warn!(error = %e, store_id = %store_id, "failed to send store invite");
    }

    Ok(StatusCode::ACCEPTED)
}

/// Accept a store invite with the code mailed to the invited address.
///
/// Holding the code is the proof of owning the address, so any signed-in
/// account may redeem it, including one that registered with a wallet only.
#[utoipa::path(
    post,
    path = "/users/me/invites/accept",
    tag = "users",
    security(("bearer_auth" = [])),
    request_body = AcceptInviteRequest,
    responses(
        (status = 200, description = "Membership created", body = AcceptInviteResponse),
        (status = 400, description = "Invalid, expired or already used code"),
        (status = 401, description = "Unauthorized"),
        (status = 409, description = "Already a member of that store"),
    )
)]
pub async fn accept_store_invite<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Json(req): Json<AcceptInviteRequest>,
) -> Result<Json<AcceptInviteResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    match state
        .data_service
        .accept_store_invite(req.token, user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        InviteAcceptance::Accepted { store_id, role_id } => {
            let role = state
                .data_service
                .get_store_role(role_id)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(Json(AcceptInviteResponse {
                store_id: store_id.0,
                role_name: role.role,
            }))
        }
        InviteAcceptance::AlreadyMember => Err(StatusCode::CONFLICT),
        InviteAcceptance::Invalid => Err(StatusCode::BAD_REQUEST),
    }
}
