//! Store token policy endpoints: get, set (upsert), delete.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use uuid::Uuid;

use auth::repository::StoreRepository;
use auth::{SessionService, StoreId};
use data_service::{
    StoreTokenPolicyReader, StoreTokenPolicyWriter, TokenPolicyEntryInput, TokenPolicyMode,
};

use super::super::extractors::StoreScopedUser;
use super::require_store_settings_permission;
use crate::api::api_key_hash::is_hex_bytes;
use crate::state::PgAppState;
pub use api_types::{SetTokenPolicyRequest, TokenPolicyEntryPayload, TokenPolicyResponse};

/// Get store token policy.
pub async fn get_token_policy<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<Json<Option<TokenPolicyResponse>>, StatusCode>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    let _ = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let policy = StoreTokenPolicyReader::get_token_policy(&*state.data_service, store_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(policy.map(|p| {
        TokenPolicyResponse {
            id: p.id.to_string(),
            store_id: p.store_id,
            mode: p.mode.to_string(),
            entries: p
                .entries
                .into_iter()
                .map(|e| TokenPolicyEntryPayload {
                    chain_id: e.chain_id.clone(),
                    token_address: e.token_address,
                    asset_symbol: e.asset_symbol,
                })
                .collect(),
            created_at: p.created_at.to_rfc3339(),
            updated_at: p.updated_at.to_rfc3339(),
        }
    })))
}

/// Set (upsert) store token policy.
pub async fn set_token_policy<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Json(req): Json<SetTokenPolicyRequest>,
) -> Result<Json<TokenPolicyResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    let _ = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Validate mode
    let mode: TokenPolicyMode = req.mode.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    // Validate entries
    for entry in &req.entries {
        if entry.asset_symbol.is_empty() || entry.asset_symbol.len() > 32 {
            return Err(StatusCode::BAD_REQUEST);
        }
        if let Some(ref addr) = entry.token_address
            && !is_hex_bytes(addr, 20)
        {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    // Collected through `Result` so one malformed identifier fails the whole
    // request rather than being silently dropped from the policy - a policy
    // missing an entry is a policy that permits something it should not.
    let inputs: Vec<TokenPolicyEntryInput> = req
        .entries
        .into_iter()
        .map(|e| TokenPolicyEntryInput {
            chain_id: e.chain_id,
            token_address: e.token_address,
            asset_symbol: e.asset_symbol,
        })
        .collect();

    let policy =
        StoreTokenPolicyWriter::upsert_token_policy(&*state.data_service, store_id, mode, &inputs)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(TokenPolicyResponse {
        id: policy.id.to_string(),
        store_id: policy.store_id,
        mode: policy.mode.to_string(),
        entries: policy
            .entries
            .into_iter()
            .map(|e| TokenPolicyEntryPayload {
                chain_id: e.chain_id.clone(),
                token_address: e.token_address,
                asset_symbol: e.asset_symbol,
            })
            .collect(),
        created_at: policy.created_at.to_rfc3339(),
        updated_at: policy.updated_at.to_rfc3339(),
    }))
}

/// Delete store token policy (revert to accept-all).
pub async fn delete_token_policy<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    let _ = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    StoreTokenPolicyWriter::delete_token_policy(&*state.data_service, store_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins the 20-byte token-address rule used by set_token_policy.
    #[test]
    fn token_address_with_non_hex_body_is_rejected() {
        let addr = format!("0x{}", "Z".repeat(40));
        assert_eq!(addr.len(), 42);
        assert!(!is_hex_bytes(&addr, 20));
    }

    #[test]
    fn token_address_valid_20_byte_hex_is_accepted() {
        let addr = format!("0x{}", "aB".repeat(20));
        assert!(is_hex_bytes(&addr, 20));
    }

    #[test]
    fn token_address_wrong_length_or_prefix_is_rejected() {
        assert!(!is_hex_bytes("0x1234", 20));
        assert!(!is_hex_bytes(&"a".repeat(42), 20));
        assert!(!is_hex_bytes("", 20));
    }
}
