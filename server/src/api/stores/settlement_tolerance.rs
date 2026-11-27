//! Store settlement tolerance endpoints: get, set, delete.
//!
//! The tolerance is how far below the invoice amount a payment may fall and
//! still settle it, as a percentage of the invoice amount. Values above the
//! ceiling are refused, never clamped.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use auth::repository::StoreRepository;
use auth::{SessionService, StoreId};
use data_service::{
    DEFAULT_TOLERANCE_PERCENT, MAX_TOLERANCE_PERCENT, SettlementToleranceReader,
    SettlementToleranceWriter,
};

use super::super::extractors::StoreScopedUser;
use super::require_store_settings_permission;
use crate::services::settlement::parse_tolerance_percent;
use crate::state::PgAppState;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SettlementToleranceResponse {
    pub store_id: Uuid,
    /// The percentage in force: the store's own, or the server default.
    pub tolerance_percent: String,
    /// `"store"` or `"default"`.
    pub source: &'static str,
    pub max_tolerance_percent: &'static str,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SetSettlementToleranceRequest {
    /// Percent of the invoice amount, as a decimal string (e.g. `"0.01"`).
    pub tolerance_percent: String,
}

fn response(store_id: Uuid, own: Option<String>) -> SettlementToleranceResponse {
    let source = if own.is_some() { "store" } else { "default" };
    SettlementToleranceResponse {
        store_id,
        tolerance_percent: own.unwrap_or_else(|| DEFAULT_TOLERANCE_PERCENT.to_string()),
        source,
        max_tolerance_percent: MAX_TOLERANCE_PERCENT,
    }
}

async fn authorize<A>(
    state: &PgAppState<A>,
    user: &auth::UserInfo,
    key_scope: Option<&[String]>,
    store_id: Uuid,
) -> Result<(), StatusCode>
where
    A: SessionService + 'static,
{
    // The scope is checked, not dropped: this setting decides how far below
    // the invoice amount a payment may fall and still settle, so a key that
    // could widen it could make underpayments settle. It is store-settings
    // authority like any other.
    require_store_settings_permission(state, user, key_scope, store_id).await?;
    state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(())
}

/// Get the settlement tolerance in force for a store.
#[utoipa::path(
    get,
    path = "/stores/{store_id}/settlement-tolerance",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    responses(
        (status = 200, description = "Tolerance in force", body = SettlementToleranceResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Store not found"),
    )
)]
pub async fn get_settlement_tolerance<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<Json<SettlementToleranceResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    authorize(&state, &user, key_scope.as_deref(), store_id).await?;
    let own = SettlementToleranceReader::get_settlement_tolerance(&*state.data_service, store_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(response(store_id, own)))
}

/// Set the store's settlement tolerance. Refuses values above the ceiling.
#[utoipa::path(
    put,
    path = "/stores/{store_id}/settlement-tolerance",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    request_body = SetSettlementToleranceRequest,
    responses(
        (status = 200, description = "Tolerance set", body = SettlementToleranceResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Store not found"),
        (status = 422, description = "Value above the ceiling, or not a valid percentage"),
    )
)]
pub async fn set_settlement_tolerance<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Json(req): Json<SetSettlementToleranceRequest>,
) -> Result<Json<SettlementToleranceResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    authorize(&state, &user, key_scope.as_deref(), store_id).await?;

    // Stored as the caller wrote it after validation, so what the store reads
    // back is what it set.
    let value = parse_tolerance_percent(&req.tolerance_percent)
        .map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?
        .to_string();

    SettlementToleranceWriter::set_settlement_tolerance(&*state.data_service, store_id, &value)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(response(store_id, Some(value))))
}

/// Revert the store to the server default tolerance.
#[utoipa::path(
    delete,
    path = "/stores/{store_id}/settlement-tolerance",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("store_id" = Uuid, Path, description = "Store ID")
    ),
    responses(
        (status = 204, description = "Reverted to the server default"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Store not found"),
    )
)]
pub async fn delete_settlement_tolerance<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode>
where
    A: SessionService + 'static,
{
    authorize(&state, &user, key_scope.as_deref(), store_id).await?;
    SettlementToleranceWriter::clear_settlement_tolerance(&*state.data_service, store_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}
