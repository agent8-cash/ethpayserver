//! API key management: list, create, revoke, update, rotate.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use auth::{ApiKey, ApiKeyId, ApiKeyInfo, ApiKeyRepository, Role, SessionService};
use data_service::ApiKeyFullInfo;

use super::key_material::build_api_key;
use super::{ApiKeyInfoResponse, ApiKeyListResponse, CreateApiKeyPayload, UpdateApiKeyPayload};
use crate::api::api_key_permissions::validate_requested_permissions;
use crate::api::extractors::AuthenticatedUser;
use crate::state::PgAppState;

/// Response after creating an API key (includes plaintext key).
///
/// Hand-mirrored from the pinned `api-types` crate rather than imported from
/// it: that crate's `CreateApiKeyResponsePayload` predates per-key scoping and
/// carries no `permissions` field, and adding one there is the three-step
/// dance (merge in `payserver-commons`, bump the pinned rev, `cargo update`)
/// this repo's `CLAUDE.md` describes - a cross-repo change this pass cannot
/// complete alone. Hand-mirroring only grows the wire shape by a field, so a
/// client still built against the pinned type keeps working unchanged.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct CreateApiKeyResponsePayload {
    pub id: Uuid,
    pub name: String,
    pub key_prefix: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    /// The plaintext API key. Store this securely — it cannot be retrieved again.
    pub key: String,
    /// The scope actually granted.
    ///
    /// `None` means the key inherits the owner's role in full - the same
    /// thing `list_api_keys` and the rotate response report as `null` for
    /// such a key. It is deliberately not an echo of the request: an absent
    /// or empty `permissions` is stored as `NULL`, so echoing `[]` back would
    /// tell a caller who asked for a least-privilege key that it can do
    /// nothing, while handing them one that can do everything its owner can.
    /// Of the three responses that report a key's scope, this was the only
    /// one that could say the opposite of what was stored.
    pub permissions: Option<Vec<String>>,
}

/// Response after rotating an API key. See `CreateApiKeyResponsePayload` for
/// why this is hand-mirrored rather than extending the pinned type.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct RotateApiKeyResponsePayload {
    /// The new API key's ID.
    pub id: Uuid,
    pub name: String,
    pub key_prefix: String,
    pub created_at: DateTime<Utc>,
    /// The new plaintext API key. Store this securely.
    pub key: String,
    /// When the old key was deprecated (grace window starts here).
    pub old_key_deprecated_at: DateTime<Utc>,
    /// When the old key's grace window ends and it stops authenticating.
    /// Clients should show this directly instead of hardcoding "48 hours".
    pub old_key_grace_expires_at: DateTime<Utc>,
    /// Permission scope carried over from the key being rotated.
    pub permissions: Option<Vec<String>>,
}

/// Build from an `ApiKey` plus the ancillary rate-limit / deprecation /
/// permission fields not present on the auth-crate struct. Used by
/// endpoints that already have an `ApiKey` in hand (e.g. update_api_key
/// after a mutation).
pub(crate) fn api_key_info_with_rate_limit(
    key: &ApiKey,
    rate_limit_rpm: Option<i32>,
    deprecated_at: Option<DateTime<Utc>>,
    permissions: Option<Vec<String>>,
) -> ApiKeyInfoResponse {
    let info = ApiKeyInfo::from(key);
    ApiKeyInfoResponse {
        id: info.id.0,
        name: info.name,
        key_prefix: info.key_prefix,
        is_active: info.is_active,
        created_at: info.created_at,
        last_used_at: info.last_used_at,
        expires_at: info.expires_at,
        rate_limit_rpm,
        deprecated_at,
        deprecation_expires_at: deprecated_at.map(deprecation_expires_at),
        permissions,
    }
}

/// Build the wire shape from the `auth` domain type.
///
/// A free function rather than a `From` impl: `ApiKeyFullInfo` belongs to
/// `data_service` and `ApiKeyInfoResponse` to `api-types`, so neither owns
/// the other.
pub(crate) fn api_key_info_response(info: ApiKeyFullInfo) -> ApiKeyInfoResponse {
    ApiKeyInfoResponse {
        id: info.id,
        name: info.name,
        key_prefix: info.key_prefix,
        is_active: info.is_active,
        created_at: info.created_at,
        last_used_at: info.last_used_at,
        expires_at: info.expires_at,
        rate_limit_rpm: info.rate_limit_rpm,
        deprecated_at: info.deprecated_at,
        deprecation_expires_at: info.deprecated_at.map(deprecation_expires_at),
        permissions: info.permissions,
    }
}

/// Translate a `deprecated_at` into the grace-window deadline. Uses the same
/// grace-seconds value as the auth extractor, so the client-visible expiry
/// matches when the server actually starts rejecting the key.
pub(crate) fn deprecation_expires_at(deprecated_at: DateTime<Utc>) -> DateTime<Utc> {
    deprecated_at + chrono::Duration::seconds(crate::api::extractors::deprecation_grace_secs())
}

/// List all API keys for the authenticated user.
#[utoipa::path(
    get,
    path = "/users/api-keys",
    tag = "users",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "API keys listed", body = ApiKeyListResponse),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn list_api_keys<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
) -> Result<Json<ApiKeyListResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let keys = state
        .data_service
        .list_user_api_keys_full(user.id.0)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let keys = keys.into_iter().map(api_key_info_response).collect();

    Ok(Json(ApiKeyListResponse { keys }))
}

/// Create a new API key for the authenticated user.
#[utoipa::path(
    post,
    path = "/users/api-keys",
    tag = "users",
    security(("bearer_auth" = [])),
    request_body = CreateApiKeyPayload,
    responses(
        (status = 201, description = "API key created", body = CreateApiKeyResponsePayload),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn create_api_key<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Json(payload): Json<CreateApiKeyPayload>,
) -> Result<(StatusCode, Json<CreateApiKeyResponsePayload>), StatusCode>
where
    A: SessionService + 'static,
{
    let name = payload.name.trim().to_string();
    if name.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // `CreateApiKeyPayload.permissions` is `Option<Vec<String>>`: an absent
    // field deserializes to `None` (inherit the owner's role - unchanged
    // existing behaviour) and an explicit `[]` to `Some(vec![])` (a
    // deliberately powerless key), so the two no longer need collapsing
    // here - the wire type already carries the distinction this handler
    // used to have to invent. `Some([..])` still goes through the same
    // validation as before.
    let requested: Option<&[String]> = payload.permissions.as_deref();
    validate_requested_permissions(requested.unwrap_or(&[]))?;

    let (raw_key, api_key) = build_api_key(&name, user.id, payload.expires_at);

    state
        .data_service
        .create_api_key_with_permissions(&api_key, requested)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((
        StatusCode::CREATED,
        Json(CreateApiKeyResponsePayload {
            id: api_key.id.0,
            name,
            key_prefix: api_key.key_prefix,
            is_active: true,
            created_at: api_key.created_at,
            expires_at: api_key.expires_at,
            key: raw_key,
            // What was stored, not an echo of the raw payload - `requested`
            // is exactly `payload.permissions` reborrowed, so this already
            // matches `None`/`Some([])`/`Some([..])` as persisted.
            permissions: requested.map(<[String]>::to_vec),
        }),
    ))
}

/// Revoke (deactivate) an API key.
#[utoipa::path(
    delete,
    path = "/users/api-keys/{id}",
    tag = "users",
    security(("bearer_auth" = [])),
    params(
        ("id" = Uuid, Path, description = "API key ID to revoke"),
    ),
    responses(
        (status = 204, description = "API key revoked"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "API key not found"),
    )
)]
pub async fn revoke_api_key<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(id): Path<Uuid>,
) -> StatusCode
where
    A: SessionService + 'static,
{
    let key = state
        .data_service
        .get_api_key(ApiKeyId(id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);

    let key = match key {
        Ok(Some(k)) => k,
        Ok(None) => return StatusCode::NOT_FOUND,
        Err(status) => return status,
    };

    if key.user_id != user.id {
        return StatusCode::NOT_FOUND;
    }

    match state.data_service.revoke_api_key(ApiKeyId(id)).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Update an API key's settings (currently: rate_limit_rpm).
#[utoipa::path(
    patch,
    path = "/users/api-keys/{id}",
    tag = "users",
    security(("bearer_auth" = [])),
    params(
        ("id" = Uuid, Path, description = "API key ID to update"),
    ),
    request_body = UpdateApiKeyPayload,
    responses(
        (status = 200, description = "API key updated", body = ApiKeyInfoResponse),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "API key not found"),
    )
)]
pub async fn update_api_key<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateApiKeyPayload>,
) -> Result<Json<ApiKeyInfoResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    // Validate rate_limit_rpm if set
    if let Some(rpm) = payload.rate_limit_rpm
        && rpm < 1
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Verify the key belongs to this user
    let key = state
        .data_service
        .get_api_key(ApiKeyId(id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let key = match key {
        Some(k) => k,
        None => return Err(StatusCode::NOT_FOUND),
    };

    if key.user_id != user.id && user.role != Role::ServerAdmin {
        return Err(StatusCode::NOT_FOUND);
    }

    state
        .data_service
        .update_api_key_rate_limit(ApiKeyId(id), payload.rate_limit_rpm)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Preserve any existing deprecation/permission state in the response
    // rather than always returning None — prevents a stale-UI bug where the
    // client thinks the key was un-deprecated, or reset to full access,
    // after a rate-limit update.
    let auth_info = state
        .data_service
        .get_api_key_auth_info_by_id(id)
        .await
        .ok()
        .flatten();
    let deprecated_at = auth_info.as_ref().and_then(|info| info.deprecated_at);
    let permissions = auth_info.and_then(|info| info.permissions);

    Ok(Json(api_key_info_with_rate_limit(
        &key,
        payload.rate_limit_rpm,
        deprecated_at,
        permissions,
    )))
}

/// Rotate an API key: creates a new key and deprecates the old one.
///
/// The old key remains usable during the grace window (default 48h,
/// configurable via `API_KEY_DEPRECATION_GRACE_SECS`).
#[utoipa::path(
    post,
    path = "/users/api-keys/{id}/rotate",
    tag = "users",
    security(("bearer_auth" = [])),
    params(
        ("id" = Uuid, Path, description = "API key ID to rotate"),
    ),
    responses(
        (status = 201, description = "Key rotated", body = RotateApiKeyResponsePayload),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "API key not found"),
        (status = 409, description = "Key already deprecated or inactive"),
    )
)]
pub async fn rotate_api_key<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<RotateApiKeyResponsePayload>), StatusCode>
where
    A: SessionService + 'static,
{
    // Fetch the existing key via trait (for ownership + name)
    let key = state
        .data_service
        .get_api_key(ApiKeyId(id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Verify ownership
    if key.user_id != user.id {
        return Err(StatusCode::NOT_FOUND);
    }

    // Must be active
    if !key.is_active {
        return Err(StatusCode::CONFLICT);
    }

    // Check deprecation via PgDataService (the trait model doesn't have deprecated_at)
    let auth_info = state
        .data_service
        .get_api_key_auth_info_by_id(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if auth_info.deprecated_at.is_some() {
        return Err(StatusCode::CONFLICT);
    }

    // Create the replacement key + deprecate the old one atomically.
    // Without a transaction a partial failure (new key created, deprecation
    // fails) would leave TWO active keys on the account — the explicit
    // enemy of rotation.
    //
    // The new key carries over the old key's permission scope: rotation
    // swaps the secret, it does not widen what the key can do.
    let new_name = format!("{} (rotated)", key.name);
    let (raw_key, new_api_key) = build_api_key(&new_name, user.id, key.expires_at);
    let now = Utc::now();

    state
        .data_service
        .rotate_api_key_atomic(&new_api_key, auth_info.permissions.as_deref(), id, now)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((
        StatusCode::CREATED,
        Json(RotateApiKeyResponsePayload {
            id: new_api_key.id.0,
            name: new_name,
            key_prefix: new_api_key.key_prefix,
            created_at: new_api_key.created_at,
            key: raw_key,
            old_key_deprecated_at: now,
            old_key_grace_expires_at: deprecation_expires_at(now),
            permissions: auth_info.permissions,
        }),
    ))
}

// Unit tests for `requested_scope` used to live here. That helper is gone -
// `CreateApiKeyPayload.permissions` is `Option<Vec<String>>` now, so the
// absent/empty/scoped distinction it used to invent comes straight from the
// wire type - and the behaviour it covered is exercised through the real
// handler instead, in `server/tests/api_key_create_scope.rs`: a unit test
// against a deleted function proves nothing a caller can observe.
